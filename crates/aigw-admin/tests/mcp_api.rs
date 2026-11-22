// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Integrationstests AIGW-41: MCP-Server und -Freigaben über die Admin-API (CRUD, ETag,
//! Prüfungen inkl. SSRF, Rollen), GitOps-Regeln und Ende-zu-Ende über Snapshot (Hot Reload,
//! entschlüsselte Zugangsdaten) bis zum MCP-Endpunkt der Datenebene.
//!
//! Läuft gegen SQLite und – mit `AIGW_TEST_POSTGRES_URL` bzw. `scripts/check.sh` mit
//! `AIGW_TEST_POSTGRES=docker` – gegen Postgres.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr,
    clippy::too_many_lines
)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aigw_admin::admin_keys::AdminKeyAuthenticator;
use aigw_admin::{AdminState, AuthChain, BootstrapToken, UpstreamPolicy};
use aigw_core::config::{AdminConfig, McpConfig};
use aigw_core::resource::new_id;
use aigw_core::snapshot::SnapshotHandle;
use aigw_proxy::{McpGateway, ProxyState, SnapshotAuthenticator};
use aigw_store::{Keyring, MasterKey, ReloadOptions, Reloader, ReloaderTask, Store};
use axum::response::IntoResponse;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{HeaderMap, Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};
use sqlx::AssertSqlSafe;

const UP_SECRET: &str = "mcp-upstream-geheimnis-0001";

struct Env {
    addr: SocketAddr,
    token: String,
    store: Store,
    keyring: Arc<Keyring>,
    snapshot: SnapshotHandle,
    reloader: ReloaderTask,
    fake: SocketAddr,
    fake_auth: Arc<Mutex<Vec<String>>>,
    _mcp: aigw_mcp::GatewayTask,
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
    fn detail(self, status: StatusCode) -> String {
        assert_eq!(self.status, status, "Antwort: {}", self.text());
        self.json()["detail"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
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
        .request(with_if_match(req).body(Full::new(body)).unwrap())
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

/// Minimaler MCP-Upstream (Streamable HTTP, nur JSON-Antworten).
async fn start_fake(auth: Arc<Mutex<Vec<String>>>) -> SocketAddr {
    let app = axum::Router::new().route(
        "/mcp",
        axum::routing::post(move |headers: HeaderMap, body: Bytes| {
            let auth = Arc::clone(&auth);
            async move {
                if let Some(a) = headers.get("authorization") {
                    auth.lock().unwrap().push(a.to_str().unwrap().to_owned());
                }
                let msg: Value = serde_json::from_slice(&body).unwrap();
                let Some(id) = msg.get("id").cloned() else {
                    return StatusCode::ACCEPTED.into_response();
                };
                let result = match msg["method"].as_str().unwrap() {
                    "initialize" => json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "fake", "version": "1"}}),
                    "tools/list" => json!({"tools": [{"name": "echo", "inputSchema": {"type": "object"}}, {"name": "drop_db", "inputSchema": {"type": "object"}}]}),
                    "tools/call" => json!({"content": [{"type": "text", "text": msg["params"]["arguments"]["text"].clone()}]}),
                    _ => json!({}),
                };
                let mut res = axum::Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response();
                res.headers_mut().insert("mcp-session-id", axum::http::HeaderValue::from_static("s1"));
                res
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
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
        let admin_cfg = AdminConfig {
            allow_insecure_upstreams: vec!["127.0.0.1".into()],
            allow_private_upstreams: vec!["127.0.0.1".into()],
            ..AdminConfig::default()
        };
        let policy = UpstreamPolicy::from_config(&admin_cfg).unwrap();
        let gateway = Arc::new(McpGateway::new(&McpConfig::default(), policy.clone()).unwrap());
        let task = gateway.spawn(snapshot.clone());
        let (token, secret) = BootstrapToken::generate().unwrap();
        let chain: Arc<dyn aigw_admin::AdminAuthenticator> = Arc::new(
            AuthChain::new()
                .bearer(Arc::new(AdminKeyAuthenticator::new(store.clone())))
                .bearer(Arc::new(token)),
        );
        let admin = aigw_admin::router_with(
            AdminState::new(store.clone(), Arc::clone(&chain))
                .with_keyring(Arc::clone(&keyring))
                .with_upstream_policy(policy),
        )
        .merge(aigw_admin::mcp_status::router(
            aigw_admin::mcp_status::McpStatusState {
                store: store.clone(),
                auth: chain,
                gateway: Some(Arc::clone(&gateway)),
            },
        ));
        let app = aigw_proxy::mcp::router(proxy, gateway).merge(admin);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let fake_auth = Arc::new(Mutex::new(Vec::new()));
        let fake = start_fake(Arc::clone(&fake_auth)).await;
        Self {
            addr,
            token: secret,
            store,
            keyring,
            snapshot,
            reloader: reloader.spawn(),
            fake,
            fake_auth,
            _mcp: task,
        }
    }

    async fn as_(&self, token: &str, method: Method, path: &str, body: Option<Value>) -> Resp {
        let auth = format!("Bearer {token}");
        let mut h = vec![("authorization", auth.as_str())];
        let tag;
        if let Some(Value::String(t)) = body.as_ref().and_then(|b| b.get("_ifMatch")) {
            tag = t.clone();
            h.push(("if-match", tag.as_str()));
        }
        let body = body.map(|mut b| {
            if let Some(o) = b.as_object_mut() {
                o.remove("_ifMatch");
            }
            b
        });
        let r = send(self.addr, method, &format!("/admin/api/v1{path}"), &h, body).await;
        assert!(
            !r.text().contains(UP_SECRET),
            "Geheimnis in Antwort: {}",
            r.text()
        );
        r
    }

    async fn admin(&self, method: Method, path: &str, body: Option<Value>) -> Resp {
        let t = self.token.clone();
        self.as_(&t, method, path, body).await
    }

    async fn mcp(&self, key: &str, session: Option<&str>, path: &str, body: Value) -> Resp {
        let auth = format!("Bearer {key}");
        let mut h = vec![
            ("authorization", auth.as_str()),
            ("accept", "application/json, text/event-stream"),
        ];
        if let Some(s) = session {
            h.push(("mcp-session-id", s));
        }
        send(self.addr, Method::POST, path, &h, Some(body)).await
    }

    /// Wartet, bis die Datenebene für `key` die Tools `want` zeigt (Hot Reload).
    async fn wait_tools(&self, key: &str, want: &[&str]) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let init = self
                .mcp(key, None, "/mcp", json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}))
                .await
                .expect(StatusCode::OK);
            let sid = init.header("mcp-session-id");
            let r = self
                .mcp(
                    key,
                    Some(&sid),
                    "/mcp",
                    json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
                )
                .await;
            let names: Vec<String> = r.json()["result"]["tools"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|t| t["name"].as_str().unwrap().to_owned())
                        .collect()
                })
                .unwrap_or_default();
            if names == want {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Tools {names:?} statt {want:?}: {}",
                r.text()
            );
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }
}

async fn scenario(store: Store) {
    let env = Env::start(store).await;
    send(
        env.addr,
        Method::GET,
        "/admin/api/v1/mcp-servers",
        &[],
        None,
    )
    .await
    .expect(StatusCode::UNAUTHORIZED);
    let fake_url = format!("http://{}/mcp", env.fake);

    // --- Grundlagen: Organisation, Team, Keys, Zugangsdaten ---
    env.admin(
        Method::POST,
        "/organizations",
        Some(json!({"metadata": {"name": "o1"}, "spec": {}})),
    )
    .await
    .expect(StatusCode::CREATED);
    env.admin(
        Method::POST,
        "/teams",
        Some(json!({"metadata": {"name": "t1"}, "spec": {"organization": "o1"}})),
    )
    .await
    .expect(StatusCode::CREATED);
    let key_solo = env
        .admin(
            Method::POST,
            "/api-keys",
            Some(json!({"metadata": {"name": "solo"}, "spec": {"organization": "o1"}})),
        )
        .await
        .expect(StatusCode::CREATED)
        .json()["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    let key_team = env
        .admin(
            Method::POST,
            "/api-keys",
            Some(json!({"metadata": {"name": "member"}, "spec": {"organization": "o1", "team": "t1"}})),
        )
        .await
        .expect(StatusCode::CREATED)
        .json()["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    env.admin(Method::POST, "/provider-credentials", Some(json!({"metadata": {"name": "mcp-cred"}, "spec": {"provider": "openai-compatible", "data": {"apiKey": {"value": UP_SECRET}}}})))
        .await
        .expect(StatusCode::CREATED);

    // --- Server: Prüfungen ---
    for (spec, want) in [
        (json!({"url": "https://169.254.169.254/mcp"}), "url:"),
        (json!({"url": "http://mcp.example.org/mcp"}), "http://"),
        (
            json!({"url": "https://10.0.0.1/mcp", "transport": "sse"}),
            "privat",
        ),
        (
            json!({"url": fake_url, "settings": {"headers": {"Authorization": "x"}}}),
            "reserviert",
        ),
        (
            json!({"url": fake_url, "settings": {"timeoutMS": 5}}),
            "settings",
        ),
        (
            json!({"url": "relative/cmd", "transport": "stdio"}),
            "absoluten Pfad",
        ),
    ] {
        let d = env
            .admin(
                Method::POST,
                "/mcp-servers",
                Some(json!({"metadata": {"name": "bad"}, "spec": spec})),
            )
            .await
            .detail(StatusCode::UNPROCESSABLE_ENTITY);
        assert!(d.contains(want), "{want} fehlt in {d}");
    }
    let d = env
        .admin(
            Method::POST,
            "/mcp-servers",
            Some(json!({"metadata": {"name": "a__b"}, "spec": {"url": fake_url}})),
        )
        .await
        .detail(StatusCode::UNPROCESSABLE_ENTITY);
    assert!(d.contains("__"), "{d}");
    env.admin(
        Method::POST,
        "/mcp-servers",
        Some(json!({"metadata": {"name": "x"}, "spec": {"url": fake_url, "credential": "nope"}})),
    )
    .await
    .expect(StatusCode::UNPROCESSABLE_ENTITY);
    // AIGW-90: Secret-Verweis muss existieren und das Token-Feld liefern (422 mit issues).
    env.admin(
        Method::POST,
        "/secrets",
        Some(
            json!({"metadata": {"name": "mcp-other"}, "spec": {"data": {"token": {"value": "t"}}}}),
        ),
    )
    .await
    .expect(StatusCode::CREATED);
    for (spec, needle) in [
        (
            json!({"url": fake_url, "secret": "fehlt"}),
            "existiert nicht",
        ),
        (
            json!({"url": fake_url, "secret": "mcp-other"}),
            "\"apiKey\"",
        ),
        (
            json!({"url": fake_url, "credential": "mcp-cred", "settings": {"credentialField": "token"}}),
            "\"token\"",
        ),
    ] {
        let r = env
            .admin(
                Method::POST,
                "/mcp-servers",
                Some(json!({"metadata": {"name": "x"}, "spec": spec})),
            )
            .await
            .expect(StatusCode::UNPROCESSABLE_ENTITY)
            .json();
        let issues = r["issues"]
            .as_array()
            .unwrap_or_else(|| panic!("issues fehlt: {r}"));
        assert_eq!(issues[0]["name"], "x", "{r}");
        let m = issues[0]["message"].as_str().unwrap();
        assert!(m.contains(needle), "{needle}: {m}");
    }

    // --- Server: CRUD ---
    let r = env
        .admin(Method::POST, "/mcp-servers", Some(json!({"metadata": {"name": "fake", "labels": {"env": "test"}}, "spec": {"url": fake_url, "credential": "mcp-cred", "settings": {"timeoutMs": 5000}}})))
        .await
        .expect(StatusCode::CREATED);
    assert_eq!(r.header("location"), "/admin/api/v1/mcp-servers/fake");
    let etag = r.header("etag");
    env.admin(
        Method::POST,
        "/mcp-servers",
        Some(json!({"metadata": {"name": "fake"}, "spec": {"url": fake_url}})),
    )
    .await
    .expect(StatusCode::CONFLICT);
    env.admin(Method::POST, "/mcp-servers", Some(json!({"metadata": {"name": "fs"}, "spec": {"url": "/usr/bin/mcp-fs", "transport": "stdio", "settings": {"args": ["--root", "/srv"]}}})))
        .await
        .expect(StatusCode::CREATED);
    let list = env
        .admin(Method::GET, "/mcp-servers?transport=stdio", None)
        .await
        .expect(StatusCode::OK)
        .json();
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    let list = env
        .admin(Method::GET, "/mcp-servers?q=127.0.0.1", None)
        .await
        .expect(StatusCode::OK)
        .json();
    assert_eq!(list["items"][0]["metadata"]["name"], "fake");
    let got = env
        .admin(Method::GET, "/mcp-servers/fake", None)
        .await
        .expect(StatusCode::OK);
    assert_eq!(got.header("etag"), etag);
    assert_eq!(
        got.json()["spec"]["transport"].as_str(),
        None,
        "Standard-Transport wird weggelassen"
    );
    // PUT mit falschem ETag → 412, richtig → 200
    env.admin(Method::PUT, "/mcp-servers/fake", Some(json!({"_ifMatch": "\"x\"", "metadata": {"name": "fake"}, "spec": {"url": fake_url, "credential": "mcp-cred"}})))
        .await
        .expect(StatusCode::PRECONDITION_FAILED);
    env.admin(Method::PUT, "/mcp-servers/fake", Some(json!({"_ifMatch": etag, "metadata": {"name": "fake"}, "spec": {"url": fake_url, "credential": "mcp-cred"}})))
        .await
        .expect(StatusCode::OK);
    env.admin(Method::DELETE, "/mcp-servers/fs", None)
        .await
        .expect(StatusCode::NO_CONTENT);
    env.admin(Method::GET, "/mcp-servers/fs", None)
        .await
        .expect(StatusCode::NOT_FOUND);
    // Zugangsdaten in Benutzung → 409
    env.admin(Method::DELETE, "/provider-credentials/mcp-cred", None)
        .await
        .expect(StatusCode::CONFLICT);

    // --- Freigaben: Prüfungen ---
    for (spec, status) in [
        (
            json!({"server": "nope", "subject": {"level": "api-key", "target": "solo"}}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            json!({"server": "fake", "subject": {"level": "team", "target": "nope"}}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            json!({"server": "fake", "subject": {"level": "api-key", "target": "nope"}}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            json!({"server": "fake", "subject": {"level": "organization", "target": "o1"}}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            json!({"server": "fake", "subject": {"level": "team", "target": "t1"}, "tools": ["bad tool"]}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        env.admin(
            Method::POST,
            "/mcp-grants",
            Some(json!({"metadata": {"name": "g-bad"}, "spec": spec})),
        )
        .await
        .expect(status);
    }

    // --- Ende zu Ende: ohne Freigabe nichts, mit Freigabe Tools (Hot Reload) ---
    env.wait_tools(&key_solo, &[]).await;
    let r = env
        .admin(Method::POST, "/mcp-grants", Some(json!({"metadata": {"name": "solo-fake"}, "spec": {"server": "fake", "subject": {"level": "api-key", "target": "solo"}, "tools": ["*", "!drop_*"]}})))
        .await
        .expect(StatusCode::CREATED);
    assert_eq!(r.header("location"), "/admin/api/v1/mcp-grants/solo-fake");
    env.wait_tools(&key_solo, &["fake__echo"]).await;
    // Snapshot trägt die entschlüsselten Zugangsdaten; der Upstream erhält das Token.
    let snap = env.snapshot.load();
    assert!(snap.mcp_servers["fake"].credential.is_some());
    let init = env.mcp(&key_solo, None, "/mcp/fake", json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}})).await.expect(StatusCode::OK);
    let sid = init.header("mcp-session-id");
    let r = env.mcp(&key_solo, Some(&sid), "/mcp/fake", json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "echo", "arguments": {"text": "hi"}}})).await;
    assert!(r.text().contains("\"hi\""), "{}", r.text());
    assert!(
        env.fake_auth
            .lock()
            .unwrap()
            .iter()
            .all(|a| a == &format!("Bearer {UP_SECRET}"))
    );
    let r = env.mcp(&key_solo, Some(&sid), "/mcp/fake", json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "drop_db"}})).await;
    assert!(r.text().contains("\"status\":403"), "{}", r.text());
    // Zustand der Upstreams (AIGW-41-Folge): fake ist verbunden.
    let st = env
        .admin(Method::GET, "/mcp-servers:status", None)
        .await
        .expect(StatusCode::OK)
        .json();
    assert_eq!(st["gatewayEnabled"], true, "{st}");
    let fake = st["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["name"] == "fake")
        .cloned()
        .unwrap();
    assert_eq!(fake["state"], "up", "{st}");
    assert_eq!(fake["failures"], 0);

    // Team-Key: Key-Freigabe allein genügt nicht, Team-Freigabe schon.
    env.admin(Method::POST, "/mcp-grants", Some(json!({"metadata": {"name": "member-fake"}, "spec": {"server": "fake", "subject": {"level": "api-key", "target": "member"}}})))
        .await
        .expect(StatusCode::CREATED);
    env.wait_tools(&key_team, &[]).await;
    env.admin(Method::POST, "/mcp-grants", Some(json!({"metadata": {"name": "t1-fake"}, "spec": {"server": "fake", "subject": {"level": "team", "target": "t1"}, "tools": ["echo"]}})))
        .await
        .expect(StatusCode::CREATED);
    env.wait_tools(&key_team, &["fake__echo"]).await;

    // Filter und Lesen.
    let l = env
        .admin(Method::GET, "/mcp-grants?team=t1", None)
        .await
        .expect(StatusCode::OK)
        .json();
    assert_eq!(l["items"].as_array().unwrap().len(), 1);
    let l = env
        .admin(Method::GET, "/mcp-grants?server=fake&limit=2", None)
        .await
        .expect(StatusCode::OK)
        .json();
    assert_eq!(l["items"].as_array().unwrap().len(), 2);
    assert!(l["nextCursor"].is_string());
    let g = env
        .admin(Method::GET, "/mcp-grants/t1-fake", None)
        .await
        .expect(StatusCode::OK);
    let gtag = g.header("etag");
    env.admin(Method::PUT, "/mcp-grants/t1-fake", Some(json!({"_ifMatch": gtag, "metadata": {"name": "t1-fake"}, "spec": {"server": "fake", "subject": {"level": "team", "target": "t1"}}})))
        .await
        .expect(StatusCode::OK);

    // --- Rollen: Team-Admin von t1 ---
    let ta = env
        .admin(Method::POST, "/admin-api-keys", Some(json!({"name": "ta", "roles": [{"role": "team-admin", "organization": "o1", "team": "t1"}]})))
        .await
        .expect(StatusCode::CREATED)
        .json()["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    // Katalog lesen ja, schreiben nein.
    env.as_(&ta, Method::GET, "/mcp-servers/fake", None)
        .await
        .expect(StatusCode::OK);
    env.as_(
        &ta,
        Method::POST,
        "/mcp-servers",
        Some(json!({"metadata": {"name": "y"}, "spec": {"url": fake_url}})),
    )
    .await
    .expect(StatusCode::FORBIDDEN);
    // Freigaben für Keys im eigenen Team ja, für das Team selbst nein, für fremde Keys nein.
    env.as_(&ta, Method::PUT, "/mcp-grants/member-fake", Some(json!({"metadata": {"name": "member-fake"}, "spec": {"server": "fake", "subject": {"level": "api-key", "target": "member"}, "tools": ["echo"]}})))
        .await
        .expect(StatusCode::OK);
    env.as_(&ta, Method::POST, "/mcp-grants", Some(json!({"metadata": {"name": "t1-more"}, "spec": {"server": "fake", "subject": {"level": "team", "target": "t1"}}})))
        .await
        .expect(StatusCode::FORBIDDEN);
    env.as_(&ta, Method::DELETE, "/mcp-grants/solo-fake", None)
        .await
        .expect(StatusCode::FORBIDDEN);
    let l = env
        .as_(&ta, Method::GET, "/mcp-grants", None)
        .await
        .expect(StatusCode::OK)
        .json();
    let names: Vec<&str> = l["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["metadata"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["member-fake", "t1-fake"],
        "nur der eigene Bereich"
    );

    // --- Löschen: Server mit Freigaben (Freigaben werden mitgelöscht) ---
    env.admin(Method::DELETE, "/mcp-grants/solo-fake", None)
        .await
        .expect(StatusCode::NO_CONTENT);
    env.wait_tools(&key_solo, &[]).await;
    env.admin(Method::DELETE, "/mcp-servers/fake", None)
        .await
        .expect(StatusCode::NO_CONTENT);
    let l = env
        .admin(Method::GET, "/mcp-grants", None)
        .await
        .expect(StatusCode::OK)
        .json();
    assert!(l["items"].as_array().unwrap().is_empty(), "{l}");

    gitops_rules(&env).await;
    env.reloader.shutdown().await;
}

async fn gitops_rules(env: &Env) {
    use aigw_store::gitops::{self, ApplyOptions, GitopsError, ProcessSecrets};
    let yaml = "\
apiVersion: aigw/v1alpha1
kind: McpServer
metadata: {name: meta}
spec: {url: 'https://169.254.169.254/mcp'}
---
apiVersion: aigw/v1alpha1
kind: McpServer
metadata: {name: a__b}
spec: {url: 'https://mcp.example.org/mcp'}
---
apiVersion: aigw/v1alpha1
kind: McpServer
metadata: {name: ok}
spec: {url: 'https://mcp.example.org/mcp', settings: {unknownKey: 1}}
---
apiVersion: aigw/v1alpha1
kind: McpServer
metadata: {name: ohne-feld}
spec: {url: 'https://mcp.example.org/mcp', secret: mcp-other}
---
apiVersion: aigw/v1alpha1
kind: McpGrant
metadata: {name: g}
spec: {server: ok, subject: {level: team, target: t1}, tools: ['a b']}
";
    let docs = gitops::parse_documents(yaml).unwrap();
    let opts = ApplyOptions {
        dry_run: true,
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
        "169.254.169.254",
        "__",
        "unknownKey",
        "ungültiges Muster",
        // AIGW-90: Secret ohne Token-Feld.
        "\"mcp-other\" enthält kein Feld \"apiKey\"",
    ] {
        assert!(text.contains(want), "{want} fehlt in:\n{text}");
    }
    // Gültiges Manifest wird angewendet.
    let yaml = "\
apiVersion: aigw/v1alpha1
kind: McpServer
metadata: {name: git-mcp}
spec: {url: 'https://mcp.example.org/mcp', settings: {timeoutMs: 1000}}
---
apiVersion: aigw/v1alpha1
kind: McpGrant
metadata: {name: git-grant}
spec: {server: git-mcp, subject: {level: team, target: t1}, tools: ['get_*', '!get_secret']}
";
    let docs = gitops::parse_documents(yaml).unwrap();
    gitops::plan(&env.store, &env.keyring, &docs, &opts, &ProcessSecrets)
        .await
        .unwrap();
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
    let db = format!("aigw_mcp_test_{}", new_id().simple());
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

/// F38: `If-Match` ist Pflicht – Anfragen ohne eigenen ETag schreiben hier mit `If-Match: *`
/// (eigene Tests prüfen 428/412 ausdrücklich).
fn with_if_match(req: axum::http::request::Builder) -> axum::http::request::Builder {
    use axum::http::Method as M;
    let action = req.uri_ref().is_some_and(|u| {
        let p = u.path();
        ["/revoke", "/suspend", "/resume", "/rotate"]
            .iter()
            .any(|s| p.ends_with(s))
    });
    let write = req
        .method_ref()
        .is_some_and(|m| matches!(*m, M::PUT | M::PATCH | M::DELETE) || (*m == M::POST && action));
    if write
        && !req
            .headers_ref()
            .is_some_and(|h| h.contains_key("if-match"))
    {
        req.header("if-match", "*")
    } else {
        req
    }
}
