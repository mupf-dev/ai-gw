// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Ende-zu-Ende-Tests Gateway → Fake-Upstreams für die Adapter Anthropic (AIGW-16),
//! Ollama/vLLM (AIGW-17) und Mistral (AIGW-20): nicht gestreamt, gestreamt, Tools, Fehler und
//! Kreuzfälle (OpenAI-Client → Anthropic-Ziel, Anthropic-Client → OpenAI-ähnliches Ziel).
//! Keine externen Aufrufe, keine echten Schlüssel.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aigw_core::observe::{PayloadMode, RequestObserver, RequestRecord};
use aigw_core::resource::{
    CredentialData, Deployment, DeploymentSpec, JsonObject, ObjectMeta, SecretString, new_id,
};
use aigw_core::snapshot::DeploymentCtx;
use aigw_core::types::ProviderKind;
use aigw_core::wire::sse::{SseEvent, SseParser};
use aigw_providers::{Registry, Upstream, UpstreamClient, with_group_defaults};
use aigw_proxy::{ProxyState, StaticRoutes};
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------------------------
// Fake-Upstreams
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Seen {
    uri: String,
    headers: HeaderMap,
    body: Value,
}

#[derive(Default)]
struct Fake {
    seen: Mutex<Vec<Seen>>,
}

fn record(fake: &Fake, uri: &Uri, headers: &HeaderMap, body: &Bytes) -> Value {
    let body: Value = serde_json::from_slice(body).unwrap();
    fake.seen.lock().unwrap().push(Seen {
        uri: uri.to_string(),
        headers: headers.clone(),
        body: body.clone(),
    });
    body
}

fn sse_body(frames: Vec<String>) -> Response {
    let s = futures_util::stream::iter(
        frames
            .into_iter()
            .map(|f| Ok::<_, std::io::Error>(Bytes::from(f))),
    );
    (
        [("content-type", "text/event-stream")],
        Body::from_stream(s),
    )
        .into_response()
}

fn error(status: u16, headers: &[(&'static str, &'static str)], body: Value) -> Response {
    let mut res = (StatusCode::from_u16(status).unwrap(), body.to_string()).into_response();
    for (k, v) in headers {
        res.headers_mut().insert(*k, v.parse().unwrap());
    }
    res
}

fn json_body(v: Value) -> Response {
    ([("content-type", "application/json")], v.to_string()).into_response()
}

fn ev(name: &str, data: Value) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

fn anthropic_usage() -> Value {
    json!({"input_tokens": 10, "output_tokens": 1,
           "cache_creation_input_tokens": 20, "cache_read_input_tokens": 30})
}

/// Anthropic Messages API (`/v1/messages`).
async fn anthropic_handler(
    State(fake): State<Arc<Fake>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let body = record(&fake, &uri, &headers, &body);
    let model = body["model"].as_str().unwrap_or_default().to_owned();
    let stream = body["stream"].as_bool().unwrap_or(false);
    let tools = body.get("tools").is_some();
    match model.as_str() {
        "claude-overloaded" => {
            return error(
                529,
                &[],
                json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
            );
        }
        "claude-ratelimit" => {
            return error(
                429,
                &[("retry-after", "4")],
                json!({"type": "error", "error": {"type": "rate_limit_error", "message": "rate limited"}}),
            );
        }
        "claude-midstream" => {
            return sse_body(vec![
                ev(
                    "message_start",
                    json!({"type": "message_start", "message": {"id": "msg_e", "type": "message",
                           "role": "assistant", "model": "claude", "content": [],
                           "usage": {"input_tokens": 3, "output_tokens": 0}}}),
                ),
                ev(
                    "error",
                    json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
                ),
            ]);
        }
        _ => {}
    }
    if !stream {
        let content = if tools {
            json!([{"type": "text", "text": "Ich schaue nach."},
                   {"type": "tool_use", "id": "toolu_01A", "name": "wetter", "input": {"ort": "Berlin"}}])
        } else {
            json!([{"type": "text", "text": "Hallo Welt"}])
        };
        return json_body(json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-up",
            "content": content,
            "stop_reason": if tools { "tool_use" } else { "end_turn" },
            "stop_sequence": null,
            "usage": anthropic_usage()
        }));
    }
    let mut frames = vec![
        ev(
            "message_start",
            json!({"type": "message_start", "message": {"id": "msg_s", "type": "message",
                   "role": "assistant", "model": "claude-up", "content": [], "stop_reason": null,
                   "stop_sequence": null, "usage": anthropic_usage()}}),
        ),
        ev("ping", json!({"type": "ping"})),
        ev(
            "content_block_start",
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        ),
        ev(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hallo"}}),
        ),
        ev(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": " Welt"}}),
        ),
        ev(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ),
    ];
    if tools {
        frames.extend([
            ev(
                "content_block_start",
                json!({"type": "content_block_start", "index": 1, "content_block":
                       {"type": "tool_use", "id": "toolu_01B", "name": "wetter", "input": {}}}),
            ),
            ev(
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 1, "delta":
                       {"type": "input_json_delta", "partial_json": "{\"ort\":"}}),
            ),
            ev(
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 1, "delta":
                       {"type": "input_json_delta", "partial_json": "\"Berlin\"}"}}),
            ),
            ev(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 1}),
            ),
        ]);
    }
    frames.extend([
        ev("ping", json!({"type": "ping"})),
        ev(
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": if tools { "tool_use" } else { "end_turn" },
                   "stop_sequence": null}, "usage": {"output_tokens": 5}}),
        ),
        ev("message_stop", json!({"type": "message_stop"})),
    ]);
    sse_body(frames)
}

fn chunk(model: &str, delta: Value, finish: Option<&str>, usage: Option<Value>) -> String {
    let mut c = json!({"id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1,
                       "model": model, "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]});
    if let Some(u) = usage {
        c["usage"] = u;
    }
    format!("data: {c}\n\n")
}

/// OpenAI-kompatible API (Ollama, vLLM, Mistral) unter `/v1/chat/completions`.
async fn compat_handler(
    State(fake): State<Arc<Fake>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let body = record(&fake, &uri, &headers, &body);
    let model = body["model"].as_str().unwrap_or_default().to_owned();
    let stream = body["stream"].as_bool().unwrap_or(false);
    let usage = json!({"prompt_tokens": 9, "completion_tokens": 4, "total_tokens": 13});
    match model.as_str() {
        "mistral-422" => {
            return error(
                422,
                &[],
                json!({"detail": [{"type": "extra_forbidden", "loc": ["body", "foo"], "msg": "Extra inputs are not permitted"}]}),
            );
        }
        "mistral-429" => {
            return error(
                429,
                &[("retry-after", "2")],
                json!({"object": "error", "message": "Requests rate limit exceeded", "type": "rate_limited"}),
            );
        }
        "ollama-missing" => return error(404, &[], json!({"error": "model 'x' not found"})),
        _ => {}
    }
    let reasoning = model.starts_with("vllm");
    let tools = body.get("tools").is_some();
    if !stream {
        let mut message = json!({"role": "assistant", "content": "Hallo Welt"});
        if reasoning {
            message["reasoning_content"] = json!("erst denken");
        }
        if tools {
            message = json!({"role": "assistant", "content": null, "tool_calls": [
                {"id": "AbC123xYz", "type": "function", "function": {"name": "wetter", "arguments": "{\"ort\":\"Berlin\"}"}}]});
        }
        return json_body(json!({
            "id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": model,
            "choices": [{"index": 0, "message": message, "finish_reason": if tools { "tool_calls" } else { "stop" }}],
            "usage": usage
        }));
    }
    let mut frames = Vec::new();
    if reasoning {
        frames.push(chunk(
            &model,
            json!({"role": "assistant", "reasoning_content": "erst "}),
            None,
            None,
        ));
        frames.push(chunk(
            &model,
            json!({"reasoning_content": "denken"}),
            None,
            None,
        ));
    }
    if tools {
        frames.push(chunk(
            &model,
            json!({"role": "assistant", "tool_calls": [{"index": 0, "id": "AbC123xYz", "type": "function",
                   "function": {"name": "wetter", "arguments": ""}}]}),
            None,
            None,
        ));
        frames.push(chunk(
            &model,
            json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"ort\":\"Berlin\"}"}}]}),
            None,
            None,
        ));
    } else {
        frames.push(chunk(&model, json!({"content": "Hallo"}), None, None));
        frames.push(chunk(&model, json!({"content": " Welt"}), None, None));
    }
    let finish = if tools { "tool_calls" } else { "stop" };
    // Mistral und ältere Ollama-Versionen: Usage im letzten Chunk mit Endgrund, kein
    // separater Usage-Chunk.
    let usage_in_final = model.starts_with("mistral") || model == "ollama-old";
    if usage_in_final {
        frames.push(chunk(&model, json!({}), Some(finish), Some(usage)));
    } else {
        frames.push(chunk(&model, json!({}), Some(finish), None));
        if body["stream_options"]["include_usage"] == true {
            frames.push(format!(
                "data: {}\n\n",
                json!({"id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1,
                       "model": model, "choices": [], "usage": usage})
            ));
        }
    }
    frames.push("data: [DONE]\n\n".to_owned());
    sse_body(frames)
}

async fn start_fake() -> (SocketAddr, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    let app = Router::new()
        .route("/v1/messages", post(anthropic_handler))
        .route("/v1/chat/completions", post(compat_handler))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, fake)
}

// ---------------------------------------------------------------------------------------------
// Gateway
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Capture(Mutex<Vec<RequestRecord>>);

impl RequestObserver for Capture {
    fn record(&self, record: RequestRecord) {
        self.0.lock().unwrap().push(record);
    }

    fn payload_mode(&self, _key: Option<&aigw_core::snapshot::KeyCtx>) -> PayloadMode {
        PayloadMode::Off
    }
}

impl Capture {
    async fn last(&self, n: usize) -> RequestRecord {
        for _ in 0..300 {
            {
                let r = self.0.lock().unwrap();
                if r.len() >= n {
                    return r[n - 1].clone();
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("nur {} von {n} Datensätzen", self.0.lock().unwrap().len());
    }
}

fn deployment(
    name: &str,
    provider: ProviderKind,
    endpoint: &str,
    model: &str,
    key: Option<&str>,
    settings: Value,
) -> DeploymentCtx {
    let Value::Object(settings) = settings else {
        panic!()
    };
    let settings: JsonObject = settings;
    DeploymentCtx {
        deployment: Deployment {
            id: new_id(),
            metadata: ObjectMeta::named(name),
            spec: DeploymentSpec {
                model_group: name.into(),
                provider,
                endpoint: Some(endpoint.into()),
                provider_model: model.into(),
                credential: key.map(|_| "c".into()),
                weight: 1,
                priority: 0,
                capacity: Default::default(),
                enabled: true,
                settings,
                budget: None,
            },
        },
        credential: key.map(|k| {
            Arc::new(CredentialData(BTreeMap::from([(
                "apiKey".to_owned(),
                SecretString::new(k),
            )])))
        }),
    }
}

struct Env {
    gw: SocketAddr,
    fake: Arc<Fake>,
    capture: Arc<Capture>,
}

async fn env() -> Env {
    let (up, fake) = start_fake().await;
    let base = format!("http://{up}");
    let v1 = format!("{base}/v1");
    let registry = Registry::with_builtin();
    let client = UpstreamClient::unguarded().unwrap();
    let mut routes = StaticRoutes::new();
    let fast = json!({"timeouts": {"firstByteMs": 2000, "idleMs": 1000, "totalMs": 5000}});

    // Anthropic: Basis ohne /v1; Modellgruppe vererbt defaultMaxTokens.
    let group_settings: JsonObject =
        serde_json::from_value(json!({"defaultMaxTokens": 777})).unwrap();
    for (alias, model) in [
        ("claude", "claude-ok"),
        ("claude-overloaded", "claude-overloaded"),
        ("claude-ratelimit", "claude-ratelimit"),
        ("claude-midstream", "claude-midstream"),
    ] {
        let mut settings = fast.clone();
        settings["anthropicBeta"] = json!("prompt-caching-2024-07-31");
        let d = deployment(
            &format!("dep-{alias}"),
            ProviderKind::Anthropic,
            &base,
            model,
            Some("sk-ant-up"),
            settings,
        );
        let d = with_group_defaults(&group_settings, &d);
        routes.insert(
            alias,
            Arc::new(Upstream::from_deployment(&d, &registry, &client).unwrap()),
        );
    }
    let budget = deployment(
        "dep-claude-budget",
        ProviderKind::Anthropic,
        &v1,
        "claude-ok",
        Some("sk-ant-up"),
        json!({"reasoningMode": "budget"}),
    );
    routes.insert(
        "claude-budget",
        Arc::new(Upstream::from_deployment(&budget, &registry, &client).unwrap()),
    );

    for (alias, kind, model, key) in [
        ("ollama", ProviderKind::Ollama, "ollama-new", None),
        ("ollama-old", ProviderKind::Ollama, "ollama-old", None),
        (
            "ollama-missing",
            ProviderKind::Ollama,
            "ollama-missing",
            None,
        ),
        ("vllm", ProviderKind::Vllm, "vllm-qwen3", Some("vllm-key")),
        (
            "mistral",
            ProviderKind::Mistral,
            "mistral-large",
            Some("mk"),
        ),
        (
            "mistral-422",
            ProviderKind::Mistral,
            "mistral-422",
            Some("mk"),
        ),
        (
            "mistral-429",
            ProviderKind::Mistral,
            "mistral-429",
            Some("mk"),
        ),
    ] {
        let d = deployment(&format!("dep-{alias}"), kind, &v1, model, key, fast.clone());
        routes.insert(
            alias,
            Arc::new(Upstream::from_deployment(&d, &registry, &client).unwrap()),
        );
    }

    let capture = Arc::new(Capture::default());
    let mut state = ProxyState::with_routes(Arc::new(routes));
    state.observer = capture.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, aigw_proxy::router(state))
            .await
            .unwrap();
    });
    Env { gw, fake, capture }
}

impl Env {
    fn last_seen(&self) -> Seen {
        self.fake.seen.lock().unwrap().last().cloned().unwrap()
    }

    async fn post(
        &self,
        path: &str,
        extra_headers: &[(&str, &str)],
        body: Value,
    ) -> (StatusCode, HeaderMap, Bytes) {
        let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
        let mut req = Request::builder()
            .method(Method::POST)
            .uri(format!("http://{}{path}", self.gw))
            .header("content-type", "application/json");
        if path == "/v1/messages" {
            req = req.header("anthropic-version", "2023-06-01");
        }
        for (k, v) in extra_headers {
            req = req.header(*k, *v);
        }
        let res = client
            .request(req.body(Full::new(Bytes::from(body.to_string()))).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let body = res.into_body().collect().await.unwrap().to_bytes();
        (status, headers, body)
    }
}

fn events(body: &[u8]) -> Vec<SseEvent> {
    SseParser::new().push(body)
}

fn json(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap()
}

/// OpenAI-Chunks: verketteter Text, Tool-Argumente, Endgründe, letzte Usage.
fn openai_stream(ev: &[SseEvent]) -> (String, String, Vec<String>, Option<Value>) {
    let mut text = String::new();
    let mut args = String::new();
    let mut finish = Vec::new();
    let mut usage = None;
    for e in ev.iter().filter(|e| e.data != "[DONE]") {
        let v: Value = serde_json::from_str(&e.data).unwrap();
        if let Some(c) = v["choices"].get(0) {
            if let Some(t) = c["delta"]["content"].as_str() {
                text.push_str(t);
            }
            if let Some(a) = c["delta"]["tool_calls"][0]["function"]["arguments"].as_str() {
                args.push_str(a);
            }
            if let Some(f) = c["finish_reason"].as_str() {
                finish.push(f.to_owned());
            }
        }
        if !v["usage"].is_null() {
            usage = Some(v["usage"].clone());
        }
    }
    (text, args, finish, usage)
}

fn tool_def() -> Value {
    json!([{"type": "function", "function": {"name": "wetter", "description": "Wetter",
            "parameters": {"type": "object", "properties": {"ort": {"type": "string"}}}}}])
}

// ---------------------------------------------------------------------------------------------
// Anthropic als Ziel
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn openai_client_to_anthropic_non_streaming_with_tools() {
    let e = env().await;
    let (status, _, body) = e
        .post(
            "/v1/chat/completions",
            &[],
            json!({"model": "claude", "tool_choice": "required", "tools": tool_def(),
                   "messages": [{"role": "system", "content": "Sei knapp."},
                                {"role": "user", "content": [
                                    {"type": "text", "text": "Wetter?"},
                                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}},
                                    {"type": "image_url", "image_url": {"url": "https://example.org/k.png"}}]}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let seen = e.last_seen();
    assert_eq!(seen.uri, "/v1/messages");
    assert_eq!(seen.headers["x-api-key"], "sk-ant-up");
    assert_eq!(seen.headers["anthropic-version"], "2023-06-01");
    assert_eq!(seen.headers["anthropic-beta"], "prompt-caching-2024-07-31");
    assert!(seen.headers.get("authorization").is_none());
    let b = &seen.body;
    assert_eq!(b["model"], "claude-ok");
    // max_tokens fehlte → geerbter Gruppenstandard.
    assert_eq!(b["max_tokens"], 777);
    assert_eq!(b["system"], "Sei knapp.");
    assert_eq!(b["tool_choice"], json!({"type": "any"}));
    assert_eq!(b["tools"][0]["input_schema"]["type"], "object");
    let content = &b["messages"][0]["content"];
    assert_eq!(content[1]["source"]["type"], "base64");
    assert_eq!(content[1]["source"]["media_type"], "image/png");
    assert_eq!(
        content[2]["source"],
        json!({"type": "url", "url": "https://example.org/k.png"})
    );

    let v = json(&body);
    let msg = &v["choices"][0]["message"];
    assert_eq!(msg["content"], "Ich schaue nach.");
    assert_eq!(msg["tool_calls"][0]["id"], "toolu_01A");
    assert_eq!(msg["tool_calls"][0]["function"]["name"], "wetter");
    let args: Value = serde_json::from_str(
        msg["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(args, json!({"ort": "Berlin"}));
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
    // Usage: Eingabe inkl. Cache (10 + 20 + 30), davon 30 aus dem Cache gelesen.
    assert_eq!(v["usage"]["prompt_tokens"], 60);
    assert_eq!(v["usage"]["prompt_tokens_details"]["cached_tokens"], 30);

    let r = e.capture.last(1).await;
    assert_eq!(r.provider, Some(ProviderKind::Anthropic));
    assert_eq!(r.usage.cache_write_tokens, Some(20));
    assert!(!r.usage_estimated);
}

#[tokio::test]
async fn openai_client_to_anthropic_streaming_with_tools_and_usage() {
    let e = env().await;
    let (status, _, body) = e
        .post(
            "/v1/chat/completions",
            &[],
            json!({"model": "claude", "stream": true, "max_tokens": 100,
                   "stream_options": {"include_usage": true}, "tools": tool_def(),
                   "messages": [{"role": "user", "content": "Wetter?"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let seen = e.last_seen();
    assert_eq!(seen.headers["accept"], "text/event-stream");
    assert!(seen.body.get("stream_options").is_none());
    assert_eq!(seen.body["max_tokens"], 100);
    let ev = events(&body);
    assert_eq!(ev.last().unwrap().data, "[DONE]");
    let (text, args, finish, usage) = openai_stream(&ev);
    assert_eq!(text, "Hallo Welt");
    assert_eq!(args, "{\"ort\":\"Berlin\"}");
    assert_eq!(finish, vec!["tool_calls"]);
    let usage = usage.unwrap();
    assert_eq!(usage["prompt_tokens"], 60);
    assert_eq!(usage["completion_tokens"], 5);
    let r = e.capture.last(1).await;
    assert!(r.stream && !r.usage_estimated);
    assert_eq!(r.usage.cache_read_tokens, Some(30));
}

#[tokio::test]
async fn anthropic_client_to_anthropic_passthrough_beta_cache_and_ping() {
    let e = env().await;
    let req = json!({"model": "claude", "max_tokens": 64, "stream": true,
                     "system": [{"type": "text", "text": "lang", "cache_control": {"type": "ephemeral"}}],
                     "thinking": {"type": "enabled", "budget_tokens": 2048},
                     "messages": [{"role": "user", "content": "hi"}]});
    let (status, _, body) = e
        .post(
            "/v1/messages",
            &[(
                "anthropic-beta",
                "interleaved-thinking-2025-05-14, prompt-caching-2024-07-31",
            )],
            req,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let seen = e.last_seen();
    // Konfigurierte und Client-Betas, ohne Duplikate.
    assert_eq!(
        seen.headers["anthropic-beta"],
        "prompt-caching-2024-07-31,interleaved-thinking-2025-05-14"
    );
    assert_eq!(seen.body["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(
        seen.body["thinking"],
        json!({"type": "enabled", "budget_tokens": 2048})
    );
    let ev = events(&body);
    let names: Vec<&str> = ev.iter().filter_map(|e| e.event.as_deref()).collect();
    assert_eq!(names.first(), Some(&"message_start"));
    assert_eq!(names.last(), Some(&"message_stop"));
    let start: Value = serde_json::from_str(&ev[0].data).unwrap();
    assert_eq!(start["message"]["usage"]["cache_read_input_tokens"], 30);
    assert_eq!(start["message"]["usage"]["cache_creation_input_tokens"], 20);
    assert_eq!(start["message"]["usage"]["input_tokens"], 10);
    let delta = ev
        .iter()
        .find(|e| e.event.as_deref() == Some("message_delta"))
        .unwrap();
    let delta: Value = serde_json::from_str(&delta.data).unwrap();
    assert_eq!(delta["usage"]["output_tokens"], 5);
    assert_eq!(delta["delta"]["stop_reason"], "end_turn");

    // Nicht gestreamt: unverändert im Anthropic-Format.
    let (status, _, body) = e
        .post(
            "/v1/messages",
            &[],
            json!({"model": "claude", "max_tokens": 64, "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let v = json(&body);
    assert_eq!(v["content"][0]["text"], "Hallo Welt");
    assert_eq!(v["usage"]["cache_read_input_tokens"], 30);
    assert_eq!(v["usage"]["input_tokens"], 10);
    assert_eq!(
        e.last_seen().headers["anthropic-beta"],
        "prompt-caching-2024-07-31"
    );
}

#[tokio::test]
async fn reasoning_effort_becomes_thinking_budget() {
    let e = env().await;
    let (status, _, _) = e
        .post(
            "/v1/chat/completions",
            &[],
            json!({"model": "claude-budget", "reasoning_effort": "low", "max_completion_tokens": 500,
                   "temperature": 0.2, "messages": [{"role": "user", "content": "rechne"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let b = e.last_seen().body;
    assert_eq!(
        b["thinking"],
        json!({"type": "enabled", "budget_tokens": 2048})
    );
    assert_eq!(b["max_tokens"], 2548);
    assert!(b.get("temperature").is_none());
    assert!(b.get("output_config").is_none());

    // Standardmodus adaptive.
    let (status, _, _) = e
        .post(
            "/v1/chat/completions",
            &[],
            json!({"model": "claude", "reasoning_effort": "high",
                   "messages": [{"role": "user", "content": "rechne"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let b = e.last_seen().body;
    assert_eq!(b["thinking"], json!({"type": "adaptive"}));
    assert_eq!(b["output_config"]["effort"], "high");
}

#[tokio::test]
async fn anthropic_errors_are_classified() {
    let e = env().await;
    // 529 overloaded: OpenAI-Client bekommt 503, Anthropic-Client 529.
    let (status, _, body) = e
        .post(
            "/v1/chat/completions",
            &[],
            json!({"model": "claude-overloaded", "messages": [{"role": "user", "content": "x"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json(&body)["error"]["code"], "overloaded_error");
    let (status, _, body) = e
        .post(
            "/v1/messages",
            &[],
            json!({"model": "claude-overloaded", "max_tokens": 5, "messages": [{"role": "user", "content": "x"}]}),
        )
        .await;
    assert_eq!(status.as_u16(), 529);
    assert_eq!(json(&body)["error"]["type"], "overloaded_error");
    let r = e.capture.last(2).await;
    assert_eq!(r.status, 529);

    // 429 mit retry-after.
    let (status, headers, body) = e
        .post(
            "/v1/messages",
            &[],
            json!({"model": "claude-ratelimit", "max_tokens": 5, "messages": [{"role": "user", "content": "x"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(headers["retry-after"], "4");
    assert_eq!(json(&body)["error"]["type"], "rate_limit_error");

    // Fehler mitten im Stream → Fehlerereignis im Eingangsformat.
    let (status, _, body) = e
        .post(
            "/v1/chat/completions",
            &[],
            json!({"model": "claude-midstream", "stream": true,
                   "messages": [{"role": "user", "content": "x"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let ev = events(&body);
    let last: Value = serde_json::from_str(&ev.last().unwrap().data).unwrap();
    assert_eq!(last["error"]["code"], "overloaded_error");
}

// ---------------------------------------------------------------------------------------------
// Ollama, vLLM, Mistral
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn ollama_usage_from_final_chunk_and_errors() {
    let e = env().await;
    // Neuere Version: stream_options wird gesendet, Usage-Chunk kommt separat.
    let (status, _, _) = e
        .post(
            "/v1/chat/completions",
            &[],
            json!({"model": "ollama", "stream": true, "max_tokens": 20,
                   "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let seen = e.last_seen();
    assert_eq!(seen.body["stream_options"]["include_usage"], true);
    assert_eq!(seen.body["max_tokens"], 20);
    assert!(seen.headers.get("authorization").is_none());
    let r = e.capture.last(1).await;
    assert!(!r.usage_estimated);
    assert_eq!(r.usage.input_tokens, 9);

    // Ältere Version: ignoriert stream_options, Usage steht im letzten Chunk mit Endgrund.
    let (status, _, body) = e
        .post(
            "/v1/messages",
            &[],
            json!({"model": "ollama-old", "stream": true, "max_tokens": 20,
                   "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let ev = events(&body);
    let delta = ev
        .iter()
        .find(|e| e.event.as_deref() == Some("message_delta"))
        .unwrap();
    let delta: Value = serde_json::from_str(&delta.data).unwrap();
    assert_eq!(delta["usage"]["output_tokens"], 4);
    assert_eq!(delta["usage"]["input_tokens"], 9);
    let r = e.capture.last(2).await;
    assert!(!r.usage_estimated);
    assert_eq!(r.provider, Some(ProviderKind::Ollama));

    // Unbekanntes Modell (`{"error":"…"}` als String) → 502 upstream_not_found.
    let (status, _, body) = e
        .post(
            "/v1/chat/completions",
            &[],
            json!({"model": "ollama-missing", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(json(&body)["error"]["code"], "upstream_not_found");
}

#[tokio::test]
async fn vllm_reasoning_content_becomes_ir_reasoning() {
    let e = env().await;
    // Anthropic-Client, gestreamt: reasoning_content → thinking-Block.
    let (status, _, body) = e
        .post(
            "/v1/messages",
            &[],
            json!({"model": "vllm", "stream": true, "max_tokens": 20, "top_k": 4,
                   "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let seen = e.last_seen();
    assert_eq!(seen.headers["authorization"], "Bearer vllm-key");
    assert_eq!(seen.body["top_k"], 4);
    assert_eq!(seen.body["max_tokens"], 20);
    let ev = events(&body);
    let mut thinking = String::new();
    let mut text = String::new();
    for e in &ev {
        let v: Value = serde_json::from_str(&e.data).unwrap();
        match v["delta"]["type"].as_str() {
            Some("thinking_delta") => thinking.push_str(v["delta"]["thinking"].as_str().unwrap()),
            Some("text_delta") => text.push_str(v["delta"]["text"].as_str().unwrap()),
            _ => {}
        }
    }
    assert_eq!(thinking, "erst denken");
    assert_eq!(text, "Hallo Welt");

    // OpenAI-Client, nicht gestreamt: reasoning_content bleibt erhalten.
    let (status, _, body) = e
        .post(
            "/v1/chat/completions",
            &[],
            json!({"model": "vllm", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let v = json(&body);
    assert_eq!(v["choices"][0]["message"]["content"], "Hallo Welt");
    assert_eq!(
        v["choices"][0]["message"]["reasoning_content"],
        "erst denken"
    );
}

#[tokio::test]
async fn anthropic_client_to_mistral_with_tool_history() {
    let e = env().await;
    let (status, _, body) = e
        .post(
            "/v1/messages",
            &[],
            json!({"model": "mistral", "max_tokens": 50, "stream": true,
                   "tool_choice": {"type": "any"},
                   "tools": [{"name": "wetter", "input_schema": {"type": "object"}}],
                   "metadata": {"user_id": "u-1"},
                   "messages": [
                       {"role": "user", "content": "Wetter?"},
                       {"role": "assistant", "content": [
                           {"type": "tool_use", "id": "toolu_01X", "name": "wetter", "input": {"ort": "Köln"}}]},
                       {"role": "user", "content": [
                           {"type": "tool_result", "tool_use_id": "toolu_01X", "content": "Regen"}]},
                       {"role": "user", "content": "Und Berlin?"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let seen = e.last_seen();
    assert_eq!(seen.headers["authorization"], "Bearer mk");
    let b = &seen.body;
    assert_eq!(b["tool_choice"], "any");
    assert!(b.get("stream_options").is_none());
    assert!(b.get("user").is_none());
    assert_eq!(b["max_tokens"], 50);
    let id = b["messages"][1]["tool_calls"][0]["id"].as_str().unwrap();
    assert!(
        id.len() == 9 && id.chars().all(|c| c.is_ascii_alphanumeric()),
        "{id}"
    );
    assert_eq!(b["messages"][2]["tool_call_id"], id);

    let ev = events(&body);
    let start = ev
        .iter()
        .find(|e| {
            e.data.contains("\"tool_use\"") && e.event.as_deref() == Some("content_block_start")
        })
        .unwrap();
    let start: Value = serde_json::from_str(&start.data).unwrap();
    assert_eq!(start["content_block"]["id"], "AbC123xYz");
    let delta = ev
        .iter()
        .find(|e| e.event.as_deref() == Some("message_delta"))
        .unwrap();
    let delta: Value = serde_json::from_str(&delta.data).unwrap();
    assert_eq!(delta["delta"]["stop_reason"], "tool_use");
    // Mistral liefert die Usage im letzten Chunk.
    assert_eq!(delta["usage"]["output_tokens"], 4);
    let r = e.capture.last(1).await;
    assert!(!r.usage_estimated);
    assert_eq!(r.provider, Some(ProviderKind::Mistral));
}

#[tokio::test]
async fn openai_client_to_mistral_and_errors() {
    let e = env().await;
    let (status, _, body) = e
        .post(
            "/v1/chat/completions",
            &[],
            json!({"model": "mistral", "seed": 3, "safe_prompt": true, "tools": tool_def(),
                   "messages": [{"role": "user", "content": "Wetter?"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let b = e.last_seen().body;
    assert_eq!(b["random_seed"], 3);
    assert!(b.get("seed").is_none());
    assert_eq!(b["safe_prompt"], true);
    let v = json(&body);
    assert_eq!(
        v["choices"][0]["message"]["tool_calls"][0]["id"],
        "AbC123xYz"
    );
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");

    let (status, _, body) = e
        .post(
            "/v1/chat/completions",
            &[],
            json!({"model": "mistral-422", "messages": [{"role": "user", "content": "x"}]}),
        )
        .await;
    // F152: sonstige 4xx des Anbieters mit exaktem Status (Klasse invalid_request_error).
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(json(&body)["error"]["type"], "invalid_request_error");
    assert_eq!(
        json(&body)["error"]["message"],
        "body.foo: Extra inputs are not permitted"
    );

    let (status, headers, body) = e
        .post(
            "/v1/messages",
            &[],
            json!({"model": "mistral-429", "max_tokens": 5, "messages": [{"role": "user", "content": "x"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(headers["retry-after"], "2");
    assert_eq!(json(&body)["error"]["type"], "rate_limit_error");
}
