// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Tool-Richtlinien und Tool-Inventar im Hot Path (AIGW-87): Entfernen/Ablehnen in beiden
//! Eingangsformaten und `/v1/responses`, `tool_choice`-Anpassung, unterbundene Tool-Aufrufe in
//! JSON und Stream, Zählen von Anfragen und Aufrufen im Inventar.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use aigw_core::ir::{
    ApiError, ChatRequest, ChatResponse, Content, ErrorKind, PartKind, StreamEvent, UpstreamError,
};
use aigw_core::resource::{
    ApiKey as KeyObject, ApiKeySpec, ApiKeyState, ApiKeyStatus, Object, ObjectMeta, Scope,
    ScopeLevel,
};
use aigw_core::snapshot::{KeyCtx, Snapshot};
use aigw_core::tools::{
    ToolAction, ToolBatch, ToolFlushResult, ToolInventorySink, ToolPolicy, ToolPolicyIndex,
    ToolPolicySpec,
};
use aigw_core::wire::sse::SseParser;
use aigw_proxy::mock::MockProvider;
use aigw_proxy::tools::ToolInventory;
use aigw_proxy::{
    ApiKey, AuthContext, Authenticator, BoxFuture, ChatProvider, Describe, EventStream, ProxyState,
    StaticRoutes,
};
use bytes::Bytes;
use futures_util::StreamExt as _;
use http_body_util::{BodyExt, Full};
use hyper::{HeaderMap, Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};

const KEY: &str = "sk-tools-test";

async fn start(state: ProxyState) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, aigw_proxy::router(state))
            .await
            .unwrap();
    });
    addr
}

struct Resp {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("kein JSON ({e}): {}", String::from_utf8_lossy(&self.body)))
    }
    fn events(&self) -> Vec<Value> {
        SseParser::new()
            .push(&self.body)
            .into_iter()
            .filter_map(|f| serde_json::from_str(&f.data).ok())
            .collect()
    }
}

async fn post(addr: SocketAddr, path: &str, body: Value) -> Resp {
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("http://{addr}{path}"))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {KEY}"))
        .header("x-api-key", KEY)
        .header("anthropic-version", "2023-06-01")
        .header("user-agent", "claude-cli/1.2.3 (test)")
        .body(Full::new(Bytes::from(serde_json::to_vec(&body).unwrap())))
        .unwrap();
    let res = client.request(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    Resp {
        status,
        headers,
        body,
    }
}

/// Mock-Anbieter, der die Anfrage festhält und Tool-Aufrufe optional umbenennt.
struct Capture {
    mock: Arc<MockProvider>,
    last: Mutex<Option<ChatRequest>>,
    rename: Option<&'static str>,
}

impl Capture {
    fn last(&self) -> ChatRequest {
        self.last.lock().unwrap().clone().expect("keine Anfrage")
    }
}

impl ChatProvider for Capture {
    fn describe(&self) -> Describe<'_> {
        self.mock.describe()
    }

    fn chat(self: Arc<Self>, req: ChatRequest) -> BoxFuture<Result<ChatResponse, UpstreamError>> {
        *self.last.lock().unwrap() = Some(req.clone());
        let rename = self.rename;
        let fut = Arc::clone(&self.mock).chat(req);
        Box::pin(async move {
            let mut resp = fut.await?;
            if let Some(n) = rename {
                for c in &mut resp.choices {
                    if let Content::Parts(parts) = &mut c.message.content {
                        for p in parts {
                            if let PartKind::ToolCall { name, .. } = &mut p.kind {
                                *name = n.to_owned();
                            }
                        }
                    }
                }
            }
            Ok(resp)
        })
    }

    fn chat_stream(
        self: Arc<Self>,
        req: ChatRequest,
    ) -> BoxFuture<Result<EventStream, UpstreamError>> {
        *self.last.lock().unwrap() = Some(req.clone());
        let rename = self.rename;
        let fut = Arc::clone(&self.mock).chat_stream(req);
        Box::pin(async move {
            let s = fut.await?;
            let s: EventStream = Box::pin(s.map(move |ev| match (ev, rename) {
                (StreamEvent::ToolCallStart(mut t), Some(n)) => {
                    t.name = n.to_owned();
                    StreamEvent::ToolCallStart(t)
                }
                (ev, _) => ev,
            }));
            Ok(s)
        })
    }
}

#[derive(Debug, Default)]
struct MemSink(Mutex<Vec<ToolBatch>>);

impl ToolInventorySink for MemSink {
    fn flush<'a>(
        &'a self,
        batch: ToolBatch,
    ) -> aigw_core::tools::BoxFuture<'a, Result<ToolFlushResult, String>> {
        let known = batch
            .items
            .iter()
            .map(|i| {
                (
                    i.key.clone(),
                    (i.description_hash.clone(), i.schema_hash.clone()),
                )
            })
            .collect();
        self.0.lock().unwrap().push(batch);
        Box::pin(async move {
            Ok(ToolFlushResult {
                known,
                events: Vec::new(),
            })
        })
    }
}

struct SnapAuth {
    snap: Arc<Snapshot>,
    key: Arc<KeyCtx>,
}

impl Authenticator for SnapAuth {
    fn authenticate(&self, key: Option<&ApiKey>) -> Result<AuthContext, ApiError> {
        if key.is_none_or(|k| k.secret != KEY) {
            return Err(ApiError::new(ErrorKind::Authentication, "unbekannt"));
        }
        Ok(AuthContext {
            key_id: Some("k".into()),
            key: Some(Arc::clone(&self.key)),
            snapshot: Some(Arc::clone(&self.snap)),
            workload: None,
        })
    }
}

fn policy(name: &str, tools: &[&str], action: ToolAction) -> ToolPolicy {
    Object {
        id: uuid::Uuid::now_v7(),
        metadata: ObjectMeta::named(name),
        spec: ToolPolicySpec {
            scope: Scope {
                level: ScopeLevel::Global,
                target: None,
            },
            tools: tools.iter().map(|s| (*s).to_owned()).collect(),
            servers: Vec::new(),
            action,
            unapproved: None,
            locked: false,
            enabled: true,
            description: None,
        },
    }
}

struct Env {
    addr: SocketAddr,
    cap: Arc<Capture>,
    sink: Arc<MemSink>,
    inv: Arc<ToolInventory>,
}

async fn env(policies: Vec<ToolPolicy>, rename: Option<&'static str>) -> Env {
    let key = Arc::new(KeyCtx {
        key: KeyObject {
            id: uuid::Uuid::now_v7(),
            metadata: ObjectMeta::named("app-key"),
            spec: ApiKeySpec::default(),
            status: ApiKeyStatus {
                prefix: "sk".into(),
                state: ApiKeyState::Active,
                last_used_at: None,
            },
        },
        team: None,
        organization: None,
        expires_at: None,
    });
    let mut snap = Snapshot {
        version: 1,
        ..Snapshot::default()
    };
    snap.model_groups.insert(
        "mock".into(),
        Arc::new(aigw_core::snapshot::ModelGroupCtx {
            group: Object {
                id: uuid::Uuid::now_v7(),
                metadata: ObjectMeta::named("mock"),
                spec: aigw_core::resource::ModelGroupSpec::default(),
            },
            deployments: vec![],
        }),
    );
    snap.tool_index = Arc::new(ToolPolicyIndex::new(policies.iter()));
    snap.tool_policies = policies.into_iter().map(Arc::new).collect();
    let cap = Arc::new(Capture {
        mock: Arc::new(MockProvider::new()),
        last: Mutex::new(None),
        rename,
    });
    let mut routes = StaticRoutes::new();
    routes.set_fallback(Arc::clone(&cap) as Arc<dyn ChatProvider>);
    let sink = Arc::new(MemSink::default());
    let inv = Arc::new(ToolInventory::new(
        Arc::clone(&sink) as Arc<dyn ToolInventorySink>,
        &aigw_core::config::ToolsConfig::default(),
    ));
    let state = ProxyState {
        auth: Arc::new(SnapAuth {
            snap: Arc::new(snap),
            key,
        }),
        tools: Arc::clone(&inv),
        ..ProxyState::with_routes(Arc::new(routes))
    };
    Env {
        addr: start(state).await,
        cap,
        sink,
        inv,
    }
}

fn oa_tool(name: &str) -> Value {
    json!({"type": "function", "function": {"name": name, "description": format!("Tool {name}"),
        "parameters": {"type": "object", "properties": {"echo": {"type": "string"}}}}})
}

fn an_tool(name: &str) -> Value {
    json!({"name": name, "description": format!("Tool {name}"),
        "input_schema": {"type": "object", "properties": {"echo": {"type": "string"}}}})
}

fn tool_names(req: &ChatRequest) -> Vec<String> {
    req.tools
        .iter()
        .map(|t| match &t.kind {
            aigw_core::ir::ToolKind::Function(f) => f.name.clone(),
            aigw_core::ir::ToolKind::Opaque { value, .. } => value["name"].to_string(),
        })
        .collect()
}

#[tokio::test]
async fn openai_strip_adjusts_tool_choice() {
    let e = env(
        vec![policy("no-delete", &["*delete*"], ToolAction::BlockStrip)],
        None,
    )
    .await;
    let r = post(
        e.addr,
        "/v1/chat/completions",
        json!({"model": "mock", "messages": [{"role": "user", "content": "hallo"}],
            "tools": [oa_tool("mcp__github__delete_repo"), oa_tool("calc")],
            "tool_choice": {"type": "function", "function": {"name": "mcp__github__delete_repo"}}}),
    )
    .await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    let up = e.cap.last();
    assert_eq!(tool_names(&up), vec!["calc".to_owned()]);
    assert_eq!(up.tool_choice, Some(aigw_core::ir::ToolChoice::Auto));
    // Alle Tools entfernt → tool_choice/parallel_tool_calls entfallen.
    let r = post(
        e.addr,
        "/v1/chat/completions",
        json!({"model": "mock", "messages": [{"role": "user", "content": "hallo"}],
            "tools": [oa_tool("delete_all")], "tool_choice": "required",
            "parallel_tool_calls": true}),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let up = e.cap.last();
    assert!(up.tools.is_empty());
    assert_eq!(up.tool_choice, None);
    assert_eq!(up.parallel_tool_calls, None);
}

#[tokio::test]
async fn anthropic_and_responses_reject() {
    let e = env(
        vec![policy("no-shell", &["shell"], ToolAction::BlockReject)],
        None,
    )
    .await;
    let r = post(
        e.addr,
        "/v1/messages",
        json!({"model": "mock", "max_tokens": 100,
            "messages": [{"role": "user", "content": "hallo"}],
            "tools": [an_tool("shell"), an_tool("calc")]}),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let v = r.json();
    assert_eq!(v["type"], "error", "{v}");
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(msg.contains("shell") && msg.contains("no-shell"), "{msg}");
    // OpenAI-Format mit Code.
    let r = post(
        e.addr,
        "/v1/chat/completions",
        json!({"model": "mock", "messages": [{"role": "user", "content": "x"}],
            "tools": [oa_tool("shell")]}),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.json()["error"]["code"], "tool_blocked");
    // Responses API.
    let r = post(
        e.addr,
        "/v1/responses",
        json!({"model": "mock", "input": "x",
            "tools": [{"type": "function", "name": "shell", "parameters": {"type": "object"}}]}),
    )
    .await;
    assert_eq!(
        r.status,
        StatusCode::FORBIDDEN,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
}

#[tokio::test]
async fn blocked_tool_calls_are_filtered_json_and_stream() {
    // Das Modell „ruft“ `secret` auf (umbenannt), obwohl nur `calc` gesendet wurde.
    let e = env(
        vec![policy("no-secret", &["secret"], ToolAction::BlockStrip)],
        Some("secret"),
    )
    .await;
    let body = |stream: bool| {
        json!({"model": "mock", "stream": stream, "messages": [{"role": "user", "content": "use the tool"}],
            "tools": [oa_tool("calc"), oa_tool("secret")]})
    };
    let r = post(e.addr, "/v1/chat/completions", body(false)).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers.get("x-aigw-guardrail").unwrap(), "tool-policy");
    let v = r.json();
    assert_eq!(v["choices"][0]["finish_reason"], "content_filter", "{v}");
    assert!(v["choices"][0]["message"]["tool_calls"].is_null(), "{v}");
    // Stream (OpenAI): kein Tool-Aufruf, Ende mit content_filter.
    let r = post(e.addr, "/v1/chat/completions", body(true)).await;
    let evs = r.events();
    assert!(
        evs.iter()
            .all(|v| v["choices"][0]["delta"]["tool_calls"].is_null())
    );
    assert!(
        evs.iter()
            .any(|v| v["choices"][0]["finish_reason"] == "content_filter"),
        "{evs:?}"
    );
    // Anthropic Stream: stop_reason refusal, kein tool_use-Block.
    let r = post(
        e.addr,
        "/v1/messages",
        json!({"model": "mock", "max_tokens": 100, "stream": true,
            "messages": [{"role": "user", "content": "use the tool"}],
            "tools": [an_tool("calc")]}),
    )
    .await;
    let evs = r.events();
    assert!(
        evs.iter().all(|v| v["content_block"]["type"] != "tool_use"),
        "{evs:?}"
    );
    assert!(
        evs.iter().any(|v| v["delta"]["stop_reason"] == "refusal"),
        "{evs:?}"
    );
    // Anthropic JSON.
    let r = post(
        e.addr,
        "/v1/messages",
        json!({"model": "mock", "max_tokens": 100,
            "messages": [{"role": "user", "content": "use the tool"}],
            "tools": [an_tool("calc")]}),
    )
    .await;
    assert_eq!(r.json()["stop_reason"], "refusal");
    e.inv.flush().await;
    let batches = e.sink.0.lock().unwrap();
    let calc = batches[0]
        .items
        .iter()
        .find(|i| &*i.key.name == "calc")
        .unwrap();
    assert_eq!(calc.requests, 4);
    let secret = batches[0]
        .items
        .iter()
        .find(|i| &*i.key.name == "secret")
        .unwrap();
    // Zwei Anfragen mit `secret` (entfernt) + vier unterbundene Aufrufe.
    assert_eq!((secret.requests, secret.calls), (2, 4));
    assert!(secret.blocked >= 6, "{secret:?}");
}

#[tokio::test]
async fn inventory_counts_calls_in_both_formats() {
    let e = env(Vec::new(), None).await;
    for stream in [false, true] {
        let r = post(
            e.addr,
            "/v1/chat/completions",
            json!({"model": "mock", "stream": stream,
                "messages": [{"role": "user", "content": "tool bitte"}],
                "tools": [oa_tool("mcp__github__search")]}),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK);
        let r = post(
            e.addr,
            "/v1/messages",
            json!({"model": "mock", "max_tokens": 100, "stream": stream,
                "messages": [{"role": "user", "content": "tool bitte"}],
                "tools": [an_tool("mcp__github__search")]}),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK);
    }
    e.inv.flush().await;
    let batches = e.sink.0.lock().unwrap();
    assert_eq!(batches.len(), 1);
    let it = &batches[0].items;
    assert_eq!(
        it.len(),
        1,
        "gleiches Tool in beiden Formaten = ein Eintrag"
    );
    let t = &it[0];
    assert_eq!((t.requests, t.calls, t.blocked), (4, 4, 0));
    assert_eq!(t.origin, "github");
    assert_eq!(t.client.as_deref(), Some("claude-cli/1.2.3 (test)"));
    assert!(t.definition.is_some());
}
