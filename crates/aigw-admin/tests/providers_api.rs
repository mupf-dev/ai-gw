// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Integrationstests AIGW-84: Zugangsdaten mit Entra-ID-Anmeldung (`spec.entra`), Deployments
//! mit `credential` statt Inline-Einstellungen, Validierung, Snapshot, Audit und die
//! Anbieter-Übersicht `GET /admin/api/v1/providers` mit Laufzeitzustand aus dem Routing.
//!
//! Läuft gegen SQLite und – mit `AIGW_TEST_POSTGRES_URL` bzw. `scripts/check.sh` mit
//! `AIGW_TEST_POSTGRES=docker` – gegen Postgres.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aigw_admin::{AdminState, BootstrapToken, UpstreamPolicy};
use aigw_core::resource::{EntraCredentialKind, new_id};
use aigw_core::snapshot::{Snapshot, SnapshotHandle};
use aigw_proxy::{RoutesTask, SnapshotRoutes};
use aigw_store::{Keyring, MasterKey, ReloadOptions, Reloader, ReloaderTask, Store};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};
use sqlx::AssertSqlSafe;

const SECRET: &str = "sp-geheimnis-aigw84-0001";
const UAMI: &str = "11111111-2222-3333-4444-555555555555";

struct Env {
    addr: SocketAddr,
    token: String,
    store: Store,
    snapshot: SnapshotHandle,
    reloader: ReloaderTask,
    _routes: RoutesTask,
}

struct Resp {
    status: StatusCode,
    body: Bytes,
}

impl Resp {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("kein JSON ({e}), {}: {}", self.status, self.text()))
    }

    #[track_caller]
    fn expect(self, status: StatusCode) -> Self {
        assert_eq!(self.status, status, "Antwort: {}", self.text());
        self
    }

    #[track_caller]
    fn detail(&self, status: StatusCode) -> String {
        assert_eq!(self.status, status, "{}", self.text());
        self.json()["detail"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }
}

impl Env {
    async fn start(store: Store) -> Self {
        store.migrate().await.unwrap();
        let keyring = Arc::new(Keyring::new(MasterKey::generate().unwrap().0));
        let reloader = Reloader::bootstrap(
            store.clone(),
            Arc::clone(&keyring),
            ReloadOptions {
                poll_interval: None,
                debounce: Duration::from_millis(10),
                ..ReloadOptions::default()
            },
        )
        .await
        .unwrap();
        let snapshot = reloader.handle().clone();
        let policy = UpstreamPolicy::strict();
        let routes = Arc::new(SnapshotRoutes::with_policy(policy.clone()).unwrap());
        let routes_task = routes.spawn(snapshot.clone());
        let (token, secret) = BootstrapToken::generate().unwrap();
        let token = Arc::new(token);
        let admin = AdminState::new(store.clone(), token)
            .with_keyring(Arc::clone(&keyring))
            .with_upstream_policy(policy);
        let auth = Arc::clone(&admin.auth);
        let app = aigw_admin::router_with(admin).merge(aigw_admin::providers::router(
            aigw_admin::providers::ProvidersState {
                store: store.clone(),
                auth,
                routes: Some(routes),
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            addr,
            token: secret,
            store,
            snapshot,
            reloader: reloader.spawn(),
            _routes: routes_task,
        }
    }

    async fn admin(&self, method: Method, path: &str, body: Option<Value>) -> Resp {
        let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
        let mut req = Request::builder()
            .method(method.clone())
            .uri(format!("http://{}/admin/api/v1{path}", self.addr))
            .header("authorization", format!("Bearer {}", self.token));
        if matches!(method, Method::PUT | Method::PATCH | Method::DELETE) {
            req = req.header("if-match", "*");
        }
        let body = match body {
            Some(b) => {
                let ct = if method == Method::PATCH {
                    "application/merge-patch+json"
                } else {
                    "application/json"
                };
                req = req.header("content-type", ct);
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
        let r = Resp { status, body };
        assert!(
            !r.text().contains(SECRET),
            "Geheimnis in Ausgabe: {}",
            r.text()
        );
        r
    }

    async fn wait_snapshot(&self, what: &str, cond: impl Fn(&Snapshot) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !cond(&self.snapshot.load()) {
            assert!(Instant::now() < deadline, "Snapshot erfüllt nicht: {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Wartet, bis die Übersicht für alle aktiven Deployments einen Laufzeitzustand hat.
    async fn overview(&self) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let v = self
                .admin(Method::GET, "/providers", None)
                .await
                .expect(StatusCode::OK)
                .json();
            let unknown = v["items"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|i| i["deployments"].as_array().unwrap())
                .any(|d| d["state"] == "unknown");
            if !unknown {
                return v;
            }
            assert!(Instant::now() < deadline, "Laufzeitzustand fehlt: {v}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn item<'a>(v: &'a Value, provider: &str, endpoint: Option<&str>) -> &'a Value {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["provider"] == provider && i["endpoint"].as_str() == endpoint)
        .unwrap_or_else(|| panic!("kein Eintrag {provider} {endpoint:?}: {v}"))
}

async fn scenario(store: Store) {
    let env = Env::start(store).await;
    let post = |path: &'static str, body: Value| env.admin(Method::POST, path, Some(body));

    // --- Zugangsdaten mit spec.entra --------------------------------------------------------
    // Managed Identity: keine Werte nötig.
    let r = post(
        "/provider-credentials",
        json!({"metadata": {"name": "foundry-mi"},
               "spec": {"provider": "anthropic",
                        "entra": {"credential": "managedIdentity", "clientId": UAMI}}}),
    )
    .await
    .expect(StatusCode::CREATED)
    .json();
    assert_eq!(r["spec"]["entra"]["credential"], "managedIdentity");
    assert_eq!(r["spec"]["entra"]["clientId"], UAMI);
    assert_eq!(r["spec"]["fields"], json!([]));
    assert_eq!(r["status"]["hasSecret"], false);
    // Service Principal: Secret nur in data (verschlüsselt), IDs im Klartext.
    let r = post(
        "/provider-credentials",
        json!({"metadata": {"name": "aoai-sp"},
               "spec": {"provider": "azure-openai",
                        "entra": {"credential": "clientSecret", "tenantId": "contoso.onmicrosoft.com",
                                  "clientId": UAMI},
                        "data": {"clientSecret": {"value": SECRET}}}}),
    )
    .await
    .expect(StatusCode::CREATED)
    .json();
    assert_eq!(r["spec"]["fields"], json!(["clientSecret"]));
    for (body, want) in [
        // Nur Azure OpenAI und Anthropic.
        (
            json!({"provider": "openai", "entra": {"credential": "managedIdentity"}}),
            "spec.entra",
        ),
        // Service Principal ohne Secret.
        (
            json!({"provider": "anthropic",
                   "entra": {"credential": "clientSecret", "tenantId": "t", "clientId": "c"}}),
            "clientSecret",
        ),
        // Managed Identity mit gespeichertem Secret.
        (
            json!({"provider": "anthropic", "entra": {"credential": "managedIdentity"},
                   "data": {"clientSecret": {"value": SECRET}}}),
            "clientSecret",
        ),
        // Scope bei Managed Identity nur Cognitive Services.
        (
            json!({"provider": "anthropic",
                   "entra": {"credential": "managedIdentity",
                             "scope": "https://management.azure.com/.default"}}),
            "spec.entra.scope",
        ),
        // Unbekanntes Feld.
        (
            json!({"provider": "anthropic", "entra": {"credential": "managedIdentity", "x": 1}}),
            "",
        ),
    ] {
        let r = post(
            "/provider-credentials",
            json!({"metadata": {"name": "bad"}, "spec": body}),
        )
        .await;
        assert!(
            r.status == StatusCode::UNPROCESSABLE_ENTITY || r.status == StatusCode::BAD_REQUEST,
            "{body}: {}",
            r.text()
        );
        assert!(r.text().contains(want), "{want}: {}", r.text());
    }

    // --- Deployments ---------------------------------------------------------------------
    for g in ["claude", "gpt", "gpt-inline", "openai"] {
        post(
            "/model-groups",
            json!({"metadata": {"name": g}, "spec": {}}),
        )
        .await
        .expect(StatusCode::CREATED);
    }
    let foundry = "https://acme.services.ai.azure.com/anthropic";
    post(
        "/deployments",
        json!({"metadata": {"name": "claude-foundry"},
               "spec": {"modelGroup": "claude", "provider": "anthropic", "endpoint": foundry,
                        "providerModel": "claude-sonnet-5-5", "credential": "foundry-mi"}}),
    )
    .await
    .expect(StatusCode::CREATED);
    // Entra-Token nie an api.anthropic.com: endpoint Pflicht, auch über die Zugangsdaten.
    let d = post(
        "/deployments",
        json!({"metadata": {"name": "claude-default"},
               "spec": {"modelGroup": "claude", "provider": "anthropic",
                        "providerModel": "claude-sonnet-5-5", "credential": "foundry-mi"}}),
    )
    .await
    .detail(StatusCode::UNPROCESSABLE_ENTITY);
    assert!(d.contains("endpoint"), "{d}");
    // settings.auth apiKey widerspricht Zugangsdaten mit Entra ID.
    let d = post(
        "/deployments",
        json!({"metadata": {"name": "claude-key"},
               "spec": {"modelGroup": "claude", "provider": "anthropic", "endpoint": foundry,
                        "providerModel": "m", "credential": "foundry-mi",
                        "settings": {"auth": "apiKey"}}}),
    )
    .await
    .detail(StatusCode::UNPROCESSABLE_ENTITY);
    assert!(d.contains("settings.auth"), "{d}");
    post(
        "/deployments",
        json!({"metadata": {"name": "gpt-sp"},
               "spec": {"modelGroup": "gpt", "provider": "azure-openai",
                        "endpoint": "https://acme.openai.azure.com", "providerModel": "gpt-5",
                        "credential": "aoai-sp", "settings": {"apiVersion": "2025-04-01-preview"}}}),
    )
    .await
    .expect(StatusCode::CREATED);
    // Rückwärtskompatibel: Inline-Einstellungen ohne Zugangsdaten.
    post(
        "/deployments",
        json!({"metadata": {"name": "gpt-inline"},
               "spec": {"modelGroup": "gpt-inline", "provider": "azure-openai",
                        "endpoint": "https://acme.openai.azure.com/", "providerModel": "gpt-5",
                        "settings": {"apiVersion": "2025-04-01-preview", "auth": "entraId",
                                     "entraCredential": "managedIdentity", "entraClientId": UAMI}}}),
    )
    .await
    .expect(StatusCode::CREATED);
    // Fehlkonfiguriert (fehlendes apiKey-Feld) → in der Übersicht „unusable“ mit Grund.
    post(
        "/provider-credentials",
        json!({"metadata": {"name": "oa"},
               "spec": {"provider": "openai", "data": {"organization": {"value": "o"},
                                                      "apiKey": {"value": "sk-x"}}}}),
    )
    .await
    .expect(StatusCode::CREATED);
    post(
        "/deployments",
        json!({"metadata": {"name": "oa-off"},
               "spec": {"modelGroup": "openai", "provider": "openai", "providerModel": "gpt-4o",
                        "credential": "oa", "enabled": false}}),
    )
    .await
    .expect(StatusCode::CREATED);

    env.wait_snapshot("Entra an Zugangsdaten", |s| {
        s.credentials
            .get("foundry-mi")
            .and_then(|c| c.spec.entra.as_ref())
            .is_some_and(|e| e.credential == EntraCredentialKind::ManagedIdentity)
            && s.model_group("gpt-inline").is_some()
    })
    .await;

    // --- Übersicht -----------------------------------------------------------------------
    let v = env.overview().await;
    assert_eq!(v["runtime"], true);
    let claude = item(&v, "anthropic", Some(foundry));
    assert_eq!(
        claude["auth"],
        json!([{"kind": "entraId", "entraCredential": "managedIdentity", "clientId": UAMI,
                "credential": "foundry-mi"}])
    );
    assert_eq!(claude["modelGroups"], json!(["claude"]));
    assert_eq!(claude["state"], "ok", "{claude}");
    assert_eq!(claude["deployments"][0]["breaker"], "closed");
    let aoai = item(&v, "azure-openai", Some("https://acme.openai.azure.com"));
    // Zwei Deployments (Endpunkt mit und ohne „/“), zwei Anmeldungen.
    assert_eq!(aoai["deployments"].as_array().unwrap().len(), 2, "{aoai}");
    let kinds: Vec<(&str, Option<&str>)> = aoai["auth"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            (
                a["entraCredential"].as_str().unwrap(),
                a["credential"].as_str(),
            )
        })
        .collect();
    assert_eq!(
        kinds,
        vec![("clientSecret", Some("aoai-sp")), ("managedIdentity", None)]
    );
    let oa = item(&v, "openai", None);
    assert_eq!(oa["state"], "disabled");
    assert_eq!(oa["auth"][0]["kind"], "apiKey");

    // --- Ändern ---------------------------------------------------------------------------
    // Entra entfernen: Anthropic bräuchte dann einen API-Key.
    env.admin(
        Method::PATCH,
        "/provider-credentials/foundry-mi",
        Some(json!({"spec": {"entra": null}})),
    )
    .await
    .detail(StatusCode::UNPROCESSABLE_ENTITY);
    // Anbieterwechsel ließe claude-foundry ungültig zurück.
    env.admin(
        Method::PUT,
        "/provider-credentials/foundry-mi",
        Some(json!({"metadata": {"name": "foundry-mi"},
                    "spec": {"provider": "azure-openai",
                             "entra": {"credential": "managedIdentity"}}})),
    )
    .await
    .detail(StatusCode::CONFLICT);
    // Andere Identität: per PATCH als Ganzes ersetzen.
    let other = "22222222-3333-4444-5555-666666666666";
    let r = env
        .admin(
            Method::PATCH,
            "/provider-credentials/foundry-mi",
            Some(json!({"spec": {"entra": {"credential": "managedIdentity", "clientId": other}}})),
        )
        .await
        .expect(StatusCode::OK)
        .json();
    assert_eq!(r["spec"]["entra"]["clientId"], other);
    // Labels ändern lässt entra unverändert.
    let r = env
        .admin(
            Method::PATCH,
            "/provider-credentials/foundry-mi",
            Some(json!({"metadata": {"labels": {"env": "prod"}}})),
        )
        .await
        .expect(StatusCode::OK)
        .json();
    assert_eq!(r["spec"]["entra"]["clientId"], other);
    env.wait_snapshot("neue Client-ID", |s| {
        s.credentials["foundry-mi"]
            .spec
            .entra
            .as_ref()
            .is_some_and(|e| e.client_id.as_deref() == Some(other))
    })
    .await;
    let v = env.overview().await;
    assert_eq!(
        item(&v, "anthropic", Some(foundry))["auth"][0]["clientId"],
        other
    );

    // --- Audit je Objekt (Journal des inkrementellen Snapshots) ---------------------------
    let entries = env.store.audit_log(10_000).await.unwrap();
    let cred_updates: Vec<_> = entries
        .iter()
        .filter(|e| {
            e.kind == "provider-credential"
                && e.object_name.as_deref() == Some("foundry-mi")
                && e.object_id.is_some()
        })
        .collect();
    assert!(cred_updates.len() >= 3, "{cred_updates:?}");
    let text = serde_json::to_string(&entries).unwrap();
    assert!(text.contains("managedIdentity"));
    assert!(!text.contains(SECRET), "Geheimnis im Audit-Log");
    env.reloader.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_in_memory() {
    scenario(Store::connect("sqlite::memory:").await.unwrap()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres() {
    let Ok(base) = std::env::var("AIGW_TEST_POSTGRES_URL") else {
        eprintln!("AIGW_TEST_POSTGRES_URL nicht gesetzt – Postgres-Test übersprungen");
        return;
    };
    let db = format!("aigw_providers_test_{}", new_id().simple());
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
