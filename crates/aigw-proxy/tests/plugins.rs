// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Plugin-Kette im Hot Path (AIGW-36): Reject/Respond in beiden Eingangsformaten,
//! eingebaute Plugins (`header`, `request-transform`), Stream-Haken, gepufferter Modus,
//! Ketten je Key aus dem Snapshot.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use aigw_core::ir::{
    ApiError, ChatRequest, ChatResponse, Choice, Content, ErrorKind, Message, Role, StopReason,
    StreamEvent, UpstreamError,
};
use aigw_core::resource::{
    ApiKey as KeyObject, ApiKeySpec, ApiKeyState, ApiKeyStatus, JsonObject, Object, ObjectMeta,
    PluginBindingSpec, Scope, ScopeLevel,
};
use aigw_core::snapshot::{KeyCtx, Snapshot};
use aigw_core::wire::sse::{SseEvent, SseParser};
use aigw_plugins::{
    BUFFERED_HEADER, Chain, ChunkCtx, ChunkFlow, Entry, Flow, Hook, Phases, Plugin, PluginChains,
    PluginError, PluginFactory, ReqCtx, RespCtx, builtin,
};
use aigw_proxy::mock::MockProvider;
use aigw_proxy::{
    ApiKey, AuthContext, Authenticator, BoxFuture, ChatProvider, Describe, EventStream, ProxyState,
    StaticRoutes,
};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{HeaderMap, Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------------------------
// Hilfen
// ---------------------------------------------------------------------------------------------

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

    fn sse(&self) -> Vec<SseEvent> {
        SseParser::new().push(&self.body)
    }

    /// Zusammengesetzter Text eines SSE-Streams (OpenAI oder Anthropic).
    fn sse_text(&self) -> String {
        let mut out = String::new();
        for f in self.sse() {
            let Ok(v) = serde_json::from_str::<Value>(&f.data) else {
                continue;
            };
            if let Some(t) = v["choices"][0]["delta"]["content"].as_str() {
                out.push_str(t);
            }
            if v["type"] == "content_block_delta"
                && let Some(t) = v["delta"]["text"].as_str()
            {
                out.push_str(t);
            }
        }
        out
    }
}

async fn post(addr: SocketAddr, path: &str, headers: &[(&str, &str)], body: Value) -> Resp {
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let mut req = Request::builder()
        .method(Method::POST)
        .uri(format!("http://{addr}{path}"))
        .header("content-type", "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let res = client
        .request(
            req.body(Full::new(Bytes::from(serde_json::to_vec(&body).unwrap())))
                .unwrap(),
        )
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

fn openai(text: &str, stream: bool) -> Value {
    json!({"model": "mock", "stream": stream, "max_tokens": 4000, "temperature": 1.5,
           "messages": [{"role": "user", "content": text}]})
}

fn anthropic(text: &str, stream: bool) -> Value {
    json!({"model": "mock", "stream": stream, "max_tokens": 4000,
           "messages": [{"role": "user", "content": text}]})
}

fn state_with(chain: Chain) -> ProxyState {
    ProxyState {
        plugins: Arc::new(PluginChains::fixed(chain)),
        ..ProxyState::mock()
    }
}

fn entry(name: &str, p: impl Plugin) -> Arc<Entry> {
    Arc::new(Entry::new(name, Arc::new(p)))
}

/// Mock-Anbieter, der die zuletzt gesendete Anfrage festhält.
struct Capture {
    mock: Arc<MockProvider>,
    last: Mutex<Option<ChatRequest>>,
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
        Arc::clone(&self.mock).chat(req)
    }

    fn chat_stream(
        self: Arc<Self>,
        req: ChatRequest,
    ) -> BoxFuture<Result<EventStream, UpstreamError>> {
        *self.last.lock().unwrap() = Some(req.clone());
        Arc::clone(&self.mock).chat_stream(req)
    }
}

fn capture_state(chains: PluginChains) -> (ProxyState, Arc<Capture>) {
    let cap = Arc::new(Capture {
        mock: Arc::new(MockProvider::new()),
        last: Mutex::new(None),
    });
    let mut routes = StaticRoutes::new();
    routes.set_fallback(Arc::clone(&cap) as Arc<dyn ChatProvider>);
    let state = ProxyState {
        plugins: Arc::new(chains),
        ..ProxyState::with_routes(Arc::new(routes))
    };
    (state, cap)
}

// ---------------------------------------------------------------------------------------------
// Test-Plugins
// ---------------------------------------------------------------------------------------------

/// Lehnt Anfragen mit „verboten“ ab.
struct Deny;

impl Plugin for Deny {
    fn name(&self) -> &'static str {
        "deny"
    }
    fn phases(&self) -> Phases {
        Phases::REQUEST
    }
    fn on_request<'a>(&'a self, ctx: &'a mut ReqCtx<'_>) -> Hook<'a, Flow> {
        let hit = ctx
            .request
            .last_user_text()
            .is_some_and(|t| t.contains("verboten"));
        Hook::ok(if hit {
            Flow::Reject(
                ApiError::new(ErrorKind::PermissionDenied, "Inhalt nicht erlaubt")
                    .with_code("content_blocked"),
            )
        } else {
            Flow::Continue
        })
    }
}

/// Beantwortet „ping“ selbst (asynchron, wie ein Cache/Webhook).
struct Pong;

fn canned(text: &str) -> ChatResponse {
    ChatResponse {
        id: "plugin-1".into(),
        model: "mock".into(),
        created: Some(1),
        choices: vec![Choice {
            index: 0,
            message: Message {
                role: Role::Assistant,
                content: Content::Text(text.into()),
                name: None,
                extra: Default::default(),
            },
            stop_reason: Some(StopReason::EndTurn),
            stop_sequence: None,
            extra: Default::default(),
        }],
        usage: None,
        extra: Default::default(),
        source: None,
    }
}

impl Plugin for Pong {
    fn name(&self) -> &'static str {
        "pong"
    }
    fn phases(&self) -> Phases {
        Phases::ALL
    }
    fn on_request<'a>(&'a self, ctx: &'a mut ReqCtx<'_>) -> Hook<'a, Flow> {
        Hook::future(async move {
            tokio::task::yield_now().await;
            if ctx.request.last_user_text().as_deref() == Some("ping") {
                return Ok(Flow::Respond(Box::new(canned("pong"))));
            }
            ctx.meta.insert("seen".into(), json!(true));
            Ok(Flow::Continue)
        })
    }
    fn on_response<'a>(&'a self, ctx: &'a mut RespCtx<'_>) -> Hook<'a, Flow> {
        // Meta aus der Anfrage-Phase ist da.
        assert_eq!(ctx.meta.get("seen"), Some(&json!(true)));
        Hook::ok(Flow::Continue)
    }
}

/// Großbuchstaben im Stream und in der Antwort; Antwort-Ablehnung bei „STOPP“.
struct Upper {
    buffered: bool,
}

fn upper_response(resp: &mut ChatResponse) {
    for c in &mut resp.choices {
        c.message.content = Content::Text(format!("[geprüft] {}", c.message.content.text()));
    }
}

impl Plugin for Upper {
    fn name(&self) -> &'static str {
        "upper"
    }
    fn phases(&self) -> Phases {
        Phases::OUTPUT
    }
    fn needs_buffered_output(&self) -> bool {
        self.buffered
    }
    fn on_response<'a>(&'a self, ctx: &'a mut RespCtx<'_>) -> Hook<'a, Flow> {
        if ctx.response.choices[0]
            .message
            .content
            .text()
            .contains("STOPP")
        {
            return Hook::ok(Flow::Reject(
                ApiError::new(ErrorKind::InvalidRequest, "Antwort blockiert")
                    .with_code("output_blocked"),
            ));
        }
        upper_response(&mut ctx.response);
        Hook::ok(Flow::Continue)
    }
    fn on_chunk(
        &self,
        _ctx: &mut ChunkCtx<'_>,
        ev: &mut StreamEvent,
    ) -> Result<ChunkFlow, PluginError> {
        if let StreamEvent::TextDelta { text, .. } = ev {
            *text = text.to_uppercase();
        }
        Ok(ChunkFlow::Continue)
    }
}

// ---------------------------------------------------------------------------------------------
// Reject / Respond
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn reject_in_both_ingress_formats() {
    let addr = start(state_with(Chain::new(vec![entry("deny", Deny)]))).await;
    let r = post(
        addr,
        "/v1/chat/completions",
        &[],
        openai("das ist verboten", false),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.json()["error"]["code"], "content_blocked");
    let r = post(
        addr,
        "/v1/messages",
        &[],
        anthropic("das ist verboten", true),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.json()["type"], "error");
    assert_eq!(r.json()["error"]["type"], "permission_error");
    // Erlaubtes geht durch.
    let r = post(addr, "/v1/messages", &[], anthropic("hallo", false)).await;
    assert_eq!(r.status, StatusCode::OK);
}

#[tokio::test]
async fn respond_in_both_ingress_formats_and_streams() {
    let addr = start(state_with(Chain::new(vec![entry("pong", Pong)]))).await;
    let r = post(addr, "/v1/chat/completions", &[], openai("ping", false)).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["choices"][0]["message"]["content"], "pong");
    assert!(r.headers.get("x-aigw-deployment").is_none());

    let r = post(addr, "/v1/messages", &[], anthropic("ping", false)).await;
    assert_eq!(r.json()["content"][0]["text"], "pong");

    let r = post(addr, "/v1/chat/completions", &[], openai("ping", true)).await;
    assert_eq!(r.sse_text(), "pong");
    assert_eq!(r.sse().last().unwrap().data, "[DONE]");
    let r = post(addr, "/v1/messages", &[], anthropic("ping", true)).await;
    assert_eq!(r.sse_text(), "pong");
    assert!(
        r.sse()
            .iter()
            .any(|f| f.event.as_deref() == Some("message_stop"))
    );

    // Sonst normal zum Upstream; Meta erreicht die Antwort-Phase.
    let r = post(addr, "/v1/chat/completions", &[], openai("hallo", false)).await;
    assert_eq!(r.json()["choices"][0]["message"]["content"], "Echo: hallo");
}

// ---------------------------------------------------------------------------------------------
// Eingebaute Plugins
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn header_and_request_transform_plugins() {
    let header = builtin::header::Factory
        .build(&json!({
            "request": {"forward": ["x-request-source"], "set": {"x-tenant": "acme"}},
            "response": {"set": {"x-policy": "strict"}, "remove": ["x-aigw-deployment"]}
        }))
        .unwrap();
    let transform = builtin::request_transform::Factory
        .build(&json!({"caps": {"maxTokens": 256, "temperature": 1.0}, "defaults": {"topP": 0.5}}))
        .unwrap();
    let chain = Chain::new(vec![
        Arc::new(Entry::new("h", header)),
        Arc::new(Entry::new("t", transform)),
    ]);
    let (state, cap) = capture_state(PluginChains::fixed(chain));
    let addr = start(state).await;

    for (path, body) in [
        ("/v1/chat/completions", openai("hallo", false)),
        ("/v1/messages", anthropic("hallo", true)),
    ] {
        let r = post(
            addr,
            path,
            &[
                ("x-request-source", "ci"),
                ("x-not-forwarded", "1"),
                ("authorization", "Bearer sk-geheim"),
            ],
            body,
        )
        .await;
        assert_eq!(r.status, StatusCode::OK, "{path}");
        assert_eq!(r.headers["x-policy"], "strict");
        assert!(r.headers.get("x-aigw-deployment").is_none());
        assert!(r.headers.get("x-aigw-request-id").is_some());

        let req = cap.last();
        assert_eq!(req.max_tokens, Some(256));
        assert_eq!(req.sampling.top_p, Some(0.5));
        if path == "/v1/chat/completions" {
            assert_eq!(req.sampling.temperature, Some(1.0));
        }
        let mut h = req.hints.upstream_headers.clone();
        h.sort();
        assert_eq!(
            h,
            [
                ("x-request-source".to_owned(), "ci".to_owned()),
                ("x-tenant".to_owned(), "acme".to_owned())
            ]
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Stream-Haken und gepufferter Modus
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn chunk_plugins_rewrite_live_streams() {
    let addr = start(state_with(Chain::new(vec![entry(
        "upper",
        Upper { buffered: false },
    )])))
    .await;
    let r = post(
        addr,
        "/v1/chat/completions",
        &[],
        openai("hallo welt", true),
    )
    .await;
    assert!(r.headers.get(BUFFERED_HEADER).is_none());
    assert_eq!(r.sse_text(), "ECHO: HALLO WELT");
    let r = post(addr, "/v1/messages", &[], anthropic("hallo welt", true)).await;
    assert_eq!(r.sse_text(), "ECHO: HALLO WELT");
    // Nicht gestreamt: Antwort-Haken.
    let r = post(addr, "/v1/chat/completions", &[], openai("hallo", false)).await;
    assert_eq!(
        r.json()["choices"][0]["message"]["content"],
        "[geprüft] Echo: hallo"
    );
    // Antwort-Haken lehnt ab.
    let r = post(addr, "/v1/messages", &[], anthropic("STOPP", false)).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["error"]["type"], "invalid_request_error");
}

#[tokio::test]
async fn buffered_mode_runs_response_hooks_on_whole_answer() {
    let addr = start(state_with(Chain::new(vec![entry(
        "upper",
        Upper { buffered: true },
    )])))
    .await;
    for (path, body) in [
        ("/v1/chat/completions", openai("hallo welt", true)),
        ("/v1/messages", anthropic("hallo welt", true)),
    ] {
        let r = post(addr, path, &[], body).await;
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(r.headers[BUFFERED_HEADER], "plugins");
        assert!(
            r.headers["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/event-stream")
        );
        // on_response (nicht on_chunk) hat die ganze Antwort gesehen.
        assert_eq!(r.sse_text(), "[geprüft] Echo: hallo welt", "{path}");
    }
    // Ablehnung auf der ganzen Antwort kommt vor dem ersten Byte als normaler Fehler.
    let r = post(addr, "/v1/chat/completions", &[], openai("STOPP", true)).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["error"]["code"], "output_blocked");
}

// ---------------------------------------------------------------------------------------------
// Ketten je Key aus dem Snapshot
// ---------------------------------------------------------------------------------------------

/// Authentifiziert `sk-a`/`sk-b` gegen einen festen Snapshot.
struct SnapAuth {
    snap: Arc<Snapshot>,
    keys: Vec<(String, Arc<KeyCtx>)>,
}

impl Authenticator for SnapAuth {
    fn authenticate(&self, key: Option<&ApiKey>) -> Result<AuthContext, ApiError> {
        let k = key
            .and_then(|k| self.keys.iter().find(|(s, _)| *s == k.secret))
            .ok_or_else(|| ApiError::new(ErrorKind::Authentication, "unbekannt"))?;
        Ok(AuthContext {
            key_id: Some(k.0.clone()),
            key: Some(Arc::clone(&k.1)),
            snapshot: Some(Arc::clone(&self.snap)),
        })
    }
}

fn key_ctx(name: &str) -> Arc<KeyCtx> {
    Arc::new(KeyCtx {
        key: KeyObject {
            id: uuid::Uuid::now_v7(),
            metadata: ObjectMeta::named(name),
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
    })
}

#[tokio::test]
async fn chains_follow_key_bindings_from_snapshot() {
    let a = key_ctx("key-a");
    let b = key_ctx("key-b");
    let config: JsonObject = match json!({"response": {"set": {"x-key": "a"}}}) {
        Value::Object(m) => m,
        _ => unreachable!(),
    };
    let mut snap = Snapshot {
        version: 5,
        ..Snapshot::default()
    };
    // Modellgruppe „mock“ (für die Modellfreigabe; geroutet wird über den Mock).
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
    snap.keys
        .insert(aigw_core::snapshot::KeyHash([1; 32]), Arc::clone(&a));
    snap.keys
        .insert(aigw_core::snapshot::KeyHash([2; 32]), Arc::clone(&b));
    snap.plugin_bindings.push(Arc::new(Object {
        id: uuid::Uuid::now_v7(),
        metadata: ObjectMeta::named("hdr-a"),
        spec: PluginBindingSpec {
            inherit: None,
            locked: false,
            fail_mode: None,
            timeout_ms: None,
            output_reject_mode: None,
            scope: Scope {
                level: ScopeLevel::ApiKey,
                target: Some("key-a".into()),
            },
            plugin: "header".into(),
            order: 0,
            enabled: true,
            config,
        },
    }));
    let state = ProxyState {
        auth: Arc::new(SnapAuth {
            snap: Arc::new(snap),
            keys: vec![("sk-a".into(), a), ("sk-b".into(), b)],
        }),
        ..ProxyState::mock()
    };
    let addr = start(state).await;
    let r = post(
        addr,
        "/v1/chat/completions",
        &[("authorization", "Bearer sk-a")],
        openai("x", false),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers["x-key"], "a");
    let r = post(
        addr,
        "/v1/chat/completions",
        &[("authorization", "Bearer sk-b")],
        openai("x", false),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.headers.get("x-key").is_none());
}

// ---------------------------------------------------------------------------------------------
// Upstream-Header über einen echten Adapter (OpenAI, lokaler Fake-Upstream)
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn plugin_headers_reach_the_upstream_but_not_credentials() {
    use aigw_core::resource::{CredentialData, Deployment, DeploymentSpec, SecretString};
    use aigw_core::snapshot::DeploymentCtx;
    use aigw_core::types::ProviderKind;
    use aigw_providers::{Registry, Upstream, UpstreamClient};

    let seen: Arc<Mutex<Vec<HeaderMap>>> = Arc::default();
    let s = Arc::clone(&seen);
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |headers: HeaderMap| {
            let s = Arc::clone(&s);
            async move {
                s.lock().unwrap().push(headers);
                (
                    [("content-type", "application/json")],
                    json!({"id": "c1", "object": "chat.completion", "created": 1, "model": "up",
                           "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"},
                                        "finish_reason": "stop"}]})
                    .to_string(),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let dep = DeploymentCtx {
        deployment: Deployment {
            id: uuid::Uuid::now_v7(),
            metadata: ObjectMeta::named("dep"),
            spec: DeploymentSpec {
                model_group: "mock".into(),
                provider: ProviderKind::OpenAi,
                endpoint: Some(format!("http://{up}/v1")),
                provider_model: "up".into(),
                credential: Some("c".into()),
                weight: 1,
                priority: 0,
                capacity: Default::default(),
                enabled: true,
                settings: JsonObject::new(),
            },
        },
        credential: Some(Arc::new(CredentialData(std::collections::BTreeMap::from(
            [("apiKey".to_owned(), SecretString::new("sk-upstream"))],
        )))),
    };
    let upstream = Upstream::from_deployment(
        &dep,
        &Registry::with_builtin(),
        &UpstreamClient::unguarded().unwrap(),
    )
    .unwrap();
    let mut routes = StaticRoutes::new();
    routes.set_fallback(Arc::new(upstream));
    let header = builtin::header::Factory
        .build(&json!({"request": {"forward": ["x-request-source"], "set": {"x-tenant": "acme"}}}))
        .unwrap();
    let state = ProxyState {
        plugins: Arc::new(PluginChains::fixed(Chain::new(vec![Arc::new(Entry::new(
            "h", header,
        ))]))),
        ..ProxyState::with_routes(Arc::new(routes))
    };
    let addr = start(state).await;
    let r = post(
        addr,
        "/v1/chat/completions",
        &[
            ("x-request-source", "ci"),
            ("authorization", "Bearer sk-client"),
        ],
        openai("hallo", false),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let h = seen.lock().unwrap().pop().unwrap();
    assert_eq!(h["x-tenant"], "acme");
    assert_eq!(h["x-request-source"], "ci");
    // Der Upstream sieht nur den Anbieter-Key, nie den Gateway-Key des Clients.
    assert_eq!(h["authorization"], "Bearer sk-upstream");
    assert_eq!(h.get_all("authorization").iter().count(), 1);
}
