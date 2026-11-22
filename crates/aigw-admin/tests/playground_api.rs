// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Playground der Admin-UI (AIGW-54) Ende zu Ende: echter Server mit Admin-API,
//! `SnapshotAuthenticator`, `SnapshotRoutes` (Mock-Deployments) und Reloader.
//!
//! Geprüft: Anmeldung nötig, unbekannter/gesperrter Key → 404, Modellliste je Key, Chat als JSON
//! und als SSE durch die Pipeline der Datenebene (Routing-Header, Request-ID), Modellfreigabe des
//! Keys greift (403 im OpenAI-Format), Audit-Eintrag mit Geltungsbereich und Request-ID, Modus „nur YAML“ lässt den Playground zu.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aigw_admin::playground::{PlaygroundState, router as playground_router};
use aigw_admin::{AdminAuthenticator, AdminState, BootstrapToken};
use aigw_proxy::{ProxyState, SnapshotAuthenticator, SnapshotRoutes};
use aigw_store::{Keyring, MasterKey, ReloadOptions, Reloader, ReloaderTask, Store};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};

struct Env {
    addr: SocketAddr,
    token: String,
    store: Store,
    _reloader: ReloaderTask,
    _routes: aigw_proxy::RoutesTask,
}

struct Resp {
    status: StatusCode,
    headers: hyper::HeaderMap,
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

    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .map(|v| v.to_str().unwrap().to_owned())
    }

    #[track_caller]
    fn expect(self, status: StatusCode) -> Self {
        assert_eq!(self.status, status, "Antwort: {}", self.text());
        self
    }
}

impl Env {
    async fn start(read_only: bool) -> Self {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        store.migrate().await.unwrap();
        let keyring = Arc::new(Keyring::new(MasterKey::generate().unwrap().0));
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
        let routes = Arc::new(SnapshotRoutes::with_defaults().unwrap());
        let routes_task = routes.spawn(snapshot.clone());
        let proxy = ProxyState {
            routes,
            auth: Arc::new(SnapshotAuthenticator::new(snapshot.clone())),
            ..ProxyState::mock()
        };
        let (token, secret) = BootstrapToken::generate().unwrap();
        let auth: Arc<dyn AdminAuthenticator> = Arc::new(token);
        let admin = aigw_admin::router_with(
            AdminState::new(store.clone(), Arc::clone(&auth))
                .with_keyring(Arc::clone(&keyring))
                .with_read_only(read_only),
        )
        .merge(playground_router(PlaygroundState {
            store: store.clone(),
            auth,
            snapshot,
            proxy: Arc::new(aigw_proxy::playground::state_for(&proxy)),
        }));
        let admin = aigw_admin::read_only::apply(admin, read_only);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, admin).await.unwrap() });
        Self {
            addr,
            token: secret,
            store,
            _reloader: reloader.spawn(),
            _routes: routes_task,
        }
    }

    async fn send(&self, method: Method, path: &str, body: Option<Value>, auth: bool) -> Resp {
        let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
        let mut req = Request::builder()
            .method(method)
            .uri(format!("http://{}/admin/api/v1{path}", self.addr));
        if auth {
            req = req.header("authorization", format!("Bearer {}", self.token));
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

    /// Ressourcen direkt in der Datenbank anlegen (Admin-API ist im Modus „nur YAML“ gesperrt).
    async fn seed(&self) {
        let yaml = "apiVersion: aigw/v1alpha1\nkind: Organization\nmetadata: {name: acme}\n---\n\
            apiVersion: aigw/v1alpha1\nkind: Team\nmetadata: {name: platform}\nspec: {organization: acme}\n---\n\
            apiVersion: aigw/v1alpha1\nkind: ModelGroup\nmetadata: {name: echo}\nspec: {routing: weighted, aliases: [echo-alias]}\n---\n\
            apiVersion: aigw/v1alpha1\nkind: Deployment\nmetadata: {name: echo-mock}\nspec: {modelGroup: echo, provider: mock, providerModel: mock}\n---\n\
            apiVersion: aigw/v1alpha1\nkind: ModelGroup\nmetadata: {name: other}\nspec: {routing: weighted}\n---\n\
            apiVersion: aigw/v1alpha1\nkind: Deployment\nmetadata: {name: other-mock}\nspec: {modelGroup: other, provider: mock, providerModel: mock}\n";
        let docs = aigw_store::gitops::parse_documents(yaml).unwrap();
        let keyring = Keyring::new(MasterKey::generate().unwrap().0);
        aigw_store::gitops::apply(
            &self.store,
            &keyring,
            &docs,
            &aigw_store::gitops::ApplyOptions::default(),
            &aigw_store::gitops::ProcessSecrets,
        )
        .await
        .unwrap();
        // Key nur für `echo` freigegeben.
        self.store
            .create_api_key(
                "test",
                aigw_core::resource::ObjectMeta::named("ci"),
                aigw_core::resource::ApiKeySpec {
                    organization: "acme".into(),
                    team: Some("platform".into()),
                    allowed_models: vec!["echo".into()],
                    ..aigw_core::resource::ApiKeySpec::default()
                },
            )
            .await
            .unwrap();
    }

    /// Wartet, bis der Key im Snapshot steht.
    async fn wait_key(&self) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let r = self
                .send(Method::GET, "/playground/models?key=ci", None, true)
                .await;
            if r.status == StatusCode::OK {
                return r.json();
            }
            assert!(
                Instant::now() < deadline,
                "Key erscheint nicht: {}",
                r.text()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn chat_body(model: &str, stream: bool) -> Value {
    json!({
        "model": model,
        "stream": stream,
        "messages": [
            {"role": "system", "content": "Sei knapp."},
            {"role": "user", "content": "Hallo Playground"}
        ]
    })
}

#[tokio::test]
async fn playground_runs_through_the_pipeline() {
    let env = Env::start(false).await;
    env.seed().await;

    // Anmeldung nötig.
    env.send(Method::GET, "/playground/models?key=ci", None, false)
        .await
        .expect(StatusCode::UNAUTHORIZED);

    // Modellliste: nur freigegebene Gruppen.
    let models = env.wait_key().await;
    assert_eq!(models["organization"], "acme");
    assert_eq!(models["team"], "platform");
    let names: Vec<&str> = models["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["echo"], "{models}");
    assert_eq!(models["models"][0]["aliases"][0], "echo-alias");
    assert_eq!(models["models"][0]["exposeReasoning"], true);

    // Unbekannter Key → 404.
    env.send(Method::GET, "/playground/models?key=nope", None, true)
        .await
        .expect(StatusCode::NOT_FOUND);
    env.send(
        Method::POST,
        "/playground/chat?key=nope",
        Some(chat_body("echo", false)),
        true,
    )
    .await
    .expect(StatusCode::NOT_FOUND);

    // Chat als JSON.
    let r = env
        .send(
            Method::POST,
            "/playground/chat?key=ci",
            Some(chat_body("echo", false)),
            true,
        )
        .await
        .expect(StatusCode::OK);
    let rid = r.header("x-aigw-request-id").expect("Request-ID");
    assert_eq!(r.header("x-aigw-deployment").as_deref(), Some("echo-mock"));
    let v = r.json();
    assert_eq!(v["object"], "chat.completion");
    assert!(v["usage"]["total_tokens"].as_u64().unwrap() > 0, "{v}");

    // Chat als SSE.
    let r = env
        .send(
            Method::POST,
            "/playground/chat?key=ci",
            Some(chat_body("echo", true)),
            true,
        )
        .await
        .expect(StatusCode::OK);
    assert!(
        r.header("content-type")
            .unwrap()
            .starts_with("text/event-stream")
    );
    let text = r.text();
    assert!(text.contains("data: ") && text.contains("[DONE]"), "{text}");

    // Modellfreigabe des Keys greift (Fehler der Datenebene im OpenAI-Format).
    let r = env
        .send(
            Method::POST,
            "/playground/chat?key=ci",
            Some(chat_body("other", false)),
            true,
        )
        .await
        .expect(StatusCode::FORBIDDEN);
    assert_eq!(r.json()["error"]["code"], "model_not_allowed");

    // Ungültiges JSON → 400 (ohne Audit-Eintrag).
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let res = client
        .request(
            Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "http://{}/admin/api/v1/playground/chat?key=ci",
                    env.addr
                ))
                .header("authorization", format!("Bearer {}", env.token))
                .header("content-type", "application/json")
                .body(Full::new(Bytes::from_static(b"{nope")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    // Audit: je Chat ein Eintrag mit Bereich und Request-ID, ohne Inhalte.
    let audit = env
        .send(Method::GET, "/audit-log?kind=playground", None, true)
        .await
        .expect(StatusCode::OK)
        .json();
    let items = audit["items"].as_array().unwrap();
    assert_eq!(items.len(), 3, "{audit}");
    let first = items
        .iter()
        .find(|e| e["after"]["requestId"] == rid.as_str())
        .unwrap_or_else(|| panic!("Audit ohne Request-ID {rid}: {audit}"));
    assert_eq!(first["action"], "chat");
    assert_eq!(first["objectName"], "ci");
    assert_eq!(first["organization"], "acme");
    assert_eq!(first["team"], "platform");
    assert_eq!(first["after"]["model"], "echo");
    assert!(!audit.to_string().contains("Hallo Playground"));
}

#[tokio::test]
async fn playground_allowed_in_read_only_mode() {
    let env = Env::start(true).await;
    env.seed().await;
    env.wait_key().await;
    env.send(
        Method::POST,
        "/playground/chat?key=ci",
        Some(chat_body("echo", false)),
        true,
    )
    .await
    .expect(StatusCode::OK);
    // Andere Schreibrouten bleiben gesperrt.
    env.send(
        Method::POST,
        "/organizations",
        Some(json!({"metadata": {"name": "x"}, "spec": {}})),
        true,
    )
    .await
    .expect(StatusCode::FORBIDDEN);
}
