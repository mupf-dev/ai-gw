// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Löschen und sofortige Neuanlage unter demselben Namen – Ende zu Ende über Admin-API und
//! Datenebene (Verdacht aus dem UI-e2e: neu angelegter Key liefert 401).
//!
//! Echter Server mit Admin-API, `SnapshotAuthenticator`, `SnapshotRoutes` (Mock-Deployments)
//! und laufendem Reloader mit dem Debounce des Betriebs (100 ms, inkrementeller Neubau).
//! Varianten: gleicher Scope, andere Organisation/Team, ohne Team, mehrfach innerhalb eines
//! Debounce-Fensters, Rotation, Modellgruppe samt Deployment, Team, Organisation – sowie das
//! wahrscheinliche Testartefakt (Löschen ohne `If-Match` → 428, Neuanlage → 409 ohne Klartext).
//!
//! Läuft gegen SQLite (In-Memory und Datei) und – mit `AIGW_TEST_POSTGRES_URL` bzw.
//! `scripts/check.sh` mit `AIGW_TEST_POSTGRES=docker` – gegen Postgres.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use aigw_admin::{AdminState, BootstrapToken};
use aigw_core::resource::new_id;
use aigw_core::snapshot::SnapshotHandle;
use aigw_proxy::{ProxyState, SnapshotAuthenticator, SnapshotRoutes};
use aigw_store::{
    Keyring, MasterKey, ReloadMetrics, ReloadOptions, Reloader, ReloaderTask, Store, load_snapshot,
};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};
use sqlx::AssertSqlSafe;

struct Env {
    addr: SocketAddr,
    token: String,
    store: Store,
    keyring: Arc<Keyring>,
    snapshot: SnapshotHandle,
    metrics: Arc<ReloadMetrics>,
    reloader: ReloaderTask,
    _routes: aigw_proxy::RoutesTask,
}

struct Resp {
    status: StatusCode,
    body: Bytes,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| {
            panic!(
                "kein JSON ({e}), Status {}: {}",
                self.status,
                String::from_utf8_lossy(&self.body)
            )
        })
    }

    #[track_caller]
    fn expect(self, status: StatusCode) -> Self {
        assert_eq!(
            self.status,
            status,
            "Antwort: {}",
            String::from_utf8_lossy(&self.body)
        );
        self
    }
}

async fn send(
    addr: SocketAddr,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> Resp {
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let mut req = Request::builder()
        .method(method.clone())
        .uri(format!("http://{addr}{path}"));
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    // F38: Schreibzugriffe ohne eigenen ETag mit `If-Match: *` (außer mit `x-test`).
    let explicit = headers.iter().any(|(k, _)| *k == "x-test");
    if !explicit && (matches!(method, Method::PUT | Method::DELETE) || path.ends_with("/rotate")) {
        req = req.header("if-match", "*");
    }
    let body = match body {
        Some(b) => {
            req = req.header("content-type", "application/json");
            Bytes::from(serde_json::to_vec(&b).unwrap())
        }
        None => Bytes::new(),
    };
    let res = client
        .request(req.body(Full::new(body)).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    Resp { status, body }
}

impl Env {
    async fn start(store: Store) -> Self {
        store.migrate().await.unwrap();
        let keyring = Arc::new(Keyring::new(MasterKey::generate().unwrap().0));
        // Wie im Betrieb: Debounce 100 ms, inkrementeller Neubau; keine Absicherungs-Abfrage
        // und keine Konsistenzprüfung (die würden einen Fehler überdecken).
        let reloader = Reloader::bootstrap(
            store.clone(),
            Arc::clone(&keyring),
            ReloadOptions {
                poll_interval: None,
                verify_interval: None,
                ..ReloadOptions::default()
            },
        )
        .await
        .unwrap();
        let snapshot = reloader.handle().clone();
        let metrics = reloader.metrics();
        let routes = Arc::new(SnapshotRoutes::with_defaults().unwrap());
        let routes_task = routes.spawn(snapshot.clone());
        let proxy = ProxyState {
            routes,
            auth: Arc::new(SnapshotAuthenticator::new(snapshot.clone())),
            ..ProxyState::mock()
        };
        let (token, secret) = BootstrapToken::generate().unwrap();
        let admin = aigw_admin::router_with(
            AdminState::new(store.clone(), Arc::new(token)).with_keyring(Arc::clone(&keyring)),
        );
        let app = aigw_proxy::router(proxy).merge(admin);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            addr,
            token: secret,
            store,
            keyring,
            snapshot,
            metrics,
            reloader: reloader.spawn(),
            _routes: routes_task,
        }
    }

    async fn admin(&self, method: Method, path: &str, body: Option<Value>) -> Resp {
        self.admin_with(method, path, &[], body).await
    }

    async fn admin_with(
        &self,
        method: Method,
        path: &str,
        extra: &[(&str, &str)],
        body: Option<Value>,
    ) -> Resp {
        let auth = format!("Bearer {}", self.token);
        let mut headers = vec![("authorization", auth.as_str())];
        headers.extend_from_slice(extra);
        send(
            self.addr,
            method,
            &format!("/admin/api/v1{path}"),
            &headers,
            body,
        )
        .await
    }

    async fn post(&self, path: &str, body: Value) -> Value {
        self.admin(Method::POST, path, Some(body))
            .await
            .expect(StatusCode::CREATED)
            .json()
    }

    async fn delete(&self, path: &str) {
        self.admin(Method::DELETE, path, None)
            .await
            .expect(StatusCode::NO_CONTENT);
    }

    /// Legt einen Key an und liefert den Klartext.
    async fn key(&self, name: &str, org: &str, team: Option<&str>) -> String {
        let mut spec = json!({"organization": org});
        if let Some(t) = team {
            spec["team"] = json!(t);
        }
        let v = self
            .post(
                "/api-keys",
                json!({"metadata": {"name": name}, "spec": spec}),
            )
            .await;
        v["secret"].as_str().unwrap().to_owned()
    }

    async fn chat(&self, key: &str, model: &str) -> Resp {
        let auth = format!("Bearer {key}");
        send(
            self.addr,
            Method::POST,
            "/v1/chat/completions",
            &[("authorization", &auth)],
            Some(json!({"model": model, "messages": [{"role": "user", "content": "hi"}]})),
        )
        .await
    }

    /// Wartet, bis die Datenebene (nach dem Hot Reload) mit `want` antwortet.
    async fn wait_chat(&self, key: &str, model: &str, want: StatusCode, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let r = self.chat(key, model).await;
            if r.status == want {
                return;
            }
            if Instant::now() >= deadline {
                let snap = self.snapshot.load();
                panic!(
                    "{what}: Datenebene liefert {} statt {want} ({model}): {}\n\
                     Snapshot v{} (DB v{}), Keys: {:?}",
                    r.status,
                    String::from_utf8_lossy(&r.body),
                    snap.version,
                    self.store.config_version().await.unwrap(),
                    snap.keys
                        .values()
                        .map(|k| k.key.metadata.name.clone())
                        .collect::<Vec<_>>(),
                );
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn ok(&self, key: &str, what: &str) {
        self.wait_chat(key, "echo", StatusCode::OK, what).await;
    }

    async fn denied(&self, key: &str, what: &str) {
        self.wait_chat(key, "echo", StatusCode::UNAUTHORIZED, what)
            .await;
    }
}

async fn scenario(store: Store) {
    let env = Env::start(store).await;
    for org in ["acme", "beta"] {
        env.post(
            "/organizations",
            json!({"metadata": {"name": org}, "spec": {}}),
        )
        .await;
    }
    for (team, org) in [("platform", "acme"), ("ops", "beta")] {
        env.post(
            "/teams",
            json!({"metadata": {"name": team}, "spec": {"organization": org}}),
        )
        .await;
    }
    let group = || json!({"metadata": {"name": "echo"}, "spec": {"routing": "weighted"}});
    let deployment = || {
        json!({"metadata": {"name": "echo-mock"},
               "spec": {"modelGroup": "echo", "provider": "mock", "providerModel": "mock"}})
    };
    env.post("/model-groups", group()).await;
    env.post("/deployments", deployment()).await;

    // 1. Key löschen und sofort unter demselben Namen (gleicher Scope) neu anlegen.
    let k1 = env.key("ci", "acme", Some("platform")).await;
    env.ok(&k1, "erster Key").await;
    env.delete("/api-keys/ci").await;
    let k2 = env.key("ci", "acme", Some("platform")).await;
    env.ok(&k2, "gelöscht + sofort gleichnamig neu").await;
    env.denied(&k1, "gelöschter Key").await;

    // 2. Gleicher Name in anderer Organisation/Team bzw. ohne Team.
    env.delete("/api-keys/ci").await;
    let k3 = env.key("ci", "beta", Some("ops")).await;
    env.ok(&k3, "gleichnamig in anderer Org/Team").await;
    env.denied(&k2, "Key aus Schritt 1").await;
    env.delete("/api-keys/ci").await;
    let k4 = env.key("ci", "acme", None).await;
    env.ok(&k4, "gleichnamig ohne Team").await;
    env.denied(&k3, "Key aus Schritt 2").await;

    // 3. Mehrfach löschen + neu ohne Pause (alles innerhalb eines Debounce-Fensters).
    let mut last = k4.clone();
    let mut old = Vec::new();
    for _ in 0..5 {
        env.delete("/api-keys/ci").await;
        old.push(last);
        last = env.key("ci", "acme", Some("platform")).await;
    }
    env.ok(&last, "nach 5× löschen + neu im Debounce-Fenster")
        .await;
    for k in &old {
        env.denied(k, "Zwischenstand").await;
    }

    // 4. Rotation: alter und neuer Key gelten; dann beide Namen löschen und neu anlegen.
    let rot = env
        .admin(
            Method::POST,
            "/api-keys/ci/rotate",
            Some(json!({"name": "ci-next", "gracePeriodSeconds": 3600})),
        )
        .await
        .expect(StatusCode::CREATED)
        .json();
    let next = rot["newKey"]["secret"].as_str().unwrap().to_owned();
    env.ok(&next, "rotierter Key").await;
    env.ok(&last, "alter Key in der Übergangszeit").await;
    env.delete("/api-keys/ci").await;
    let k5 = env.key("ci", "acme", Some("platform")).await;
    env.delete("/api-keys/ci-next").await;
    let k6 = env.key("ci-next", "beta", None).await;
    env.ok(&k5, "alter Name nach Rotation neu").await;
    env.ok(&k6, "rotierter Name neu").await;
    env.denied(&last, "rotierter alter Key gelöscht").await;
    env.denied(&next, "rotierter neuer Key gelöscht").await;

    // 5. Modellgruppe samt Deployment löschen und gleichnamig neu anlegen.
    env.delete("/deployments/echo-mock").await;
    env.delete("/model-groups/echo").await;
    env.post("/model-groups", group()).await;
    env.post("/deployments", deployment()).await;
    env.ok(&k5, "Modellgruppe gleichnamig neu").await;

    // 6. Team löschen und gleichnamig in anderer Organisation neu; Key gleichnamig neu.
    env.delete("/api-keys/ci").await;
    env.delete("/teams/platform").await;
    env.post(
        "/teams",
        json!({"metadata": {"name": "platform"}, "spec": {"organization": "beta"}}),
    )
    .await;
    let k7 = env.key("ci", "beta", Some("platform")).await;
    env.ok(&k7, "Team gleichnamig in anderer Org neu").await;
    env.denied(&k5, "Key des gelöschten Teams").await;

    // 7. Organisation löschen und gleichnamig neu.
    env.delete("/api-keys/ci").await;
    env.delete("/api-keys/ci-next").await;
    env.delete("/teams/platform").await;
    env.delete("/teams/ops").await;
    env.delete("/organizations/beta").await;
    env.post(
        "/organizations",
        json!({"metadata": {"name": "beta"}, "spec": {}}),
    )
    .await;
    let k8 = env.key("ci", "beta", None).await;
    env.ok(&k8, "Organisation gleichnamig neu").await;
    env.denied(&k7, "Key der gelöschten Org").await;
    env.denied(&k6, "Key der gelöschten Org").await;

    // 8. Wahrscheinliches Testartefakt im UI-e2e: „Löschen“ ohne `If-Match` wird mit 428
    //    abgelehnt (F38) – der Key bleibt. Die gleichnamige Neuanlage endet dann mit 409 ohne
    //    Klartext, und der Test schickt einen leeren/undefinierten Key → 401. Das ist korrektes
    //    Verhalten, kein Snapshot-Fehler.
    let r = env
        .admin_with(
            Method::DELETE,
            "/api-keys/ci",
            &[("x-test", "ohne-if-match")],
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::PRECONDITION_REQUIRED);
    let r = env
        .admin(
            Method::POST,
            "/api-keys",
            Some(json!({"metadata": {"name": "ci"}, "spec": {"organization": "beta"}})),
        )
        .await
        .expect(StatusCode::CONFLICT);
    assert!(r.json().get("secret").is_none(), "409 ohne Klartext");
    env.ok(&k8, "Key bleibt nach abgelehntem Löschen gültig")
        .await;
    env.denied("undefined", "fehlender Klartext").await;

    // Inkrementell ohne Rückfall, und der Stand gleicht dem vollständigen Neubau.
    assert_eq!(env.metrics.incremental_fallbacks.load(Ordering::Relaxed), 0);
    assert!(env.metrics.reloads_incremental.load(Ordering::Relaxed) > 0);
    let full = load_snapshot(&env.store, &env.keyring).await.unwrap();
    assert_eq!(*env.snapshot.load(), full);
    env.reloader.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_in_memory() {
    scenario(Store::connect("sqlite::memory:").await.unwrap()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_file() {
    let dir = std::env::temp_dir().join(format!("aigw-recreate-test-{}", new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let url = format!("sqlite://{}/aigw.db?mode=rwc", dir.display());
    scenario(Store::connect(&url).await.unwrap()).await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres() {
    let Ok(base) = std::env::var("AIGW_TEST_POSTGRES_URL") else {
        eprintln!("AIGW_TEST_POSTGRES_URL nicht gesetzt – Postgres-Test übersprungen");
        return;
    };
    let db = format!("aigw_recreate_test_{}", new_id().simple());
    let admin = sqlx::PgPool::connect(&base).await.unwrap();
    sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {db}")))
        .execute(&admin)
        .await
        .unwrap();
    let (prefix, _) = base.rsplit_once('/').expect("URL mit Datenbankpfad");
    let store = Store::connect(&format!("{prefix}/{db}")).await.unwrap();
    scenario(store.clone()).await;
    store.close().await;
    sqlx::query(AssertSqlSafe(format!("DROP DATABASE {db} WITH (FORCE)")))
        .execute(&admin)
        .await
        .unwrap();
}
