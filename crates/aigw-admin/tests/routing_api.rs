// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Integrationstests AIGW-65: Modellgruppen, Deployments, Anbieter-Zugangsdaten und Preise
//! über die Admin-API mit echtem Server und echter Datenbank – Alias-Kollisionen,
//! Fallback-Zyklen, SSRF-Ablehnung, Geheimnisse nie in Antworten/Audit-Log, PATCH erhält
//! Geheimwerte, Preisversionierung, Hot Reload in Snapshot und Datenebene.
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

use aigw_admin::{AdminState, BootstrapToken, UpstreamPolicy};
use aigw_core::config::AdminConfig;
use aigw_core::resource::new_id;
use aigw_core::snapshot::{Snapshot, SnapshotHandle};
use aigw_core::types::ProviderKind;
use aigw_proxy::{ProxyState, SnapshotAuthenticator};
use aigw_store::{Keyring, MasterKey, ReloadOptions, Reloader, ReloaderTask, Store};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{HeaderMap, Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};
use sqlx::AssertSqlSafe;

const SECRET1: &str = "sk-test-erstes-geheimnis-0001";
const SECRET2: &str = "org-zweites-geheimnis-0002";
const SECRET3: &str = "sk-test-drittes-geheimnis-0003";
const SECRET4: &str = "sk-test-viertes-geheimnis-0004";

struct Env {
    addr: SocketAddr,
    token: String,
    store: Store,
    keyring: Arc<Keyring>,
    snapshot: SnapshotHandle,
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

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
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
        assert_eq!(self.status, status, "Antwort: {}", self.text());
        self
    }

    #[track_caller]
    fn problem(&self, status: StatusCode, code: &str) -> String {
        assert_eq!(self.status, status, "{}", self.text());
        assert_eq!(self.header("content-type"), "application/problem+json");
        let v = self.json();
        assert_eq!(v["code"], code, "{v}");
        v["detail"].as_str().unwrap_or_default().to_owned()
    }
}

/// Kein Geheimwert in der Antwort.
#[track_caller]
fn assert_no_secrets(text: &str) {
    for s in [SECRET1, SECRET2, SECRET3, SECRET4] {
        assert!(!text.contains(s), "Geheimnis in Ausgabe: {text}");
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
    let mut has_ct = false;
    for (k, v) in headers {
        has_ct |= k.eq_ignore_ascii_case("content-type");
        req = req.header(*k, *v);
    }
    let body = match body {
        Some(b) => {
            if !has_ct {
                req = req.header("content-type", "application/json");
            }
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
        let proxy = ProxyState {
            auth: Arc::new(SnapshotAuthenticator::new(snapshot.clone())),
            ..ProxyState::mock()
        };
        // Ausnahmen wie für einen lokalen Ollama: http + privat nur für `localhost`.
        let policy = UpstreamPolicy::from_config(&AdminConfig {
            allow_insecure_upstreams: vec!["localhost".into()],
            allow_private_upstreams: vec!["localhost".into()],
        })
        .unwrap();
        let (token, secret) = BootstrapToken::generate().unwrap();
        let admin = aigw_admin::router_with(
            AdminState::new(store.clone(), Arc::new(token))
                .with_keyring(Arc::clone(&keyring))
                .with_upstream_policy(policy),
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
        let r = send(
            self.addr,
            method,
            &format!("/admin/api/v1{path}"),
            &headers,
            body,
        )
        .await;
        assert_no_secrets(&r.text());
        r
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
                r.text()
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

    /// Wartet, bis der Snapshot (nach dem Hot Reload) die Bedingung erfüllt.
    async fn wait_snapshot(&self, what: &str, cond: impl Fn(&Snapshot) -> bool) -> Arc<Snapshot> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let s = self.snapshot.load();
            if cond(&s) {
                return s;
            }
            assert!(
                Instant::now() < deadline,
                "Snapshot (Version {}) erfüllt nicht: {what}",
                s.version
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Entschlüsselte Werte einer Zugangsdaten-Ressource (direkt aus dem Store).
    async fn secret_values(&self, name: &str) -> Vec<(String, String)> {
        let all = self.store.decrypt_credentials(&self.keyring).await.unwrap();
        let (_, data) = all.get(name).unwrap();
        data.0
            .iter()
            .map(|(k, v)| (k.clone(), v.expose().to_owned()))
            .collect()
    }
}

async fn model_groups(env: &Env) {
    // Anlegen mit Aliassen und Strategie
    let r = env
        .admin(
            Method::POST,
            "/model-groups",
            Some(
                json!({"metadata": {"name": "gpt-4o", "labels": {"tier": "premium"}},
                        "spec": {"aliases": ["gpt-4o-latest", "openai/gpt-4o"],
                                 "routing": "least-latency"}}),
            ),
        )
        .await
        .expect(StatusCode::CREATED);
    assert_eq!(r.header("location"), "/admin/api/v1/model-groups/gpt-4o");
    assert!(r.header("etag").starts_with('"'));
    assert_eq!(r.json()["spec"]["routing"], "least-latency");
    env.admin(
        Method::POST,
        "/model-groups",
        Some(json!({"metadata": {"name": "claude"}, "spec": {"aliases": ["claude-sonnet"]}})),
    )
    .await
    .expect(StatusCode::CREATED);

    // Aliasse global eindeutig: Alias ↔ Alias, Name ↔ Alias, Alias ↔ Name → 409
    let version = env.store.config_version().await.unwrap();
    env.admin(
        Method::POST,
        "/model-groups",
        Some(json!({"metadata": {"name": "other"}, "spec": {"aliases": ["gpt-4o-latest"]}})),
    )
    .await
    .problem(StatusCode::CONFLICT, "conflict");
    env.admin(
        Method::POST,
        "/model-groups",
        Some(json!({"metadata": {"name": "claude-sonnet"}, "spec": {}})),
    )
    .await
    .problem(StatusCode::CONFLICT, "conflict");
    env.admin(
        Method::PUT,
        "/model-groups/claude",
        Some(json!({"metadata": {"name": "claude"}, "spec": {"aliases": ["gpt-4o"]}})),
    )
    .await
    .problem(StatusCode::CONFLICT, "conflict");
    // fehlgeschlagene Änderungen hinterlassen keine Konfigurationsversion
    assert_eq!(env.store.config_version().await.unwrap(), version);
    // eigener Name / doppelt / Leerzeichen → 422
    for aliases in [json!(["x", "x"]), json!(["new"]), json!(["a b"])] {
        env.admin(
            Method::POST,
            "/model-groups",
            Some(json!({"metadata": {"name": "new"}, "spec": {"aliases": aliases}})),
        )
        .await
        .problem(StatusCode::UNPROCESSABLE_ENTITY, "validation-failed");
    }

    // Routing-Einstellungen werden mit dem Parser der Datenebene geprüft (Tippfehler → 422)
    let d = env
        .admin(
            Method::POST,
            "/model-groups",
            Some(json!({"metadata": {"name": "new"},
                        "spec": {"settings": {"retry": {"maxAtempts": 3}}}})),
        )
        .await
        .problem(StatusCode::UNPROCESSABLE_ENTITY, "validation-failed");
    assert!(d.contains("settings"), "{d}");
    env.admin(
        Method::POST,
        "/model-groups",
        Some(json!({"metadata": {"name": "new"},
                    "spec": {"settings": {"retry": {"maxAttempts": 3},
                                          "circuitBreaker": {"failureRate": 0.5}}}})),
    )
    .await
    .expect(StatusCode::CREATED);
    env.admin(Method::DELETE, "/model-groups/new", None)
        .await
        .expect(StatusCode::NO_CONTENT);

    // Fallbacks: gültig, unbekannt, Alias statt Name, selbst, Zyklus
    let r = env
        .admin(Method::GET, "/model-groups/gpt-4o", None)
        .await
        .expect(StatusCode::OK);
    let etag = r.header("etag");
    env.admin_with(
        Method::PUT,
        "/model-groups/gpt-4o",
        &[("if-match", &etag)],
        Some(
            json!({"metadata": {"name": "gpt-4o", "labels": {"tier": "premium"}},
                    "spec": {"aliases": ["gpt-4o-latest", "openai/gpt-4o"],
                             "routing": "least-latency",
                             "fallbacks": [{"modelGroup": "claude", "on": ["rate-limit"]}]}}),
        ),
    )
    .await
    .expect(StatusCode::OK);
    // alter ETag → 412
    env.admin_with(
        Method::PUT,
        "/model-groups/gpt-4o",
        &[("if-match", &etag)],
        Some(json!({"metadata": {"name": "gpt-4o"}, "spec": {}})),
    )
    .await
    .problem(StatusCode::PRECONDITION_FAILED, "precondition-failed");
    for (fallback, hint) in [
        ("nope", "unbekannte"),
        ("claude-sonnet", "Alias"),
        ("claude", "selbst"),
        ("gpt-4o", "Zyklus"),
    ] {
        let d = env
            .admin(
                Method::PUT,
                "/model-groups/claude",
                Some(json!({"metadata": {"name": "claude"},
                            "spec": {"aliases": ["claude-sonnet"],
                                     "fallbacks": [{"modelGroup": fallback}]}})),
            )
            .await
            .problem(StatusCode::UNPROCESSABLE_ENTITY, "validation-failed");
        assert!(d.contains(hint), "{fallback}: {d}");
    }
    // Fallback-Ziel kann nicht gelöscht werden
    let d = env
        .admin(Method::DELETE, "/model-groups/claude", None)
        .await
        .problem(StatusCode::CONFLICT, "conflict");
    assert!(d.contains("Fallback"), "{d}");

    // Liste: Suche auch in Aliassen, Labels
    let r = env
        .admin(Method::GET, "/model-groups?q=sonnet", None)
        .await
        .expect(StatusCode::OK);
    assert_eq!(r.json()["items"].as_array().unwrap().len(), 1);
    assert_eq!(r.json()["items"][0]["metadata"]["name"], "claude");
    let r = env
        .admin(Method::GET, "/model-groups?label=tier=premium", None)
        .await
        .expect(StatusCode::OK);
    assert_eq!(r.json()["items"][0]["metadata"]["name"], "gpt-4o");
    env.admin(Method::GET, "/model-groups?typo=1", None)
        .await
        .problem(StatusCode::BAD_REQUEST, "bad-request");

    // Alias freigeben und neu vergeben
    env.admin(
        Method::POST,
        "/model-groups",
        Some(json!({"metadata": {"name": "tmp"}, "spec": {"aliases": ["tmp-alias"]}})),
    )
    .await
    .expect(StatusCode::CREATED);
    env.admin(Method::DELETE, "/model-groups/tmp", None)
        .await
        .expect(StatusCode::NO_CONTENT);
    env.admin(
        Method::POST,
        "/model-groups",
        Some(json!({"metadata": {"name": "tmp-alias"}, "spec": {}})),
    )
    .await
    .expect(StatusCode::CREATED);
    env.admin(Method::DELETE, "/model-groups/tmp-alias", None)
        .await
        .expect(StatusCode::NO_CONTENT);
}

async fn credentials(env: &Env) {
    // Anlegen: Antwort ohne Werte, mit hasSecret/keyId/Zeitstempeln
    let r = env
        .admin(
            Method::POST,
            "/provider-credentials",
            Some(json!({"metadata": {"name": "openai-prod"},
                        "spec": {"provider": "openai", "data": {"apiKey": {"value": SECRET1}}}})),
        )
        .await
        .expect(StatusCode::CREATED);
    assert_eq!(r.header("cache-control"), "no-store");
    assert_eq!(
        r.header("location"),
        "/admin/api/v1/provider-credentials/openai-prod"
    );
    let v = r.json();
    assert_eq!(v["spec"]["provider"], "openai");
    assert_eq!(v["spec"]["fields"], json!(["apiKey"]));
    assert_eq!(v["status"]["hasSecret"], true);
    assert_eq!(v["status"]["keyId"], env.keyring.primary_id());
    assert!(v["metadata"]["createdAt"].is_string());
    assert!(v["metadata"]["updatedAt"].is_string());
    assert!(v["spec"].get("data").is_none());

    // Ungültige Eingaben (Geheimnis taucht auch im Fehlertext nicht auf)
    for (body, code) in [
        // valueFrom liest die Server-Umgebung → über die API verboten
        (
            json!({"metadata": {"name": "x"}, "spec": {"provider": "openai",
                   "data": {"apiKey": {"valueFrom": {"env": "AIGW_MASTER_KEY"}}}}}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        // Pflichtfeld des Anbieters fehlt
        (
            json!({"metadata": {"name": "x"}, "spec": {"provider": "openai",
                   "data": {"token": {"value": SECRET2}}}}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            json!({"metadata": {"name": "x"}, "spec": {"provider": "bedrock",
                   "data": {"accessKeyId": {"value": SECRET2}}}}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        // Wert direkt statt {"value": …}: serde würde den Wert im Fehler zitieren
        (
            json!({"metadata": {"name": "x"}, "spec": {"provider": "openai",
                   "data": {"apiKey": SECRET2}}}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        // Platzhalter aus einer GET-Antwort ist kein Wert
        (
            json!({"metadata": {"name": "x"}, "spec": {"provider": "openai",
                   "data": {"apiKey": {"value": "<redacted>"}}}}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            json!({"metadata": {"name": "x"}, "spec": {"provider": "openai",
                   "data": {"apiKey": {"value": ""}}}}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        let r = env
            .admin(Method::POST, "/provider-credentials", Some(body))
            .await;
        assert_eq!(r.status, code, "{}", r.text());
    }
    // Name vergeben
    env.admin(
        Method::POST,
        "/provider-credentials",
        Some(json!({"metadata": {"name": "openai-prod"},
                    "spec": {"provider": "openai", "data": {"apiKey": {"value": SECRET2}}}})),
    )
    .await
    .problem(StatusCode::CONFLICT, "conflict");
    assert_eq!(
        env.secret_values("openai-prod").await,
        [("apiKey".into(), SECRET1.into())]
    );

    // Lesen/Liste ohne Werte
    let r = env
        .admin(Method::GET, "/provider-credentials/openai-prod", None)
        .await
        .expect(StatusCode::OK);
    let etag = r.header("etag");
    let r = env
        .admin(Method::GET, "/provider-credentials?provider=openai", None)
        .await
        .expect(StatusCode::OK);
    assert_eq!(r.json()["items"].as_array().unwrap().len(), 1);

    // PATCH: neues Feld, apiKey bleibt erhalten
    let r = env
        .admin_with(
            Method::PATCH,
            "/provider-credentials/openai-prod",
            &[
                ("if-match", &etag),
                ("content-type", "application/merge-patch+json"),
            ],
            Some(json!({"spec": {"data": {"organization": {"value": SECRET2}}}})),
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(
        r.json()["spec"]["fields"],
        json!(["apiKey", "organization"])
    );
    assert_ne!(r.header("etag"), etag);
    assert_eq!(
        env.secret_values("openai-prod").await,
        [
            ("apiKey".into(), SECRET1.into()),
            ("organization".into(), SECRET2.into())
        ]
    );
    // alter ETag → 412
    env.admin_with(
        Method::PATCH,
        "/provider-credentials/openai-prod",
        &[("if-match", &etag)],
        Some(json!({"metadata": {"labels": {"a": "b"}}})),
    )
    .await
    .problem(StatusCode::PRECONDITION_FAILED, "precondition-failed");
    // Pflichtfeld entfernen → 422; optionales Feld entfernen → ok
    env.admin(
        Method::PATCH,
        "/provider-credentials/openai-prod",
        Some(json!({"spec": {"data": {"apiKey": null}}})),
    )
    .await
    .problem(StatusCode::UNPROCESSABLE_ENTITY, "validation-failed");
    let r = env
        .admin(
            Method::PATCH,
            "/provider-credentials/openai-prod",
            Some(json!({"metadata": {"labels": {"env": "prod"}},
                        "spec": {"data": {"organization": null}}})),
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(r.json()["spec"]["fields"], json!(["apiKey"]));
    assert_eq!(r.json()["metadata"]["labels"]["env"], "prod");
    assert_eq!(
        env.secret_values("openai-prod").await,
        [("apiKey".into(), SECRET1.into())]
    );
    // Umbenennen per PATCH nicht möglich
    env.admin(
        Method::PATCH,
        "/provider-credentials/openai-prod",
        Some(json!({"metadata": {"name": "other"}})),
    )
    .await
    .problem(StatusCode::UNPROCESSABLE_ENTITY, "validation-failed");

    // PUT ersetzt alle Werte
    let r = env
        .admin(
            Method::PUT,
            "/provider-credentials/openai-prod",
            Some(
                json!({"metadata": {"name": "openai-prod", "labels": {"env": "prod"}},
                        "spec": {"provider": "openai", "data": {"apiKey": {"value": SECRET3}}}}),
            ),
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(r.json()["spec"]["fields"], json!(["apiKey"]));
    assert_eq!(
        env.secret_values("openai-prod").await,
        [("apiKey".into(), SECRET3.into())]
    );

    // Azure-Zugangsdaten (Entra-ID-Variante) für später
    env.admin(
        Method::POST,
        "/provider-credentials",
        Some(json!({"metadata": {"name": "azure-entra"},
                    "spec": {"provider": "azure-openai", "data": {
                        "tenantId": {"value": "t"}, "clientId": {"value": "c"},
                        "clientSecret": {"value": SECRET4}}}})),
    )
    .await
    .expect(StatusCode::CREATED);
}

async fn deployments(env: &Env) {
    let r = env
        .admin(
            Method::POST,
            "/deployments",
            Some(json!({"metadata": {"name": "gpt-4o-openai"},
                        "spec": {"modelGroup": "gpt-4o", "provider": "openai",
                                 "providerModel": "gpt-4o-2024-08-06",
                                 "credential": "openai-prod", "weight": 3,
                                 "capacity": {"rpm": 500, "tpm": 300000}}})),
        )
        .await
        .expect(StatusCode::CREATED);
    let v = r.json();
    assert_eq!(v["spec"]["capacity"]["rpm"], 500);
    assert_eq!(v["spec"]["enabled"], true);
    assert_eq!(v["spec"]["priority"], 0);
    env.admin(
        Method::POST,
        "/deployments",
        Some(json!({"metadata": {"name": "gpt-4o-azure"},
                    "spec": {"modelGroup": "gpt-4o", "provider": "azure-openai",
                             "endpoint": "https://acme.openai.azure.com",
                             "providerModel": "gpt4o-ptu", "credential": "azure-entra",
                             "priority": 1, "settings": {"apiVersion": "2024-10-21"}}})),
    )
    .await
    .expect(StatusCode::CREATED);
    // lokaler Ollama: per Konfiguration erlaubt (http + privat für `localhost`)
    env.admin(
        Method::POST,
        "/deployments",
        Some(json!({"metadata": {"name": "claude-local"},
                    "spec": {"modelGroup": "claude", "provider": "ollama",
                             "endpoint": "http://localhost:11434/v1",
                             "providerModel": "llama3.1", "enabled": false}})),
    )
    .await
    .expect(StatusCode::CREATED);

    // Ablehnungen
    let base = |spec: Value| json!({"metadata": {"name": "bad"}, "spec": spec});
    for (spec, hint) in [
        // SSRF
        (
            json!({"modelGroup": "gpt-4o", "provider": "openai-compatible",
                   "endpoint": "http://10.0.0.1:8000/v1", "providerModel": "m"}),
            "endpoint",
        ),
        (
            json!({"modelGroup": "gpt-4o", "provider": "openai-compatible",
                   "endpoint": "https://10.0.0.1:8000/v1", "providerModel": "m"}),
            "privat",
        ),
        (
            json!({"modelGroup": "gpt-4o", "provider": "openai",
                   "endpoint": "https://169.254.169.254/latest", "providerModel": "m"}),
            "privat",
        ),
        (
            json!({"modelGroup": "gpt-4o", "provider": "openai",
                   "endpoint": "https://[::ffff:127.0.0.1]/", "providerModel": "m"}),
            "privat",
        ),
        (
            json!({"modelGroup": "gpt-4o", "provider": "openai",
                   "endpoint": "https://user:pw@api.example.com", "providerModel": "m"}),
            "Zugangsdaten",
        ),
        (
            json!({"modelGroup": "gpt-4o", "provider": "openai",
                   "endpoint": "file:///etc/passwd", "providerModel": "m"}),
            "endpoint",
        ),
        // Pflichtfelder je Anbieter
        (
            json!({"modelGroup": "gpt-4o", "provider": "vllm", "providerModel": "m"}),
            "Pflicht",
        ),
        (
            json!({"modelGroup": "gpt-4o", "provider": "azure-openai",
                   "endpoint": "https://acme.openai.azure.com", "providerModel": "d"}),
            "apiVersion",
        ),
        (
            json!({"modelGroup": "gpt-4o", "provider": "azure-openai",
                   "endpoint": "https://acme.openai.azure.com", "providerModel": "a/b",
                   "settings": {"apiVersion": "2024-10-21"}}),
            "Deployment-Name",
        ),
        // Zugangsdaten eines anderen Anbieters
        (
            json!({"modelGroup": "gpt-4o", "provider": "azure-openai",
                   "endpoint": "https://acme.openai.azure.com", "providerModel": "d",
                   "credential": "openai-prod", "settings": {"apiVersion": "2024-10-21"}}),
            "Anbieter",
        ),
        // unbekannte Verweise
        (
            json!({"modelGroup": "nope", "provider": "openai", "providerModel": "m"}),
            "nope",
        ),
        (
            json!({"modelGroup": "gpt-4o", "provider": "openai", "providerModel": "m",
                   "credential": "nope"}),
            "nope",
        ),
        // Gewicht/Kapazität
        (
            json!({"modelGroup": "gpt-4o", "provider": "openai", "providerModel": "m",
                   "weight": 0}),
            "weight",
        ),
        (
            json!({"modelGroup": "gpt-4o", "provider": "openai", "providerModel": "m",
                   "capacity": {"tpm": 0}}),
            "capacity",
        ),
    ] {
        let d = env
            .admin(Method::POST, "/deployments", Some(base(spec.clone())))
            .await
            .problem(StatusCode::UNPROCESSABLE_ENTITY, "validation-failed");
        assert!(d.contains(hint), "{spec}: {d}");
    }

    // Filter
    let names = |r: Resp| -> Vec<String> {
        r.json()["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["metadata"]["name"].as_str().unwrap().to_owned())
            .collect()
    };
    let r = env
        .admin(Method::GET, "/deployments?modelGroup=gpt-4o", None)
        .await
        .expect(StatusCode::OK);
    assert_eq!(names(r), ["gpt-4o-azure", "gpt-4o-openai"]);
    let r = env
        .admin(
            Method::GET,
            "/deployments?provider=ollama&enabled=false",
            None,
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(names(r), ["claude-local"]);
    let r = env
        .admin(Method::GET, "/deployments?credential=openai-prod", None)
        .await
        .expect(StatusCode::OK);
    assert_eq!(names(r), ["gpt-4o-openai"]);
    let r = env
        .admin(Method::GET, "/deployments?q=ptu", None)
        .await
        .expect(StatusCode::OK);
    assert_eq!(names(r), ["gpt-4o-azure"]);

    // Verwendete Ressourcen lassen sich nicht löschen bzw. umstellen
    env.admin(Method::DELETE, "/model-groups/gpt-4o", None)
        .await
        .problem(StatusCode::CONFLICT, "conflict");
    env.admin(Method::DELETE, "/provider-credentials/openai-prod", None)
        .await
        .problem(StatusCode::CONFLICT, "conflict");
    env.admin(
        Method::PATCH,
        "/provider-credentials/openai-prod",
        Some(json!({"spec": {"provider": "anthropic"}})),
    )
    .await
    .problem(StatusCode::CONFLICT, "conflict");
}

async fn snapshot_and_data_plane(env: &Env) {
    // Key ohne Einschränkung
    let r = env
        .admin(
            Method::POST,
            "/api-keys",
            Some(json!({"metadata": {"name": "app"}, "spec": {}})),
        )
        .await
        .expect(StatusCode::CREATED);
    let key = r.json()["secret"].as_str().unwrap().to_owned();
    // Freigabe auf die neue Modellgruppe (über Alias)
    let r = env
        .admin(
            Method::POST,
            "/api-keys",
            Some(json!({"metadata": {"name": "only-gpt"},
                        "spec": {"allowedModels": ["gpt-4o-latest"]}})),
        )
        .await
        .expect(StatusCode::CREATED);
    let limited = r.json()["secret"].as_str().unwrap().to_owned();

    // Hot Reload: Snapshot enthält Gruppe, Aliasse, Deployments nach Priorität, entschlüsselte
    // Zugangsdaten (letzter Stand aus dem PUT); deaktivierte Deployments fehlen.
    let snap = env
        .wait_snapshot("gpt-4o mit zwei Deployments", |s| {
            s.model_groups
                .get("openai/gpt-4o")
                .is_some_and(|g| g.deployments.len() == 2)
        })
        .await;
    let g = &snap.model_groups["gpt-4o-latest"];
    assert_eq!(g.group.metadata.name, "gpt-4o");
    let deps: Vec<&str> = g
        .deployments
        .iter()
        .map(|d| d.deployment.metadata.name.as_str())
        .collect();
    assert_eq!(deps, ["gpt-4o-openai", "gpt-4o-azure"]);
    let cred = g.deployments[0].credential.as_ref().unwrap();
    assert_eq!(cred.get("apiKey").unwrap().expose(), SECRET3);
    assert!(snap.model_groups["claude"].deployments.is_empty());
    assert_eq!(g.group.spec.fallbacks[0].model_group, "claude");

    // Datenebene: /v1/models und Chat gegen die Modellgruppe (Mock-Anbieter bis AIGW-15)
    env.wait_chat(&key, "gpt-4o", StatusCode::OK).await;
    env.wait_chat(&key, "openai/gpt-4o", StatusCode::OK).await;
    env.wait_chat(&limited, "gpt-4o-latest", StatusCode::OK)
        .await;
    env.wait_chat(&limited, "claude", StatusCode::FORBIDDEN)
        .await;
    let mut models = env.models(&key).await;
    models.sort();
    assert_eq!(models, ["claude", "gpt-4o"]);
    assert_eq!(env.models(&limited).await, ["gpt-4o"]);

    // Deployment deaktivieren/aktivieren wirkt im Snapshot
    let r = env
        .admin(Method::GET, "/deployments/claude-local", None)
        .await
        .expect(StatusCode::OK);
    let mut body = r.json();
    body["spec"]["enabled"] = json!(true);
    let body = json!({"metadata": {"name": "claude-local"}, "spec": body["spec"]});
    env.admin(Method::PUT, "/deployments/claude-local", Some(body))
        .await
        .expect(StatusCode::OK);
    env.wait_snapshot("claude-local aktiv", |s| {
        s.model_groups
            .get("claude-sonnet")
            .is_some_and(|g| g.deployments.len() == 1)
    })
    .await;

    // Alias umbenennen: alter Alias verschwindet aus Snapshot und Datenebene
    env.admin(
        Method::PUT,
        "/model-groups/claude",
        Some(json!({"metadata": {"name": "claude"}, "spec": {"aliases": ["claude-3"]}})),
    )
    .await
    .expect(StatusCode::OK);
    env.wait_snapshot("Alias claude-3", |s| {
        s.model_groups.contains_key("claude-3") && !s.model_groups.contains_key("claude-sonnet")
    })
    .await;
    env.wait_chat(&key, "claude-sonnet", StatusCode::NOT_FOUND)
        .await;
    env.wait_chat(&key, "claude-3", StatusCode::OK).await;

    // Löschen: erst Deployments, dann Gruppe → Datenebene 404
    for d in ["gpt-4o-openai", "gpt-4o-azure"] {
        env.admin(Method::DELETE, &format!("/deployments/{d}"), None)
            .await
            .expect(StatusCode::NO_CONTENT);
    }
    env.admin(Method::DELETE, "/model-groups/gpt-4o", None)
        .await
        .expect(StatusCode::NO_CONTENT);
    env.wait_chat(&key, "gpt-4o", StatusCode::NOT_FOUND).await;
    env.admin(Method::DELETE, "/provider-credentials/openai-prod", None)
        .await
        .expect(StatusCode::NO_CONTENT);
    env.wait_snapshot("Zugangsdaten gelöscht", |s| {
        !s.credentials.contains_key("openai-prod")
    })
    .await;
}

async fn prices(env: &Env) {
    // Ohne Name → Server vergibt `<anbieter>-<modell>-<datum>`
    let r = env
        .admin(
            Method::POST,
            "/prices",
            Some(
                json!({"spec": {"provider": "openai", "model": "gpt-4o-2024-08-06",
                                 "validFrom": "2024-08-06T00:00:00Z",
                                 "perMillionTokens": {"input": "2.50", "cachedInput": "1.25",
                                                      "output": 10}}}),
            ),
        )
        .await
        .expect(StatusCode::CREATED);
    let v = r.json();
    assert_eq!(v["metadata"]["name"], "openai-gpt-4o-2024-08-06-20240806");
    assert_eq!(v["spec"]["perMillionTokens"]["output"], "10");
    assert_eq!(v["spec"]["perMillionTokens"]["input"], "2.5");
    let first = "/prices/openai-gpt-4o-2024-08-06-20240806";

    // gleiche Version (Anbieter, Modell, validFrom) unter anderem Namen → 409
    env.admin(
        Method::POST,
        "/prices",
        Some(json!({"metadata": {"name": "dup"},
                    "spec": {"provider": "openai", "model": "gpt-4o-2024-08-06",
                             "validFrom": "2024-08-06T00:00:00Z"}})),
    )
    .await
    .problem(StatusCode::CONFLICT, "conflict");
    // negative Preise → 422
    env.admin(
        Method::POST,
        "/prices",
        Some(
            json!({"spec": {"model": "m", "validFrom": "2024-01-01T00:00:00Z",
                             "perMillionTokens": {"output": "-1"}}}),
        ),
    )
    .await
    .problem(StatusCode::UNPROCESSABLE_ENTITY, "validation-failed");

    // gültige Version: Preis ändern/löschen → 409, Labels ändern → ok
    let spec = v["spec"].clone();
    let mut changed = spec.clone();
    changed["perMillionTokens"]["output"] = json!("12");
    let d = env
        .admin(
            Method::PUT,
            first,
            Some(
                json!({"metadata": {"name": "openai-gpt-4o-2024-08-06-20240806"},
                        "spec": changed}),
            ),
        )
        .await
        .problem(StatusCode::CONFLICT, "conflict");
    assert!(d.contains("neue Version"), "{d}");
    env.admin(
        Method::PUT,
        first,
        Some(
            json!({"metadata": {"name": "openai-gpt-4o-2024-08-06-20240806",
                                 "labels": {"source": "openai.com/pricing"}},
                    "spec": spec}),
        ),
    )
    .await
    .expect(StatusCode::OK);
    env.admin(Method::DELETE, first, None)
        .await
        .problem(StatusCode::CONFLICT, "conflict");

    // Neue Version (vergangen) und künftige Version
    env.admin(
        Method::POST,
        "/prices",
        Some(
            json!({"spec": {"provider": "openai", "model": "gpt-4o-2024-08-06",
                             "validFrom": "2025-01-01T12:00:00Z",
                             "perMillionTokens": {"input": "2", "output": "8"}}}),
        ),
    )
    .await
    .expect(StatusCode::CREATED);
    let future = time::OffsetDateTime::now_utc() + time::Duration::days(30);
    let future = future
        .replace_nanosecond(0)
        .unwrap()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    env.admin(
        Method::POST,
        "/prices",
        Some(json!({"metadata": {"name": "gpt-4o-next"},
                    "spec": {"provider": "openai", "model": "gpt-4o-2024-08-06",
                             "validFrom": future, "perMillionTokens": {"output": "6"}}})),
    )
    .await
    .expect(StatusCode::CREATED);
    // künftige Version darf geändert werden – aber nicht in die Vergangenheit verlegt
    env.admin(
        Method::PUT,
        "/prices/gpt-4o-next",
        Some(json!({"metadata": {"name": "gpt-4o-next"},
                    "spec": {"provider": "openai", "model": "gpt-4o-2024-08-06",
                             "validFrom": future, "perMillionTokens": {"output": "5"}}})),
    )
    .await
    .expect(StatusCode::OK);
    env.admin(
        Method::PUT,
        "/prices/gpt-4o-next",
        Some(json!({"metadata": {"name": "gpt-4o-next"},
                    "spec": {"provider": "openai", "model": "gpt-4o-2024-08-06",
                             "validFrom": "2025-06-01T00:00:00Z"}})),
    )
    .await
    .problem(StatusCode::CONFLICT, "conflict");

    // Liste: Filter und „gültig zum Zeitpunkt“
    let list = |q: &'static str| async move {
        env.admin(Method::GET, &format!("/prices{q}"), None)
            .await
            .expect(StatusCode::OK)
            .json()["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["metadata"]["name"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(list("?model=gpt-4o-2024-08-06").await.len(), 3);
    assert_eq!(
        list("?effectiveAt=2024-12-01T00:00:00Z").await,
        ["openai-gpt-4o-2024-08-06-20240806"]
    );
    assert_eq!(
        list("?effectiveAt=2025-06-01T00:00:00Z&provider=openai").await,
        ["openai-gpt-4o-2024-08-06-20250101120000"]
    );
    assert!(list("?provider=any").await.is_empty());
    env.admin(Method::GET, "/prices?provider=nope", None)
        .await
        .problem(StatusCode::BAD_REQUEST, "bad-request");

    // Snapshot: Versionen je (Anbieter, Modell), aufsteigend
    let snap = env
        .wait_snapshot("drei Preisversionen", |s| {
            s.prices
                .get(&(Some(ProviderKind::OpenAi), "gpt-4o-2024-08-06".to_owned()))
                .is_some_and(|v| v.len() == 3)
        })
        .await;
    // 2025-03-01T00:00:00Z
    let at = time::OffsetDateTime::from_unix_timestamp(1_740_787_200).unwrap();
    let p = snap
        .price_at(ProviderKind::OpenAi, "gpt-4o-2024-08-06", at)
        .unwrap();
    assert_eq!(p.metadata.name, "openai-gpt-4o-2024-08-06-20250101120000");

    // künftige Version löschen ist erlaubt
    env.admin(Method::DELETE, "/prices/gpt-4o-next", None)
        .await
        .expect(StatusCode::NO_CONTENT);
}

/// GitOps wendet dieselben Regeln an (gemeinsam in `aigw_core::validate`).
async fn gitops_rules(env: &Env) {
    use aigw_store::gitops::{self, ApplyOptions, GitopsError, ProcessSecrets};
    let yaml = "\
apiVersion: aigw/v1alpha1
kind: ModelGroup
metadata: {name: g-a}
spec: {fallbacks: [{modelGroup: g-b}]}
---
apiVersion: aigw/v1alpha1
kind: ModelGroup
metadata: {name: g-b}
spec: {fallbacks: [{modelGroup: g-a}], aliases: [x, x]}
---
apiVersion: aigw/v1alpha1
kind: ModelGroup
metadata: {name: g-c}
spec: {settings: {retry: {maxAtempts: 3}}}
---
apiVersion: aigw/v1alpha1
kind: Deployment
metadata: {name: ssrf}
spec: {modelGroup: gpt-4o, provider: openai-compatible, endpoint: 'https://10.1.1.1/v1', providerModel: m}
---
apiVersion: aigw/v1alpha1
kind: Deployment
metadata: {name: azure-no-version}
spec: {modelGroup: gpt-4o, provider: azure-openai, endpoint: 'https://a.openai.azure.com', providerModel: d}
---
apiVersion: aigw/v1alpha1
kind: Deployment
metadata: {name: wrong-cred}
spec: {modelGroup: gpt-4o, provider: openai, providerModel: m, credential: azure-entra}
---
apiVersion: aigw/v1alpha1
kind: Price
metadata: {name: negative}
spec: {model: m, validFrom: 2024-01-01T00:00:00Z, perMillionTokens: {output: '-1'}}
";
    let docs = gitops::parse_documents(yaml).unwrap();
    let opts = ApplyOptions {
        dry_run: true,
        group_settings: Some(aigw_admin::resources::group_settings),
        ..ApplyOptions::default()
    };
    let err = gitops::plan(&env.store, &env.keyring, &docs, &opts, &ProcessSecrets)
        .await
        .unwrap_err();
    let GitopsError::Invalid(issues) = err else {
        panic!("Validierungsfehler erwartet: {err}");
    };
    let text: Vec<String> = issues.iter().map(ToString::to_string).collect();
    let text = text.join("\n");
    for want in [
        "doppelt",
        "Zyklus",
        "privat",
        "apiVersion",
        "gehört zu Anbieter",
        "negativ",
        "settings",
    ] {
        assert!(text.contains(want), "{want} fehlt in:\n{text}");
    }
}

async fn audit_has_no_secrets(env: &Env) {
    let entries = env.store.audit_log(10_000).await.unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e.kind == "ProviderCredential" && e.action == "update")
    );
    let text = serde_json::to_string(&entries).unwrap();
    assert_no_secrets(&text);
}

async fn scenario(store: Store) {
    let env = Env::start(store).await;
    // ohne Anmeldung → 401
    send(
        env.addr,
        Method::GET,
        "/admin/api/v1/deployments",
        &[],
        None,
    )
    .await
    .problem(StatusCode::UNAUTHORIZED, "unauthorized");
    model_groups(&env).await;
    credentials(&env).await;
    deployments(&env).await;
    gitops_rules(&env).await;
    snapshot_and_data_plane(&env).await;
    prices(&env).await;
    audit_has_no_secrets(&env).await;
    env.reloader.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_in_memory() {
    scenario(Store::connect("sqlite::memory:").await.unwrap()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_file() {
    let dir = std::env::temp_dir().join(format!("aigw-routing-test-{}", new_id()));
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
    let db = format!("aigw_routing_test_{}", new_id().simple());
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
