// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Integrationstests Admin-API ↔ Datenebene (AIGW-28) mit echtem Server und echter Datenbank:
//! Admin legt Organisation/Team/Key an → Key wirkt nach dem Hot Reload auf der Datenebene →
//! Sperren → 401; Modellfreigaben, Ablauf, Rotation, ETag-Konflikte, Paginierung.
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
use std::time::{Duration, Instant};

use aigw_admin::{AdminState, BootstrapToken};
use aigw_core::resource::{ModelGroupSpec, ObjectMeta, new_id};
use aigw_proxy::{ProxyState, SnapshotAuthenticator};
use aigw_store::{Keyring, MasterKey, ReloadOptions, Reloader, ReloaderTask, Store};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{HeaderMap, Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};
use sqlx::AssertSqlSafe;

struct Env {
    addr: SocketAddr,
    token: String,
    reloader: ReloaderTask,
}

struct Resp {
    status: StatusCode,
    headers: HeaderMap,
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

    fn header(&self, name: &str) -> String {
        self.headers
            .get(name)
            .unwrap_or_else(|| panic!("Header {name} fehlt"))
            .to_str()
            .unwrap()
            .to_owned()
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

    #[track_caller]
    fn problem(&self, status: StatusCode, code: &str) {
        assert_eq!(
            self.status,
            status,
            "{}",
            String::from_utf8_lossy(&self.body)
        );
        assert_eq!(self.header("content-type"), "application/problem+json");
        let v = self.json();
        assert_eq!(v["status"], status.as_u16());
        assert_eq!(v["code"], code);
        assert_eq!(v["type"], format!("urn:aigw:problem:{code}"));
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
        .method(method)
        .uri(format!("http://{addr}{path}"));
    for (k, v) in headers {
        req = req.header(*k, *v);
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
    let headers = res.headers().clone();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    Resp {
        status,
        headers,
        body,
    }
}

impl Env {
    async fn start(store: Store) -> Self {
        store.migrate().await.unwrap();
        // Modellgruppen legt die Admin-API (noch) nicht an – direkt über den Store.
        for (name, aliases) in [
            ("gpt-4o", vec!["gpt-4o-latest".to_owned()]),
            ("claude", vec![]),
        ] {
            store
                .create(
                    "test",
                    ObjectMeta::named(name),
                    ModelGroupSpec {
                        aliases,
                        ..ModelGroupSpec::default()
                    },
                )
                .await
                .unwrap();
        }
        let keyring = Arc::new(Keyring::new(MasterKey::generate().unwrap().0));
        let reloader = Reloader::bootstrap(
            store.clone(),
            keyring,
            ReloadOptions {
                poll_interval: None,
                debounce: Duration::from_millis(10),
                ..ReloadOptions::default()
            },
        )
        .await
        .unwrap();
        let proxy = ProxyState {
            auth: Arc::new(SnapshotAuthenticator::new(reloader.handle().clone())),
            ..ProxyState::mock()
        };
        let (token, secret) = BootstrapToken::generate().unwrap();
        let admin = aigw_admin::router_with(AdminState::new(store, Arc::new(token)));
        let app = aigw_proxy::router(proxy).merge(admin);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            addr,
            token: secret,
            reloader: reloader.spawn(),
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

    /// OpenAI-Chat mit `Authorization: Bearer`.
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

    /// Anthropic-Messages mit `x-api-key`.
    async fn messages(&self, key: &str, model: &str) -> Resp {
        send(
            self.addr,
            Method::POST,
            "/v1/messages",
            &[("x-api-key", key), ("anthropic-version", "2023-06-01")],
            Some(json!({"model": model, "max_tokens": 5,
                        "messages": [{"role": "user", "content": "hi"}]})),
        )
        .await
    }

    /// Wartet, bis die Datenebene (nach dem Hot Reload) mit `want` antwortet.
    async fn wait_chat(&self, key: &str, model: &str, want: StatusCode) -> Resp {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let r = self.chat(key, model).await;
            if r.status == want {
                return r;
            }
            assert!(
                Instant::now() < deadline,
                "Datenebene liefert {} statt {want} ({model}): {}",
                r.status,
                String::from_utf8_lossy(&r.body)
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn models(&self, key: &str) -> Vec<String> {
        let auth = format!("Bearer {key}");
        let r = send(
            self.addr,
            Method::GET,
            "/v1/models",
            &[("authorization", &auth)],
            None,
        )
        .await
        .expect(StatusCode::OK);
        r.json()["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap().to_owned())
            .collect()
    }
}

async fn create_key(env: &Env, body: Value) -> (String, Value) {
    let r = env
        .admin(Method::POST, "/api-keys", Some(body))
        .await
        .expect(StatusCode::CREATED);
    assert_eq!(r.header("cache-control"), "no-store");
    let v = r.json();
    let secret = v["secret"].as_str().unwrap().to_owned();
    assert!(secret.starts_with("sk-aigw-"));
    (secret, v["key"].clone())
}

async fn scenario(store: Store) {
    let env = Env::start(store).await;

    // --- Anmeldung an der Admin-API ----------------------------------------------------------
    send(
        env.addr,
        Method::GET,
        "/admin/api/v1/organizations",
        &[],
        None,
    )
    .await
    .problem(StatusCode::UNAUTHORIZED, "unauthorized");
    let r = send(
        env.addr,
        Method::GET,
        "/admin/api/v1/organizations",
        &[("authorization", "Bearer falsch")],
        None,
    )
    .await;
    r.problem(StatusCode::UNAUTHORIZED, "unauthorized");
    assert!(r.header("www-authenticate").starts_with("Bearer"));
    // Version und OpenAPI bleiben ohne Anmeldung erreichbar.
    send(env.addr, Method::GET, "/admin/api/v1/version", &[], None)
        .await
        .expect(StatusCode::OK);

    // --- Organisation, Team, Nutzer ----------------------------------------------------------
    let r = env
        .admin(
            Method::POST,
            "/organizations",
            Some(
                json!({"metadata": {"name": "acme", "labels": {"env": "prod"}},
                        "spec": {"displayName": "ACME"}}),
            ),
        )
        .await
        .expect(StatusCode::CREATED);
    assert_eq!(r.header("location"), "/admin/api/v1/organizations/acme");
    assert!(r.header("etag").starts_with('"'));
    assert_eq!(r.json()["spec"]["displayName"], "ACME");
    env.admin(
        Method::POST,
        "/organizations",
        Some(json!({"metadata": {"name": "acme"}, "spec": {}})),
    )
    .await
    .problem(StatusCode::CONFLICT, "conflict");
    env.admin(
        Method::POST,
        "/organizations",
        Some(json!({"metadata": {"name": "Ungültig"}, "spec": {}})),
    )
    .await
    .problem(StatusCode::UNPROCESSABLE_ENTITY, "validation-failed");
    env.admin(
        Method::POST,
        "/organizations",
        Some(json!({"metadata": {"name": "x"}, "spec": {"unbekannt": 1}})),
    )
    .await
    .problem(StatusCode::UNPROCESSABLE_ENTITY, "validation-failed");

    env.admin(
        Method::POST,
        "/teams",
        Some(json!({"metadata": {"name": "platform"},
                    "spec": {"organization": "acme", "allowedModels": ["gpt-4o"]}})),
    )
    .await
    .expect(StatusCode::CREATED);
    env.admin(
        Method::POST,
        "/teams",
        Some(json!({"metadata": {"name": "t2"}, "spec": {"organization": "gibt-es-nicht"}})),
    )
    .await
    .problem(StatusCode::UNPROCESSABLE_ENTITY, "validation-failed");
    env.admin(
        Method::POST,
        "/teams",
        Some(json!({"metadata": {"name": "t3"},
                    "spec": {"organization": "acme", "allowedModels": ["gpt-5-gibts-nicht"]}})),
    )
    .await
    .problem(StatusCode::UNPROCESSABLE_ENTITY, "validation-failed");
    env.admin(
        Method::POST,
        "/users",
        Some(json!({"metadata": {"name": "alice"},
                    "spec": {"email": "Alice@example.org", "roles": [{"role": "viewer"}]}})),
    )
    .await
    .expect(StatusCode::CREATED);
    let users = env
        .admin(Method::GET, "/users?email=alice@example.org", None)
        .await
        .expect(StatusCode::OK)
        .json();
    assert_eq!(users["items"].as_array().unwrap().len(), 1);

    // --- Key anlegen → wirkt sofort (Hot Reload) ---------------------------------------------
    let (ci, ci_key) = create_key(
        &env,
        json!({"metadata": {"name": "ci"}, "spec": {"team": "platform", "user": "alice"}}),
    )
    .await;
    assert_eq!(ci_key["status"]["state"], "active");
    assert_eq!(ci_key["status"]["prefix"].as_str().unwrap().len(), 12);
    env.wait_chat(&ci, "gpt-4o", StatusCode::OK).await;
    // Alias der freigegebenen Gruppe, Anthropic-Eingang mit x-api-key.
    env.chat(&ci, "gpt-4o-latest").await.expect(StatusCode::OK);
    env.messages(&ci, "gpt-4o").await.expect(StatusCode::OK);
    // Team erlaubt nur gpt-4o.
    let r = env.chat(&ci, "claude").await.expect(StatusCode::FORBIDDEN);
    assert_eq!(r.json()["error"]["code"], "model_not_allowed");
    let r = env
        .messages(&ci, "claude")
        .await
        .expect(StatusCode::FORBIDDEN);
    assert_eq!(r.json()["error"]["type"], "permission_error");
    assert_eq!(env.models(&ci).await, ["gpt-4o"]);

    // Detail/Liste zeigen nie Klartext.
    let r = env
        .admin(Method::GET, "/api-keys/ci", None)
        .await
        .expect(StatusCode::OK);
    assert!(r.json().get("secret").is_none());
    assert!(!String::from_utf8_lossy(&r.body).contains(&ci));
    for (q, n) in [
        ("team=platform", 1),
        ("organization=acme", 1),
        ("user=alice", 1),
        ("state=revoked", 0),
        ("q=c", 1),
    ] {
        let v = env
            .admin(Method::GET, &format!("/api-keys?{q}"), None)
            .await
            .expect(StatusCode::OK)
            .json();
        assert_eq!(v["items"].as_array().unwrap().len(), n, "Filter {q}");
    }
    env.admin(Method::GET, "/api-keys?unbekannt=1", None)
        .await
        .problem(StatusCode::BAD_REQUEST, "bad-request");

    // --- Sperren → 401, Entsperren → 200 ------------------------------------------------------
    env.admin(Method::POST, "/api-keys/ci/revoke", None)
        .await
        .expect(StatusCode::OK);
    let r = env.wait_chat(&ci, "gpt-4o", StatusCode::UNAUTHORIZED).await;
    assert_eq!(r.json()["error"]["code"], "invalid_api_key");
    let r = env
        .messages(&ci, "gpt-4o")
        .await
        .expect(StatusCode::UNAUTHORIZED);
    assert_eq!(r.json()["type"], "error");
    assert_eq!(r.json()["error"]["type"], "authentication_error");
    // Gesperrt, unbekannt und fehlend sind nicht unterscheidbar.
    let unknown = env.chat("sk-aigw-gibtesnicht-xyz", "gpt-4o").await;
    assert_eq!(unknown.status, StatusCode::UNAUTHORIZED);
    assert_eq!(unknown.json()["error"], r_openai_error(&env, &ci).await);
    env.admin(Method::POST, "/api-keys/ci/reactivate", None)
        .await
        .expect(StatusCode::OK);
    env.wait_chat(&ci, "gpt-4o", StatusCode::OK).await;

    // --- Modellfreigaben auf Key-Ebene ohne Team ---------------------------------------------
    let (restricted, _) = create_key(
        &env,
        json!({"metadata": {"name": "restricted"}, "spec": {"allowedModels": ["claude"]}}),
    )
    .await;
    env.wait_chat(&restricted, "claude", StatusCode::OK).await;
    env.chat(&restricted, "gpt-4o")
        .await
        .expect(StatusCode::FORBIDDEN);
    // Eingeschränkt, aber präzise (F39): unbekanntes Modell = 404.
    let r = env
        .chat(&restricted, "nope")
        .await
        .expect(StatusCode::NOT_FOUND);
    assert_eq!(r.json()["error"]["code"], "model_not_found");
    assert_eq!(env.models(&restricted).await, ["claude"]);
    let (free, _) = create_key(&env, json!({"metadata": {"name": "free"}, "spec": {}})).await;
    env.wait_chat(&free, "claude", StatusCode::OK).await;
    let r = env.chat(&free, "nope").await.expect(StatusCode::NOT_FOUND);
    assert_eq!(r.json()["error"]["code"], "model_not_found");
    assert_eq!(env.models(&free).await, ["claude", "gpt-4o"]);
    env.admin(
        Method::POST,
        "/api-keys",
        Some(json!({"metadata": {"name": "bad"}, "spec": {"allowedModels": ["nope"]}})),
    )
    .await
    .problem(StatusCode::UNPROCESSABLE_ENTITY, "validation-failed");

    // --- Ablauf -------------------------------------------------------------------------------
    let r = env
        .admin(Method::GET, "/api-keys/free", None)
        .await
        .expect(StatusCode::OK);
    let etag = r.header("etag");
    env.admin_with(
        Method::PUT,
        "/api-keys/free",
        &[("if-match", &etag)],
        Some(json!({"metadata": {"name": "free"},
                    "spec": {"expiresAt": "2020-01-01T00:00:00Z"}})),
    )
    .await
    .expect(StatusCode::OK);
    env.wait_chat(&free, "claude", StatusCode::UNAUTHORIZED)
        .await;
    let (expired, _) = create_key(
        &env,
        json!({"metadata": {"name": "expired"}, "spec": {"expiresAt": "2020-01-01T00:00:00Z"}}),
    )
    .await;
    // Nach dem Neubau ist der Key im Snapshot, aber abgelaufen.
    let (probe, _) = create_key(&env, json!({"metadata": {"name": "probe"}, "spec": {}})).await;
    env.wait_chat(&probe, "claude", StatusCode::OK).await;
    env.chat(&expired, "claude")
        .await
        .expect(StatusCode::UNAUTHORIZED);

    // --- Rotation -----------------------------------------------------------------------------
    let r = env
        .admin(
            Method::POST,
            "/api-keys/restricted/rotate",
            Some(json!({"name": "restricted-2"})),
        )
        .await
        .expect(StatusCode::CREATED);
    assert_eq!(r.header("cache-control"), "no-store");
    let v = r.json();
    let restricted2 = v["newKey"]["secret"].as_str().unwrap().to_owned();
    assert_eq!(v["newKey"]["key"]["metadata"]["name"], "restricted-2");
    assert_eq!(
        v["newKey"]["key"]["spec"]["allowedModels"],
        json!(["claude"])
    );
    assert!(v["previousKey"]["spec"]["expiresAt"].is_string());
    env.wait_chat(&restricted2, "claude", StatusCode::OK).await;
    // Übergangszeit: der alte Key gilt weiter.
    env.chat(&restricted, "claude").await.expect(StatusCode::OK);

    // Ohne Übergangszeit und mit Standardnamen.
    let v = env
        .admin(
            Method::POST,
            "/api-keys/ci/rotate",
            Some(json!({"gracePeriodSeconds": 0})),
        )
        .await
        .expect(StatusCode::CREATED)
        .json();
    let ci2 = v["newKey"]["secret"].as_str().unwrap().to_owned();
    let ci2_name = v["newKey"]["key"]["metadata"]["name"].as_str().unwrap();
    assert!(ci2_name.starts_with("ci-") && ci2_name.len() == 3 + 14);
    assert_eq!(v["newKey"]["key"]["spec"]["team"], "platform");
    env.wait_chat(&ci2, "gpt-4o", StatusCode::OK).await;
    env.wait_chat(&ci, "gpt-4o", StatusCode::UNAUTHORIZED).await;
    env.admin(Method::POST, "/api-keys/ci/revoke", None)
        .await
        .expect(StatusCode::OK);
    env.admin(Method::POST, "/api-keys/ci/rotate", None)
        .await
        .problem(StatusCode::CONFLICT, "conflict");

    // --- ETag / If-Match -----------------------------------------------------------------------
    let r = env
        .admin(Method::GET, "/teams/platform", None)
        .await
        .expect(StatusCode::OK);
    let etag1 = r.header("etag");
    let mut team = r.json();
    team["spec"]["displayName"] = json!("Plattform");
    let body = json!({"metadata": {"name": "platform"}, "spec": team["spec"].clone()});
    let r = env
        .admin_with(
            Method::PUT,
            "/teams/platform",
            &[("if-match", &etag1)],
            Some(body.clone()),
        )
        .await
        .expect(StatusCode::OK);
    let etag2 = r.header("etag");
    assert_ne!(etag1, etag2);
    env.admin_with(
        Method::PUT,
        "/teams/platform",
        &[("if-match", &etag1)],
        Some(body.clone()),
    )
    .await
    .problem(StatusCode::PRECONDITION_FAILED, "precondition-failed");
    env.admin_with(
        Method::DELETE,
        "/teams/platform",
        &[("if-match", &etag1)],
        None,
    )
    .await
    .problem(StatusCode::PRECONDITION_FAILED, "precondition-failed");
    env.admin(
        Method::PUT,
        "/teams/platform",
        Some(json!({"metadata": {"name": "anders"}, "spec": team["spec"].clone()})),
    )
    .await
    .problem(StatusCode::UNPROCESSABLE_ENTITY, "validation-failed");
    // Team-Freigabe erweitern → Hot Reload → claude für den neuen CI-Key erlaubt.
    let mut spec = team["spec"].clone();
    spec["allowedModels"] = json!(["gpt-4o", "claude"]);
    env.admin_with(
        Method::PUT,
        "/teams/platform",
        &[("if-match", &etag2)],
        Some(json!({"metadata": {"name": "platform"}, "spec": spec})),
    )
    .await
    .expect(StatusCode::OK);
    env.wait_chat(&ci2, "claude", StatusCode::OK).await;

    // --- Paginierung --------------------------------------------------------------------------
    for i in 0..5 {
        env.admin(
            Method::POST,
            "/organizations",
            Some(json!({"metadata": {"name": format!("org-{i}")}, "spec": {}})),
        )
        .await
        .expect(StatusCode::CREATED);
    }
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let path = match &cursor {
            Some(c) => format!("/organizations?limit=2&cursor={c}"),
            None => "/organizations?limit=2".to_owned(),
        };
        let v = env
            .admin(Method::GET, &path, None)
            .await
            .expect(StatusCode::OK)
            .json();
        let items = v["items"].as_array().unwrap();
        assert!(items.len() <= 2);
        seen.extend(
            items
                .iter()
                .map(|o| o["metadata"]["name"].as_str().unwrap().to_owned()),
        );
        match v.get("nextCursor").and_then(Value::as_str) {
            Some(c) => cursor = Some(c.to_owned()),
            None => break,
        }
    }
    assert_eq!(seen, ["acme", "org-0", "org-1", "org-2", "org-3", "org-4"]);
    let v = env
        .admin(Method::GET, "/organizations?label=env=prod", None)
        .await
        .expect(StatusCode::OK)
        .json();
    assert_eq!(v["items"].as_array().unwrap().len(), 1);
    env.admin(Method::GET, "/organizations?limit=0", None)
        .await
        .problem(StatusCode::BAD_REQUEST, "bad-request");

    // --- Modellgruppen (lesend) ---------------------------------------------------------------
    let v = env
        .admin(Method::GET, "/model-groups?q=latest", None)
        .await
        .expect(StatusCode::OK)
        .json();
    assert_eq!(v["items"][0]["metadata"]["name"], "gpt-4o");
    env.admin(Method::GET, "/model-groups/claude", None)
        .await
        .expect(StatusCode::OK);
    env.admin(Method::GET, "/model-groups/nope", None)
        .await
        .problem(StatusCode::NOT_FOUND, "not-found");

    // --- Löschen ------------------------------------------------------------------------------
    env.admin(Method::DELETE, "/organizations/acme", None)
        .await
        .problem(StatusCode::CONFLICT, "conflict");
    env.admin(Method::DELETE, "/api-keys/probe", None)
        .await
        .expect(StatusCode::NO_CONTENT);
    env.admin(Method::GET, "/api-keys/probe", None)
        .await
        .problem(StatusCode::NOT_FOUND, "not-found");
    env.wait_chat(&probe, "claude", StatusCode::UNAUTHORIZED)
        .await;
    env.admin(Method::DELETE, "/organizations/org-0", None)
        .await
        .expect(StatusCode::NO_CONTENT);

    env.reloader.shutdown().await;
}

/// Fehlerobjekt (OpenAI) für einen gesperrten Key – zum Vergleich mit einem unbekannten.
async fn r_openai_error(env: &Env, key: &str) -> Value {
    env.chat(key, "gpt-4o").await.json()["error"].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_in_memory() {
    scenario(Store::connect("sqlite::memory:").await.unwrap()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_file() {
    let dir = std::env::temp_dir().join(format!("aigw-admin-test-{}", new_id()));
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
    let db = format!("aigw_admin_test_{}", new_id().simple());
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
