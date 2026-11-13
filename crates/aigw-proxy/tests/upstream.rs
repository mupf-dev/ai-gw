// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Ende-zu-Ende-Tests Gateway → echter HTTP-Upstream (lokaler Fake-Server, der OpenAI- und
//! Azure-Antworten simuliert): Übersetzung, Usage-Erzwingung, Timeouts, Fehlerklassen,
//! Client-Abbruch und Backpressure. Keine externen Aufrufe, keine echten Schlüssel.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aigw_core::ir::ErrorKind;
use aigw_core::observe::{PayloadMode, RequestObserver, RequestRecord};
use aigw_core::resource::{
    CredentialData, Deployment, DeploymentSpec, JsonObject, ObjectMeta, SecretString, new_id,
};
use aigw_core::snapshot::DeploymentCtx;
use aigw_core::types::ProviderKind;
use aigw_core::wire::sse::{SseEvent, SseParser};
use aigw_providers::{Registry, Upstream, UpstreamClient};
use aigw_proxy::{ProxyState, StaticRoutes};
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use bytes::Bytes;
use futures_util::StreamExt as _;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------------------------
// Fake-Upstream
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
    /// Vom Upstream-Body bisher erzeugte Chunks (Modell `infinite`).
    pulled: AtomicUsize,
    /// Der Upstream-Body wurde verworfen (Verbindung vom Gateway geschlossen).
    dropped: AtomicBool,
}

struct DropFlag(Arc<Fake>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.dropped.store(true, Ordering::SeqCst);
    }
}

fn chunk(content: &str) -> String {
    format!(
        "data: {}\n\n",
        json!({"id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1,
               "model": "up-model", "choices": [{"index": 0, "delta": {"content": content}}]})
    )
}

fn finish_chunk() -> String {
    format!(
        "data: {}\n\n",
        json!({"id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1, "model": "up-model",
               "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})
    )
}

fn usage_chunk() -> String {
    format!(
        "data: {}\n\n",
        json!({"id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1, "model": "up-model",
               "choices": [], "usage": {"prompt_tokens": 11, "completion_tokens": 2, "total_tokens": 13}})
    )
}

fn sse(parts: Vec<(u64, String)>, guard: Option<DropFlag>) -> Response {
    let s =
        futures_util::stream::unfold((parts.into_iter(), guard), |(mut it, guard)| async move {
            let (delay, data) = it.next()?;
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            Some((Ok::<_, std::io::Error>(Bytes::from(data)), (it, guard)))
        });
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

async fn fake_handler(
    State(fake): State<Arc<Fake>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let body: Value = serde_json::from_slice(&body).unwrap();
    fake.seen.lock().unwrap().push(Seen {
        uri: uri.to_string(),
        headers: headers.clone(),
        body: body.clone(),
    });
    let model = body["model"].as_str().unwrap_or_default().to_owned();
    let stream = body["stream"].as_bool().unwrap_or(false);
    let wants_usage = body["stream_options"]["include_usage"]
        .as_bool()
        .unwrap_or(false);
    match model.as_str() {
        "rate-limited" => error(
            429,
            &[("retry-after", "3")],
            json!({"error": {"message": "Rate limit reached", "type": "requests", "code": "rate_limit_exceeded"}}),
        ),
        "broken" => error(500, &[], json!({"error": {"message": "boom", "type": "server_error"}})),
        "too-long" => error(
            400,
            &[],
            json!({"error": {"message": "This model's maximum context length is 10 tokens", "code": "context_length_exceeded", "param": "messages"}}),
        ),
        "bad-key" => error(401, &[], json!({"error": {"message": "Incorrect API key provided: sk-up***"}})),
        "hang" => sse(vec![(0, chunk("a")), (60_000, chunk("b"))], None),
        "midstream" => sse(
            vec![
                (0, chunk("a")),
                (0, "data: {\"error\":{\"message\":\"upstream exploded\",\"type\":\"server_error\"}}\n\n".to_owned()),
            ],
            None,
        ),
        "truncated" => sse(vec![(0, chunk("a"))], None),
        "infinite" => {
            let big = "x".repeat(8 * 1024);
            let guard = DropFlag(fake.clone());
            let f = fake.clone();
            let s = futures_util::stream::unfold((guard, f, big), |(g, f, big)| async move {
                f.pulled.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
                let c = chunk(&big);
                Some((Ok::<_, std::io::Error>(Bytes::from(c)), (g, f, big)))
            });
            ([("content-type", "text/event-stream")], Body::from_stream(s)).into_response()
        }
        _ if stream => {
            let delay = if model == "slow" { 150 } else { 0 };
            let mut parts = vec![
                (delay, chunk("Hallo")),
                (delay, chunk(" Welt")),
                (0, finish_chunk()),
            ];
            if wants_usage && model != "no-usage" {
                parts.push((0, usage_chunk()));
            }
            parts.push((0, "data: [DONE]\n\n".to_owned()));
            sse(parts, None)
        }
        _ => (
            [("content-type", "application/json")],
            json!({"id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": "up-model",
                   "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hallo Welt"},
                                "finish_reason": "stop"}],
                   "usage": {"prompt_tokens": 11, "completion_tokens": 2, "total_tokens": 13}})
            .to_string(),
        )
            .into_response(),
    }
}

async fn start_fake() -> (SocketAddr, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    let app = Router::new()
        .route("/v1/chat/completions", post(fake_handler))
        .route(
            "/openai/deployments/{deployment}/chat/completions",
            post(fake_handler),
        )
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
struct Capture(Mutex<Vec<RequestRecord>>, PayloadMode);

impl RequestObserver for Capture {
    fn record(&self, record: RequestRecord) {
        self.0.lock().unwrap().push(record);
    }

    fn payload_mode(&self) -> PayloadMode {
        self.1
    }
}

impl Capture {
    async fn wait(&self, n: usize) -> Vec<RequestRecord> {
        for _ in 0..300 {
            {
                let r = self.0.lock().unwrap();
                if r.len() >= n {
                    return r.clone();
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
    env_with(PayloadMode::Off).await
}

async fn env_with(mode: PayloadMode) -> Env {
    let (up, fake) = start_fake().await;
    let base = format!("http://{up}/v1");
    let registry = Registry::with_builtin();
    let client = UpstreamClient::unguarded().unwrap();
    let mut routes = StaticRoutes::new();
    let fast = json!({"timeouts": {"firstByteMs": 2000, "idleMs": 300, "totalMs": 5000}});
    for model in [
        "ok",
        "slow",
        "no-usage",
        "rate-limited",
        "broken",
        "too-long",
        "bad-key",
        "hang",
        "midstream",
        "truncated",
        "infinite",
    ] {
        let d = deployment(
            &format!("dep-{model}"),
            ProviderKind::OpenAi,
            &base,
            model,
            Some("sk-upstream"),
            fast.clone(),
        );
        routes.insert(
            model,
            Arc::new(Upstream::from_deployment(&d, &registry, &client).unwrap()),
        );
    }
    let azure = deployment(
        "dep-azure",
        ProviderKind::AzureOpenAi,
        &format!("http://{up}"),
        "ok",
        Some("az-key"),
        json!({"apiVersion": "2024-10-21"}),
    );
    routes.insert(
        "azure",
        Arc::new(Upstream::from_deployment(&azure, &registry, &client).unwrap()),
    );
    let compat = deployment(
        "dep-compat",
        ProviderKind::OpenAiCompatible,
        &base,
        "ok",
        None,
        json!({}),
    );
    routes.insert(
        "compat",
        Arc::new(Upstream::from_deployment(&compat, &registry, &client).unwrap()),
    );
    let down = deployment(
        "dep-down",
        ProviderKind::OpenAiCompatible,
        "http://127.0.0.1:1/v1",
        "ok",
        None,
        json!({}),
    );
    routes.insert(
        "down",
        Arc::new(Upstream::from_deployment(&down, &registry, &client).unwrap()),
    );

    let capture = Arc::new(Capture(Mutex::default(), mode));
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

    async fn post(&self, path: &str, body: Value) -> (StatusCode, HeaderMap, Bytes) {
        let res = self.open(path, body).await;
        let status = res.status();
        let headers = res.headers().clone();
        let body = res.into_body().collect().await.unwrap().to_bytes();
        (status, headers, body)
    }

    async fn open(&self, path: &str, body: Value) -> hyper::Response<hyper::body::Incoming> {
        let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
        let mut req = Request::builder()
            .method(Method::POST)
            .uri(format!("http://{}{path}", self.gw))
            .header("content-type", "application/json");
        if path == "/v1/messages" {
            req = req.header("anthropic-version", "2023-06-01");
        }
        client
            .request(req.body(Full::new(Bytes::from(body.to_string()))).unwrap())
            .await
            .unwrap()
    }
}

fn events(body: &[u8]) -> Vec<SseEvent> {
    SseParser::new().push(body)
}

fn chat(model: &str, stream: bool) -> Value {
    json!({"model": model, "stream": stream, "max_tokens": 50,
           "messages": [{"role": "user", "content": "Sag hallo"}]})
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn non_streaming_roundtrip_through_openai_upstream() {
    let e = env().await;
    let (status, _, body) = e.post("/v1/chat/completions", chat("ok", false)).await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "Hallo Welt");
    assert_eq!(v["usage"]["prompt_tokens"], 11);

    let seen = e.last_seen();
    assert_eq!(seen.uri, "/v1/chat/completions");
    assert_eq!(seen.headers["authorization"], "Bearer sk-upstream");
    assert_eq!(seen.body["model"], "ok");
    // Client schickte das veraltete max_tokens; OpenAI bekommt max_completion_tokens.
    assert_eq!(seen.body["max_completion_tokens"], 50);
    assert!(seen.body.get("max_tokens").is_none());
    assert!(seen.body.get("stream_options").is_none());

    let r = &e.capture.wait(1).await[0];
    assert_eq!(r.model_alias.as_deref(), Some("ok"));
    assert_eq!(r.model_group.as_deref(), Some("dep-ok"));
    assert!(r.deployment_id.is_some());
    assert_eq!(r.provider, Some(ProviderKind::OpenAi));
    assert_eq!(r.status, 200);
    assert!(!r.stream && !r.client_aborted && !r.usage_estimated);
    assert_eq!(r.usage.output_tokens, 2);
    assert!(r.ttft.is_none());
}

#[tokio::test]
async fn azure_and_compatible_targets() {
    let e = env().await;
    let (status, _, _) = e.post("/v1/chat/completions", chat("azure", false)).await;
    assert_eq!(status, StatusCode::OK);
    let seen = e.last_seen();
    assert_eq!(
        seen.uri,
        "/openai/deployments/ok/chat/completions?api-version=2024-10-21"
    );
    assert_eq!(seen.headers["api-key"], "az-key");
    assert!(seen.headers.get("authorization").is_none());
    assert_eq!(seen.body["max_completion_tokens"], 50);

    let (status, _, _) = e.post("/v1/chat/completions", chat("compat", false)).await;
    assert_eq!(status, StatusCode::OK);
    let seen = e.last_seen();
    assert!(seen.headers.get("authorization").is_none());
    assert_eq!(seen.body["max_tokens"], 50);
}

#[tokio::test]
async fn streaming_forces_usage_and_strips_it_for_the_client() {
    let e = env().await;
    // Client will keine Usage: Upstream bekommt include_usage trotzdem, Client sieht keine.
    let (status, headers, body) = e.post("/v1/chat/completions", chat("ok", true)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    assert_eq!(e.last_seen().body["stream_options"]["include_usage"], true);
    let ev = events(&body);
    assert_eq!(ev.last().unwrap().data, "[DONE]");
    assert!(ev.iter().all(|e| !e.data.contains("\"usage\"")));
    let text: String = ev
        .iter()
        .filter(|e| e.data != "[DONE]")
        .filter_map(|e| {
            serde_json::from_str::<Value>(&e.data).unwrap()["choices"][0]["delta"]["content"]
                .as_str()
                .map(str::to_owned)
        })
        .collect();
    assert_eq!(text, "Hallo Welt");
    let r = &e.capture.wait(1).await[0];
    assert!(r.stream && !r.usage_estimated && !r.client_aborted);
    assert_eq!(r.usage.input_tokens, 11);
    assert!(r.ttft.is_some());

    // Client will Usage: Usage-Chunk vor [DONE].
    let mut req = chat("ok", true);
    req["stream_options"] = json!({"include_usage": true});
    let (_, _, body) = e.post("/v1/chat/completions", req).await;
    let ev = events(&body);
    let usage: Value = serde_json::from_str(&ev[ev.len() - 2].data).unwrap();
    assert_eq!(usage["usage"]["completion_tokens"], 2);
}

#[tokio::test]
async fn anthropic_ingress_streams_from_openai_upstream() {
    let e = env().await;
    let (status, _, body) = e
        .post(
            "/v1/messages",
            json!({"model": "slow", "max_tokens": 50, "stream": true,
                   "messages": [{"role": "user", "content": "Sag hallo"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(e.last_seen().body["stream_options"]["include_usage"], true);
    let ev = events(&body);
    let names: Vec<&str> = ev.iter().filter_map(|e| e.event.as_deref()).collect();
    assert_eq!(names.first(), Some(&"message_start"));
    assert_eq!(names.last(), Some(&"message_stop"));
    let delta = ev
        .iter()
        .find(|e| e.event.as_deref() == Some("message_delta"))
        .unwrap();
    let delta: Value = serde_json::from_str(&delta.data).unwrap();
    assert_eq!(delta["usage"]["output_tokens"], 2);
    assert_eq!(delta["delta"]["stop_reason"], "end_turn");
    let r = &e.capture.wait(1).await[0];
    // Zwei Chunks à 150 ms Verzögerung → TTFT ≥ 150 ms.
    assert!(r.ttft.unwrap() >= Duration::from_millis(140), "{r:?}");
    assert!(r.duration >= r.ttft.unwrap());
}

#[tokio::test]
async fn missing_stream_usage_is_estimated() {
    let e = env().await;
    let (status, _, _) = e.post("/v1/chat/completions", chat("no-usage", true)).await;
    assert_eq!(status, StatusCode::OK);
    let r = &e.capture.wait(1).await[0];
    assert!(r.usage_estimated);
    let u = &r.usage;
    assert!(u.input_tokens > 0 && u.output_tokens > 0, "{u:?}");
}

#[tokio::test]
async fn upstream_errors_are_classified_and_in_ingress_format() {
    let e = env().await;
    let (status, headers, body) = e
        .post("/v1/chat/completions", chat("rate-limited", false))
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(headers["retry-after"], "3");
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"]["type"], "rate_limit_error");

    let (status, _, body) = e
        .post(
            "/v1/messages",
            json!({"model": "broken", "max_tokens": 5, "stream": true,
                   "messages": [{"role": "user", "content": "x"}]}),
        )
        .await;
    assert_eq!(status.as_u16(), 500);
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["type"], "error");

    let (status, _, body) = e
        .post("/v1/chat/completions", chat("too-long", false))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"]["code"], "context_length_exceeded");

    let (status, _, body) = e.post("/v1/chat/completions", chat("bad-key", false)).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(!String::from_utf8_lossy(&body).contains("sk-up"));

    let (status, _, body) = e.post("/v1/chat/completions", chat("down", true)).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"]["code"], "upstream_connect_error");

    let (status, _, _) = e.post("/v1/chat/completions", chat("unknown", false)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let recs = e.capture.wait(6).await;
    let kinds: Vec<Option<ErrorKind>> = recs.iter().map(|r| r.error_kind).collect();
    assert_eq!(
        kinds,
        vec![
            Some(ErrorKind::RateLimit),
            Some(ErrorKind::Internal),
            Some(ErrorKind::InvalidRequest),
            Some(ErrorKind::BadGateway),
            Some(ErrorKind::BadGateway),
            Some(ErrorKind::NotFound),
        ]
    );
    assert_eq!(recs[0].status, 429);
    assert_eq!(recs[3].status, 502);
    assert!(
        recs.iter()
            .all(|r| r.usage.total_tokens() == 0 && !r.usage_estimated)
    );
}

#[tokio::test]
async fn mid_stream_errors_and_timeouts_become_error_events() {
    let e = env().await;
    // Pause zwischen Chunks > idleMs (300 ms).
    let (status, _, body) = e.post("/v1/chat/completions", chat("hang", true)).await;
    assert_eq!(status, StatusCode::OK);
    let ev = events(&body);
    let last: Value = serde_json::from_str(&ev.last().unwrap().data).unwrap();
    assert_eq!(last["error"]["code"], "upstream_timeout");
    assert_eq!(last["error"]["type"], "timeout_error");

    // Fehlerobjekt im Upstream-Stream → Anthropic-error-Ereignis.
    let (_, _, body) = e
        .post(
            "/v1/messages",
            json!({"model": "midstream", "max_tokens": 5, "stream": true,
                   "messages": [{"role": "user", "content": "x"}]}),
        )
        .await;
    let ev = events(&body);
    let last = ev.last().unwrap();
    assert_eq!(last.event.as_deref(), Some("error"));
    assert!(last.data.contains("upstream exploded"));

    // Upstream endet ohne finish_reason/[DONE].
    let (_, _, body) = e
        .post("/v1/chat/completions", chat("truncated", true))
        .await;
    let ev = events(&body);
    assert!(
        ev.last()
            .unwrap()
            .data
            .contains("upstream_stream_truncated")
    );

    let recs = e.capture.wait(3).await;
    assert_eq!(recs[0].error_kind, Some(ErrorKind::Timeout));
    assert_eq!(recs[1].error_kind, Some(ErrorKind::BadGateway));
    assert_eq!(recs[2].error_kind, Some(ErrorKind::BadGateway));
    assert!(recs.iter().all(|r| r.status == 200 && r.usage_estimated));
}

#[tokio::test]
async fn backpressure_and_client_abort() {
    let e = env().await;
    let res = e.open("/v1/chat/completions", chat("infinite", true)).await;
    assert_eq!(res.status(), StatusCode::OK);
    let mut body = res.into_body();
    // Erstes Stück lesen, dann nicht mehr abnehmen.
    let first = body.frame().await.unwrap().unwrap();
    assert!(first.is_data());
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let a = e.fake.pulled.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let b = e.fake.pulled.load(Ordering::SeqCst);
    // Der Upstream wird nur so weit gelesen, wie Socket-Puffer reichen – danach steht er.
    assert!(b - a <= 2, "Upstream läuft weiter: {a} → {b}");
    assert!(
        b * 8 * 1024 < 64 * 1024 * 1024,
        "zu viel gepuffert: {b} Chunks"
    );
    assert!(!e.fake.dropped.load(Ordering::SeqCst));

    // Client bricht ab → Upstream-Verbindung wird sofort geschlossen.
    drop(body);
    for _ in 0..200 {
        if e.fake.dropped.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        e.fake.dropped.load(Ordering::SeqCst),
        "Upstream nicht abgebrochen"
    );
    let r = &e.capture.wait(1).await[0];
    assert!(r.client_aborted && r.usage_estimated && r.stream);
    assert!(r.usage.output_tokens > 0);
}

#[tokio::test]
async fn sse_frames_reach_the_client_without_buffering() {
    let e = env().await;
    let res = e.open("/v1/chat/completions", chat("slow", true)).await;
    let mut stream = res.into_body().into_data_stream();
    let t0 = std::time::Instant::now();
    let mut got = Vec::new();
    // Das erste Textstück kommt nach ~150 ms, nicht erst am Ende (~300 ms).
    while let Some(Ok(b)) = stream.next().await {
        got.extend_from_slice(&b);
        if String::from_utf8_lossy(&got).contains("Hallo") {
            break;
        }
    }
    let ttft = t0.elapsed();
    assert!(ttft < Duration::from_millis(280), "{ttft:?}");
}

#[tokio::test]
async fn payload_is_captured_only_on_request() {
    let e = env_with(PayloadMode::Full).await;
    let (status, _, _) = e.post("/v1/chat/completions", chat("ok", false)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = e.post("/v1/chat/completions", chat("ok", true)).await;
    assert_eq!(status, StatusCode::OK);
    let recs = e.capture.wait(2).await;
    let p = recs[0].payload.as_ref().unwrap();
    assert!(String::from_utf8_lossy(&p.request_body).contains("Sag hallo"));
    assert!(String::from_utf8_lossy(&p.response_body).contains("Hallo Welt"));
    assert!(p.request_headers.iter().any(|(k, _)| k == "content-type"));
    let p = recs[1].payload.as_ref().unwrap();
    assert_eq!(&p.response_body[..], b"Hallo Welt");

    let e = env().await;
    e.post("/v1/chat/completions", chat("ok", false)).await;
    assert!(e.capture.wait(1).await[0].payload.is_none());
}
