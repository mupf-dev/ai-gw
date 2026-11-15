// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Integrationstests MCP-Gateway (AIGW-41) gegen lokale Fake-MCP-Upstreams (Streamable HTTP
//! mit JSON- und SSE-Antworten, älteres HTTP+SSE) mit eigenem Minimal-Client:
//! Aggregation/Namensraum, Pagination, Rechteprüfung (404 vs. 403), Sessions, Aushandlung,
//! Abbruch, `list_changed`, abgelaufene Upstream-Session, SSRF, Limits, Request-Log.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines
)]

use std::collections::HashSet;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aigw_core::config::{AdminConfig, McpConfig};
use aigw_core::observe::{McpOutcome, RequestObserver, RequestRecord};
use aigw_core::resource::{
    ApiKey, ApiKeySpec, ApiKeyState, ApiKeyStatus, LimitMetric, LimitSpec, LimitWindow, McpGrant,
    McpGrantSpec, McpServerSpec, McpTransport, Object, ObjectMeta, Scope, ScopeLevel, Team,
    TeamSpec, new_id,
};
use aigw_core::snapshot::{KeyCtx, McpServerCtx, Snapshot, SnapshotHandle};
use aigw_core::upstream::UpstreamPolicy;
use aigw_core::wire::WireFormat;
use aigw_proxy::auth::hash_key;
use aigw_proxy::{McpGateway, ProxyState, SnapshotAuthenticator};
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};

// ---------------------------------------------------------------------------------------------
// Fake-Upstream
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Fake {
    initializes: AtomicUsize,
    tool_lists: AtomicUsize,
    sessions: Mutex<HashSet<String>>,
    cancelled: Mutex<Vec<Value>>,
    calls: Mutex<Vec<Value>>,
    auth: Mutex<Vec<String>>,
    /// Benachrichtigungen an GET-Ströme (Streamable HTTP).
    push: Mutex<Option<broadcast::Sender<String>>>,
    /// Ausgehender Strom des älteren SSE-Transports.
    legacy: Mutex<Option<mpsc::UnboundedSender<String>>>,
}

fn tool(name: &str) -> Value {
    json!({"name": name, "description": format!("Tool {name}"), "inputSchema": {"type": "object"}})
}

/// Fachliche Antwort (ohne Transport). `Err(())` = Anfrage ohne Antwort (abgebrochen).
async fn answer(
    fake: &Fake,
    method: &str,
    params: &Value,
    progress: Option<&mpsc::UnboundedSender<Value>>,
) -> Result<Value, Value> {
    match method {
        "initialize" => {
            fake.initializes.fetch_add(1, Ordering::SeqCst);
            Ok(json!({
                "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": {"tools": {"listChanged": true}, "prompts": {}, "resources": {}},
                "serverInfo": {"name": "fake", "version": "1"},
            }))
        }
        "tools/list" => {
            fake.tool_lists.fetch_add(1, Ordering::SeqCst);
            if params.get("cursor").and_then(Value::as_str) == Some("p2") {
                Ok(json!({"tools": [tool("fail"), tool("delete_repo"), tool("get__odd")]}))
            } else {
                Ok(json!({"tools": [tool("echo"), tool("slow")], "nextCursor": "p2"}))
            }
        }
        "tools/call" => {
            fake.calls.lock().unwrap().push(params.clone());
            match params["name"].as_str().unwrap_or("") {
                "echo" | "get__odd" => Ok(
                    json!({"content": [{"type": "text", "text": params["arguments"]["text"].clone()}]}),
                ),
                "fail" => {
                    Ok(json!({"content": [{"type": "text", "text": "kaputt"}], "isError": true}))
                }
                "slow" => {
                    if let (Some(tx), Some(t)) = (progress, params.pointer("/_meta/progressToken"))
                    {
                        let _ = tx.send(json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progressToken": t, "progress": 1, "total": 2}}));
                    }
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok(json!({"content": []}))
                }
                other => Err(json!({"code": -32602, "message": format!("Unknown tool {other}")})),
            }
        }
        "prompts/list" => Ok(json!({"prompts": [{"name": "greet"}]})),
        "prompts/get" => Ok(
            json!({"messages": [{"role": "user", "content": {"type": "text", "text": format!("Hallo {}", params["name"])}}]}),
        ),
        "resources/list" => Ok(json!({"resources": [{"uri": "file:///a.txt", "name": "a"}]})),
        "resources/templates/list" => Ok(json!({"resourceTemplates": []})),
        "resources/read" => Ok(json!({"contents": [{"uri": params["uri"].clone(), "text": "A"}]})),
        "ping" => Ok(json!({})),
        other => Err(json!({"code": -32601, "message": format!("{other}?")})),
    }
}

fn rpc_reply(id: &Value, r: Result<Value, Value>) -> Value {
    match r {
        Ok(v) => json!({"jsonrpc": "2.0", "id": id, "result": v}),
        Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
    }
}

fn sse(data: &Value) -> Bytes {
    Bytes::from(format!("event: message\ndata: {data}\n\n"))
}

async fn fake_post(State(fake): State<Arc<Fake>>, headers: HeaderMap, body: Bytes) -> Response {
    let msg: Value = serde_json::from_slice(&body).unwrap();
    if let Some(a) = headers.get("authorization") {
        fake.auth
            .lock()
            .unwrap()
            .push(a.to_str().unwrap().to_owned());
    }
    let method = msg["method"].as_str().unwrap_or("").to_owned();
    if msg.get("id").is_none() {
        if method == "notifications/cancelled" {
            fake.cancelled.lock().unwrap().push(msg["params"].clone());
        }
        return StatusCode::ACCEPTED.into_response();
    }
    let id = msg["id"].clone();
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    if method == "initialize" {
        let sid = format!("up-{}", fake.initializes.load(Ordering::SeqCst) + 1);
        fake.sessions.lock().unwrap().insert(sid.clone());
        let r = answer(&fake, &method, &params, None).await;
        let mut res = axum::Json(rpc_reply(&id, r)).into_response();
        res.headers_mut()
            .insert("mcp-session-id", HeaderValue::from_str(&sid).unwrap());
        return res;
    }
    let sid = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !fake.sessions.lock().unwrap().contains(sid) {
        return StatusCode::NOT_FOUND.into_response();
    }
    assert!(
        headers.get("mcp-protocol-version").is_some(),
        "Protokoll-Header fehlt"
    );
    if method == "tools/call" {
        // Antwort als SSE-Strom (Fortschritt, dann Ergebnis).
        let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
        let fake2 = Arc::clone(&fake);
        tokio::spawn(async move {
            let r = answer(&fake2, &method, &params, Some(&tx)).await;
            let _ = tx.send(rpc_reply(&id, r));
        });
        let stream = futures_util::stream::unfold(rx_take(&mut rx), |mut rx| async move {
            rx.recv().await.map(|v| (Ok::<_, Infallible>(sse(&v)), rx))
        });
        let mut res = Response::new(Body::from_stream(stream));
        res.headers_mut().insert(
            "content-type",
            HeaderValue::from_static("text/event-stream"),
        );
        return res;
    }
    let r = answer(&fake, &method, &params, None).await;
    axum::Json(rpc_reply(&id, r)).into_response()
}

fn rx_take(rx: &mut mpsc::UnboundedReceiver<Value>) -> mpsc::UnboundedReceiver<Value> {
    let (_tx, empty) = mpsc::unbounded_channel();
    std::mem::replace(rx, empty)
}

async fn fake_get(State(fake): State<Arc<Fake>>) -> Response {
    let (tx, rx) = broadcast::channel::<String>(16);
    *fake.push.lock().unwrap() = Some(tx);
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.ok().map(|m| {
            (
                Ok::<_, Infallible>(Bytes::from(format!("data: {m}\n\n"))),
                rx,
            )
        })
    });
    let mut res = Response::new(Body::from_stream(stream));
    res.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("text/event-stream"),
    );
    res
}

/// Älterer Transport: `GET /sse` → `endpoint`, Antworten über den Strom.
async fn legacy_get(State(fake): State<Arc<Fake>>) -> Response {
    let (tx, rx) = mpsc::unbounded_channel::<String>();
    tx.send("event: endpoint\ndata: /messages?sid=1\n\n".into())
        .unwrap();
    *fake.legacy.lock().unwrap() = Some(tx);
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|m| (Ok::<_, Infallible>(Bytes::from(m)), rx))
    });
    let mut res = Response::new(Body::from_stream(stream));
    res.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("text/event-stream"),
    );
    res
}

async fn legacy_post(State(fake): State<Arc<Fake>>, body: Bytes) -> StatusCode {
    let msg: Value = serde_json::from_slice(&body).unwrap();
    if let Some(id) = msg.get("id").cloned() {
        let method = msg["method"].as_str().unwrap_or("").to_owned();
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        let fake2 = Arc::clone(&fake);
        tokio::spawn(async move {
            let r = answer(&fake2, &method, &params, None).await;
            let out = format!("event: message\ndata: {}\n\n", rpc_reply(&id, r));
            if let Some(tx) = fake2.legacy.lock().unwrap().as_ref() {
                let _ = tx.send(out);
            }
        });
    }
    StatusCode::ACCEPTED
}

async fn start_fake() -> (SocketAddr, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    let app = axum::Router::new()
        .route(
            "/mcp",
            axum::routing::post(fake_post)
                .get(fake_get)
                .delete(|| async { StatusCode::OK }),
        )
        .route("/sse", axum::routing::get(legacy_get))
        .route("/messages", axum::routing::post(legacy_post))
        .with_state(Arc::clone(&fake));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, fake)
}

// ---------------------------------------------------------------------------------------------
// Gateway mit Snapshot
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Capture(Mutex<Vec<RequestRecord>>);

impl RequestObserver for Capture {
    fn record(&self, record: RequestRecord) {
        self.0.lock().unwrap().push(record);
    }
}

fn obj<S>(name: &str, spec: S) -> Object<S> {
    Object {
        id: new_id(),
        metadata: ObjectMeta::named(name),
        spec,
    }
}

fn server(name: &str, url: String, transport: McpTransport) -> (String, Arc<McpServerCtx>) {
    let credential = Some(Arc::new(aigw_core::resource::CredentialData(
        [(
            "apiKey".to_owned(),
            aigw_core::resource::SecretString::new("up-secret"),
        )]
        .into(),
    )));
    (
        name.to_owned(),
        Arc::new(McpServerCtx {
            server: obj(
                name,
                McpServerSpec {
                    secret: None,
                    url,
                    transport,
                    credential: Some("c".into()),
                    enabled: true,
                    settings: Default::default(),
                },
            ),
            credential,
        }),
    )
}

fn grant(server: &str, level: ScopeLevel, target: &str, tools: &[&str]) -> Arc<McpGrant> {
    Arc::new(obj(
        &format!("{server}-{target}"),
        McpGrantSpec {
            server: server.into(),
            subject: Scope {
                level,
                target: Some(target.into()),
            },
            tools: tools.iter().map(|t| (*t).to_owned()).collect(),
        },
    ))
}

fn key(name: &str, team: Option<&Arc<Team>>, limits: Vec<LimitSpec>) -> Arc<KeyCtx> {
    Arc::new(KeyCtx {
        key: ApiKey {
            id: new_id(),
            metadata: ObjectMeta::named(name),
            spec: ApiKeySpec {
                team: team.map(|t| t.metadata.name.clone()),
                limits,
                ..ApiKeySpec::default()
            },
            status: ApiKeyStatus {
                prefix: "sk-aigw-x".into(),
                state: ApiKeyState::Active,
                last_used_at: None,
            },
        },
        team: team.cloned(),
        organization: None,
        expires_at: None,
    })
}

struct Env {
    addr: SocketAddr,
    fake: Arc<Fake>,
    records: Arc<Capture>,
    snapshot: SnapshotHandle,
    _task: aigw_mcp::GatewayTask,
}

const KEY_A: &str = "sk-aigw-key-a";
const KEY_B: &str = "sk-aigw-key-b";
const KEY_LIMITED: &str = "sk-aigw-key-limited";
const KEY_NONE: &str = "sk-aigw-key-none";

fn build_snapshot(fake: SocketAddr) -> Snapshot {
    let mut snap = Snapshot::default();
    let team: Arc<Team> = Arc::new(obj(
        "t1",
        TeamSpec {
            organization: "o".into(),
            ..TeamSpec::default()
        },
    ));
    let a = key("key-a", None, vec![]);
    let b = key("key-b", Some(&team), vec![]);
    let limited = key(
        "key-limited",
        None,
        vec![LimitSpec {
            metric: LimitMetric::Requests,
            limit: 3,
            window: LimitWindow::Minute,
            model_group: None,
        }],
    );
    let none = key("key-none", None, vec![]);
    for (secret, k) in [
        (KEY_A, a),
        (KEY_B, b),
        (KEY_LIMITED, limited),
        (KEY_NONE, none),
    ] {
        snap.keys.insert(hash_key(secret), k);
    }
    let base = format!("http://{fake}");
    for (n, s) in [
        server("gh", format!("{base}/mcp"), McpTransport::StreamableHttp),
        server("legacy", format!("{base}/sse"), McpTransport::Sse),
        server("other", format!("{base}/mcp"), McpTransport::StreamableHttp),
        // Am Admin vorbei eingetragen (z. B. direkt in der DB): Cloud-Metadaten.
        server(
            "meta",
            "http://169.254.169.254/mcp".into(),
            McpTransport::StreamableHttp,
        ),
        server("tool", "/bin/true".into(), McpTransport::Stdio),
    ] {
        snap.mcp_servers.insert(n, s);
    }
    snap.mcp_grants = vec![
        grant("gh", ScopeLevel::ApiKey, "key-a", &["*", "!delete_*"]),
        grant("legacy", ScopeLevel::ApiKey, "key-a", &[]),
        grant("meta", ScopeLevel::ApiKey, "key-a", &[]),
        grant("tool", ScopeLevel::ApiKey, "key-a", &[]),
        grant("gh", ScopeLevel::Team, "t1", &["echo"]),
        grant("gh", ScopeLevel::ApiKey, "key-limited", &[]),
    ];
    snap
}

async fn start() -> Env {
    start_with(McpConfig {
        call_timeout_secs: 20,
        ..McpConfig::default()
    })
    .await
}

async fn start_with(cfg: McpConfig) -> Env {
    let (fake_addr, fake) = start_fake().await;
    let snapshot = SnapshotHandle::new(build_snapshot(fake_addr));
    let records = Arc::new(Capture::default());
    let state = ProxyState {
        auth: Arc::new(SnapshotAuthenticator::new(snapshot.clone())),
        observer: Arc::clone(&records) as Arc<dyn RequestObserver>,
        ..ProxyState::mock()
    };
    let policy = UpstreamPolicy::from_config(&AdminConfig {
        allow_insecure_upstreams: vec!["127.0.0.1".into()],
        allow_private_upstreams: vec!["127.0.0.1".into()],
        ..AdminConfig::default()
    })
    .unwrap();
    let gateway = Arc::new(McpGateway::new(&cfg, policy).unwrap());
    let task = gateway.spawn(snapshot.clone());
    let app = aigw_proxy::router(state.clone()).merge(aigw_proxy::mcp::router(state, gateway));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Env {
        addr,
        fake,
        records,
        snapshot,
        _task: task,
    }
}

// ---------------------------------------------------------------------------------------------
// Minimaler MCP-Client (Streamable HTTP)
// ---------------------------------------------------------------------------------------------

struct Resp {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Resp {
    /// JSON-RPC-Nachrichten (JSON-Body oder alle SSE-Ereignisse).
    fn messages(&self) -> Vec<Value> {
        let ct = self
            .headers
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default();
        if ct.starts_with("text/event-stream") {
            String::from_utf8_lossy(&self.body)
                .split("\n\n")
                .filter_map(|ev| ev.lines().find_map(|l| l.strip_prefix("data: ")))
                .map(|d| serde_json::from_str(d).unwrap())
                .collect()
        } else if self.body.is_empty() {
            vec![]
        } else {
            vec![serde_json::from_slice(&self.body).unwrap()]
        }
    }

    fn last(&self) -> Value {
        self.messages()
            .pop()
            .unwrap_or_else(|| panic!("keine Nachricht, Status {}", self.status))
    }
}

struct McpClient {
    addr: SocketAddr,
    path: String,
    key: Option<String>,
    session: Option<String>,
    next: i64,
}

impl McpClient {
    fn new(env: &Env, path: &str, key: Option<&str>) -> Self {
        Self {
            addr: env.addr,
            path: path.to_owned(),
            key: key.map(str::to_owned),
            session: None,
            next: 1,
        }
    }

    async fn send(
        &self,
        method: hyper::Method,
        body: Option<Value>,
        extra: &[(&str, &str)],
    ) -> Resp {
        let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
        let mut req = hyper::Request::builder()
            .method(method)
            .uri(format!("http://{}{}", self.addr, self.path))
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json");
        if let Some(k) = &self.key {
            req = req.header("authorization", format!("Bearer {k}"));
        }
        if let Some(s) = &self.session {
            req = req.header("mcp-session-id", s);
        }
        for (k, v) in extra {
            req = req.header(*k, *v);
        }
        let body = body
            .map(|b| Bytes::from(serde_json::to_vec(&b).unwrap()))
            .unwrap_or_default();
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

    async fn initialize_with(&mut self, version: &str) -> Resp {
        let r = self
            .send(
                hyper::Method::POST,
                Some(json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
                    "protocolVersion": version, "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}})),
                &[],
            )
            .await;
        if r.status == StatusCode::OK {
            self.session = r
                .headers
                .get("mcp-session-id")
                .map(|v| v.to_str().unwrap().to_owned());
            let n = self
                .send(
                    hyper::Method::POST,
                    Some(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})),
                    &[],
                )
                .await;
            assert_eq!(n.status, StatusCode::ACCEPTED);
        }
        r
    }

    async fn initialize(&mut self) {
        let r = self.initialize_with("2025-06-18").await;
        assert_eq!(
            r.status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&r.body)
        );
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        let r = self.request(method, params).await;
        assert_eq!(
            r.status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&r.body)
        );
        r.last()
    }

    async fn request(&mut self, method: &str, params: Value) -> Resp {
        self.next += 1;
        let id = self.next;
        self.send(
            hyper::Method::POST,
            Some(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})),
            &[("mcp-protocol-version", "2025-06-18")],
        )
        .await
    }

    async fn tool_names(&mut self) -> Vec<String> {
        let r = self.call("tools/list", json!({})).await;
        r["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect()
    }
}

fn err_status(v: &Value) -> u64 {
    v["error"]["data"]["status"]
        .as_u64()
        .unwrap_or_else(|| panic!("kein Fehler mit Status: {v}"))
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aggregation_namespaces_and_rights() {
    let env = start().await;
    let mut c = McpClient::new(&env, "/mcp", Some(KEY_A));
    let init = c.initialize_with("2025-06-18").await;
    assert_eq!(init.last()["result"]["protocolVersion"], "2025-06-18");
    assert!(
        init.last()["result"]["capabilities"]["tools"]["listChanged"]
            .as_bool()
            .unwrap()
    );
    assert!(c.session.as_ref().unwrap().len() >= 32);

    // Pagination des Upstreams vollständig gelesen, Namensraum, Ausschluss `!delete_*`;
    // `meta` (SSRF) und `tool` (stdio aus) fallen im Aggregat heraus.
    let names = c.tool_names().await;
    for want in [
        "gh__echo",
        "gh__slow",
        "gh__fail",
        "gh__get__odd",
        "legacy__echo",
    ] {
        assert!(names.contains(&want.to_owned()), "{want} fehlt: {names:?}");
    }
    assert!(!names.contains(&"gh__delete_repo".to_owned()), "{names:?}");
    assert!(
        names.contains(&"legacy__delete_repo".to_owned()),
        "Ausschluss gilt nur für gh"
    );
    assert!(
        !names
            .iter()
            .any(|n| n.starts_with("other__") || n.starts_with("meta__"))
    );

    // Aufruf über den Namensraum; Upstream erhält den Originalnamen und das Token.
    let r = c
        .call(
            "tools/call",
            json!({"name": "gh__echo", "arguments": {"text": "hallo"}}),
        )
        .await;
    assert_eq!(r["result"]["content"][0]["text"], "hallo", "{r}");
    let r = c
        .call(
            "tools/call",
            json!({"name": "gh__get__odd", "arguments": {"text": "x"}}),
        )
        .await;
    assert_eq!(
        r["result"]["content"][0]["text"], "x",
        "Tool-Name mit __: {r}"
    );
    assert_eq!(env.fake.calls.lock().unwrap()[0]["name"], "echo");
    assert!(
        env.fake
            .auth
            .lock()
            .unwrap()
            .iter()
            .all(|a| a == "Bearer up-secret")
    );
    // Tool meldet Fehler → Ergebnis mit isError (kein JSON-RPC-Fehler).
    let r = c.call("tools/call", json!({"name": "gh__fail"})).await;
    assert_eq!(r["result"]["isError"], true);
    // Älterer HTTP+SSE-Transport.
    let r = c
        .call(
            "tools/call",
            json!({"name": "legacy__echo", "arguments": {"text": "alt"}}),
        )
        .await;
    assert_eq!(r["result"]["content"][0]["text"], "alt", "{r}");

    // Rechte: unbekannt 404, nicht freigegeben 403.
    let r = c.call("tools/call", json!({"name": "gh__nope"})).await;
    assert_eq!(err_status(&r), 404, "{r}");
    assert_eq!(r["error"]["code"], -32602);
    let r = c
        .call("tools/call", json!({"name": "gh__delete_repo"}))
        .await;
    assert_eq!(err_status(&r), 403, "{r}");
    assert_eq!(r["error"]["data"]["code"], "tool_not_allowed");
    let r = c.call("tools/call", json!({"name": "unknown__echo"})).await;
    assert_eq!(err_status(&r), 404);
    let r = c.call("tools/call", json!({"name": "other__echo"})).await;
    assert_eq!(err_status(&r), 403);
    assert_eq!(r["error"]["data"]["code"], "server_not_allowed");

    // SSRF: am Admin vorbei eingetragener Server wird nicht verbunden.
    let r = c.call("tools/call", json!({"name": "meta__x"})).await;
    assert_eq!(err_status(&r), 502, "{r}");
    assert!(
        r["error"]["message"].as_str().unwrap().contains("gesperrt"),
        "{r}"
    );
    // stdio ohne Freischaltung.
    let r = c.call("tools/call", json!({"name": "tool__x"})).await;
    assert!(
        r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("nicht freigeschaltet"),
        "{r}"
    );

    // Prompts und Ressourcen.
    let r = c.call("prompts/list", json!({})).await;
    let prompts: Vec<&str> = r["result"]["prompts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert!(
        prompts.contains(&"gh__greet") && prompts.contains(&"legacy__greet"),
        "{prompts:?}"
    );
    let r = c.call("prompts/get", json!({"name": "gh__greet"})).await;
    assert_eq!(
        r["result"]["messages"][0]["content"]["text"],
        "Hallo \"greet\""
    );
    let r = c.call("resources/list", json!({})).await;
    assert_eq!(r["result"]["resources"].as_array().unwrap().len(), 2, "{r}");
    let r = c
        .call("resources/read", json!({"uri": "file:///a.txt"}))
        .await;
    assert_eq!(r["result"]["contents"][0]["text"], "A", "{r}");
    let r = c.call("ping", json!({})).await;
    assert_eq!(r["result"], json!({}));
    let r = c.call("sampling/createMessage", json!({})).await;
    assert_eq!(r["error"]["code"], -32601);

    // Request-Log: je tools/call ein Eintrag.
    let recs = env.records.0.lock().unwrap().clone();
    let mcp: Vec<_> = recs
        .iter()
        .filter(|r| r.ingress == WireFormat::Mcp)
        .collect();
    assert_eq!(
        mcp.len(),
        recs.len(),
        "nur Tool-Aufrufe werden protokolliert"
    );
    let outcome = |server: &str, tool: &str| {
        mcp.iter()
            .find(|r| {
                r.mcp
                    .as_ref()
                    .is_some_and(|m| m.server == server && m.tool == tool)
            })
            .map(|r| (r.mcp.as_ref().unwrap().outcome, r.status))
            .unwrap_or_else(|| panic!("{server}/{tool} fehlt"))
    };
    assert_eq!(outcome("gh", "echo"), (McpOutcome::Ok, 200));
    assert_eq!(outcome("gh", "fail"), (McpOutcome::ToolError, 200));
    assert_eq!(outcome("gh", "nope"), (McpOutcome::NotFound, 404));
    assert_eq!(outcome("gh", "delete_repo"), (McpOutcome::Denied, 403));
    assert_eq!(outcome("other", "echo"), (McpOutcome::Denied, 403));
    assert_eq!(outcome("meta", "x"), (McpOutcome::UpstreamError, 502));
    let echo = mcp
        .iter()
        .find(|r| r.mcp.as_ref().unwrap().tool == "echo")
        .unwrap();
    assert_eq!(echo.model_alias.as_deref(), Some("gh__echo"));
    assert!(echo.api_key_id.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_server_endpoint_and_team_ceiling() {
    let env = start().await;
    // Einzel-Endpunkt: Namen wie beim Upstream.
    let mut c = McpClient::new(&env, "/mcp/gh", Some(KEY_A));
    c.initialize().await;
    let names = c.tool_names().await;
    assert_eq!(names, vec!["echo", "slow", "fail", "get__odd"]);
    let r = c
        .call(
            "tools/call",
            json!({"name": "echo", "arguments": {"text": "e"}}),
        )
        .await;
    assert_eq!(r["result"]["content"][0]["text"], "e");

    // Unbekannter Server 404, nicht freigegebener 403 (HTTP-Ebene).
    let mut u = McpClient::new(&env, "/mcp/unknown", Some(KEY_A));
    assert_eq!(
        u.initialize_with("2025-06-18").await.status,
        StatusCode::NOT_FOUND
    );
    let mut o = McpClient::new(&env, "/mcp/other", Some(KEY_A));
    assert_eq!(
        o.initialize_with("2025-06-18").await.status,
        StatusCode::FORBIDDEN
    );
    // Ohne Key 401, Key ohne Freigaben sieht nichts.
    let mut n = McpClient::new(&env, "/mcp", None);
    let r = n.initialize_with("2025-06-18").await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert!(r.headers.get("www-authenticate").is_some());
    let mut none = McpClient::new(&env, "/mcp", Some(KEY_NONE));
    none.initialize().await;
    assert!(none.tool_names().await.is_empty());

    // Key im Team: Team-Freigabe (`echo`) ist Obergrenze.
    let mut b = McpClient::new(&env, "/mcp", Some(KEY_B));
    b.initialize().await;
    assert_eq!(b.tool_names().await, vec!["gh__echo"]);
    let r = b.call("tools/call", json!({"name": "gh__slow"})).await;
    assert_eq!(err_status(&r), 403);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_and_versions() {
    let env = start().await;
    let mut c = McpClient::new(&env, "/mcp", Some(KEY_A));
    // Unbekannte Version → neueste unterstützte.
    let r = c.initialize_with("1999-01-01").await;
    assert_eq!(
        r.last()["result"]["protocolVersion"],
        aigw_mcp::protocol::LATEST_VERSION
    );
    // Ungültiger Versions-Header → 400.
    let r = c
        .send(
            hyper::Method::POST,
            Some(json!({"jsonrpc": "2.0", "id": 5, "method": "ping"})),
            &[("mcp-protocol-version", "1999-01-01")],
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    // Ohne Session → 400, fremde/unbekannte Session → 404.
    let sid = c.session.clone();
    c.session = None;
    assert_eq!(
        c.request("ping", json!({})).await.status,
        StatusCode::BAD_REQUEST
    );
    c.session = Some("abc".into());
    assert_eq!(
        c.request("ping", json!({})).await.status,
        StatusCode::NOT_FOUND
    );
    let mut other = McpClient::new(&env, "/mcp", Some(KEY_B));
    other.session.clone_from(&sid);
    assert_eq!(
        other.request("ping", json!({})).await.status,
        StatusCode::NOT_FOUND,
        "Session an Key gebunden"
    );
    let mut wrong_scope = McpClient::new(&env, "/mcp/gh", Some(KEY_A));
    wrong_scope.session.clone_from(&sid);
    assert_eq!(
        wrong_scope.request("ping", json!({})).await.status,
        StatusCode::NOT_FOUND,
        "Session an Endpunkt gebunden"
    );
    // Ungültiges JSON / Batch.
    c.session.clone_from(&sid);
    let r = c
        .send(
            hyper::Method::POST,
            Some(json!([{"jsonrpc": "2.0", "id": 1, "method": "ping"}])),
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.last()["error"]["code"], -32600);
    // DELETE beendet die Session.
    assert_eq!(
        c.send(hyper::Method::DELETE, None, &[]).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        c.request("ping", json!({})).await.status,
        StatusCode::NOT_FOUND
    );

    // Abgelaufene Upstream-Session: Gateway verbindet neu (transparent).
    let mut c = McpClient::new(&env, "/mcp/gh", Some(KEY_A));
    c.initialize().await;
    let r = c
        .call(
            "tools/call",
            json!({"name": "echo", "arguments": {"text": "1"}}),
        )
        .await;
    assert_eq!(r["result"]["content"][0]["text"], "1");
    let before = env.fake.initializes.load(Ordering::SeqCst);
    env.fake.sessions.lock().unwrap().clear();
    let r = c
        .call(
            "tools/call",
            json!({"name": "echo", "arguments": {"text": "2"}}),
        )
        .await;
    assert_eq!(r["result"]["content"][0]["text"], "2", "{r}");
    assert_eq!(env.fake.initializes.load(Ordering::SeqCst), before + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_progress_and_list_changed() {
    let env = start().await;
    let mut c = McpClient::new(&env, "/mcp", Some(KEY_A));
    c.initialize().await;
    c.tool_names().await;
    let lists_before = env.fake.tool_lists.load(Ordering::SeqCst);

    // GET-Strom der Gateway-Session öffnen.
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let get = hyper::Request::builder()
        .method("GET")
        .uri(format!("http://{}/mcp", env.addr))
        .header("accept", "text/event-stream")
        .header("authorization", format!("Bearer {KEY_A}"))
        .header("mcp-session-id", c.session.clone().unwrap())
        .body(Full::new(Bytes::new()))
        .unwrap();
    let res = client.request(get).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let mut events = res.into_body();

    // Upstream meldet eine geänderte Tool-Liste über seinen GET-Strom.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(tx) = env.fake.push.lock().unwrap().as_ref() {
            let _ = tx.send(
                json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}).to_string(),
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Gateway öffnet keinen GET-Strom zum Upstream"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut seen = String::new();
    while !seen.contains("notifications/tools/list_changed") {
        let frame = tokio::time::timeout(Duration::from_secs(10), events.frame())
            .await
            .expect("kein list_changed")
            .unwrap()
            .unwrap();
        if let Ok(d) = frame.into_data() {
            seen.push_str(&String::from_utf8_lossy(&d));
        }
    }
    // Cache geleert: nächste Liste fragt den Upstream erneut.
    c.tool_names().await;
    assert!(env.fake.tool_lists.load(Ordering::SeqCst) > lists_before);

    // Langer Aufruf mit Fortschritt (SSE) und Abbruch durch den Client.
    let addr = env.addr;
    let sid = c.session.clone().unwrap();
    let call = tokio::spawn(async move {
        let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
        let req = hyper::Request::builder()
            .method("POST")
            .uri(format!("http://{addr}/mcp"))
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {KEY_A}"))
            .header("mcp-session-id", sid)
            .body(Full::new(Bytes::from(
                json!({"jsonrpc": "2.0", "id": "long", "method": "tools/call", "params": {"name": "gh__slow", "_meta": {"progressToken": "mein-token"}}}).to_string(),
            )))
            .unwrap();
        let res = client.request(req).await.unwrap();
        assert_eq!(res.headers()["content-type"], "text/event-stream");
        res.into_body().collect().await.unwrap().to_bytes()
    });
    // Warten, bis der Upstream den Aufruf hat.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !env
        .fake
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|c| c["name"] == "slow")
    {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let upstream_token = env
        .fake
        .calls
        .lock()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "slow")
        .unwrap()["_meta"]["progressToken"]
        .clone();
    assert!(
        upstream_token.as_str().unwrap().starts_with("aigw-"),
        "Token umgeschrieben: {upstream_token}"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    let r = c
        .send(hyper::Method::POST, Some(json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": "long"}})), &[])
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    let body = tokio::time::timeout(Duration::from_secs(10), call)
        .await
        .expect("Abbruch beendet den Strom nicht")
        .unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("notifications/progress") && text.contains("mein-token"),
        "Fortschritt mit Client-Token: {text}"
    );
    assert!(
        !text.contains("\"result\""),
        "keine Antwort nach Abbruch: {text}"
    );
    // Upstream erhält notifications/cancelled.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while env.fake.cancelled.lock().unwrap().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "kein Abbruch beim Upstream"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let found = env.records.0.lock().unwrap().iter().any(|r| {
            r.mcp
                .as_ref()
                .is_some_and(|m| m.tool == "slow" && m.outcome == McpOutcome::Cancelled)
        });
        if found {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Abbruch nicht protokolliert"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Konfigurationsänderung → list_changed an die Session.
    let mut snap = (*env.snapshot.load()).clone();
    snap.version += 1;
    snap.mcp_grants.retain(|g| g.spec.server != "legacy");
    env.snapshot.store(snap);
    let mut seen = String::new();
    while !seen.contains("notifications/prompts/list_changed") {
        let frame = tokio::time::timeout(Duration::from_secs(10), events.frame())
            .await
            .expect("kein list_changed nach Konfigurationsänderung")
            .unwrap()
            .unwrap();
        if let Ok(d) = frame.into_data() {
            seen.push_str(&String::from_utf8_lossy(&d));
        }
    }
    assert!(
        !c.tool_names()
            .await
            .iter()
            .any(|n| n.starts_with("legacy__"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn key_limits_apply_to_tool_calls() {
    let env = start().await;
    let mut c = McpClient::new(&env, "/mcp", Some(KEY_LIMITED));
    // S-28: initialize und Listen zählen mit (Limit 3 Anfragen je Minute).
    c.initialize().await;
    assert_eq!(c.tool_names().await.len(), 5);
    let r = c
        .call(
            "tools/call",
            json!({"name": "gh__echo", "arguments": {"text": "1"}}),
        )
        .await;
    assert!(r.get("result").is_some(), "{r}");
    let r = c
        .request(
            "tools/call",
            json!({"name": "gh__echo", "arguments": {"text": "2"}}),
        )
        .await;
    assert!(r.headers.get("retry-after").is_some());
    let v = r.last();
    assert_eq!(v["error"]["code"], -32004, "{v}");
    assert_eq!(err_status(&v), 429);
    assert!(env.records.0.lock().unwrap().iter().any(|r| {
        r.mcp
            .as_ref()
            .is_some_and(|m| m.outcome == McpOutcome::RateLimited)
            && r.status == 429
    }));
    // Ressourcen und Listen sind ebenfalls begrenzt.
    for m in ["resources/list", "tools/list", "prompts/list"] {
        let r = c.request(m, json!({})).await;
        assert!(r.headers.get("retry-after").is_some(), "{m}");
        assert_eq!(err_status(&r.last()), 429, "{m}");
    }
    // ping bleibt frei (Keep-Alive).
    let r = c.call("ping", json!({})).await;
    assert!(r.get("result").is_some(), "{r}");
    // Neue Session: initialize wird abgelehnt.
    let mut d = McpClient::new(&env, "/mcp", Some(KEY_LIMITED));
    let r = d.initialize_with("2025-06-18").await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(r.headers.get("mcp-session-id").is_none());
    assert!(r.headers.get("retry-after").is_some());
    assert_eq!(err_status(&r.last()), 429);
}

/// S-28: Sessions je Key und Team begrenzt; andere Keys sind nicht betroffen; DELETE gibt frei.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_limited_per_key_and_team() {
    let env = start_with(McpConfig {
        max_sessions_per_key: 2,
        max_sessions_per_team: 1,
        ..McpConfig::default()
    })
    .await;
    let mut a1 = McpClient::new(&env, "/mcp", Some(KEY_A));
    a1.initialize().await;
    let mut a2 = McpClient::new(&env, "/mcp/gh", Some(KEY_A));
    a2.initialize().await;
    let mut a3 = McpClient::new(&env, "/mcp", Some(KEY_A));
    let r = a3.initialize_with("2025-06-18").await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.last()["error"]["data"]["code"], "too_many_sessions");
    // KEY_B (Team t1, Grenze 1) ist unabhängig von KEY_A.
    let mut b1 = McpClient::new(&env, "/mcp", Some(KEY_B));
    b1.initialize().await;
    let mut b2 = McpClient::new(&env, "/mcp", Some(KEY_B));
    let r = b2.initialize_with("2025-06-18").await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(
        r.last()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Team")
    );
    // Beenden gibt einen Platz frei.
    assert_eq!(
        a1.send(hyper::Method::DELETE, None, &[]).await.status,
        StatusCode::NO_CONTENT
    );
    a3.initialize().await;
}
