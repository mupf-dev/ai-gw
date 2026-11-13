// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Ende-zu-Ende-Tests der weiteren Endpunkte (AIGW-21): Embeddings, Bilder, Audio und die
//! Responses API – gegen den Mock-Anbieter und gegen lokale Fake-Upstreams je Anbieter
//! (OpenAI, Azure, kompatibel, Ollama, Mistral, Bedrock, Gemini API, Vertex AI). Dazu
//! Fähigkeiten der Modellgruppen (`settings.capabilities`) und Request-Log-Kennzahlen.
//! Keine externen Aufrufe, keine echten Schlüssel.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aigw_core::observe::{PayloadMode, RequestObserver, RequestRecord};
use aigw_core::resource::{
    CredentialData, Deployment, DeploymentSpec, JsonObject, ObjectMeta, RoutingStrategy,
    SecretString, new_id,
};
use aigw_core::snapshot::DeploymentCtx;
use aigw_core::types::ProviderKind;
use aigw_core::wire::WireFormat;
use aigw_core::wire::base64;
use aigw_core::wire::multipart::{self, FormBuilder};
use aigw_core::wire::sse::SseParser;
use aigw_providers::{Registry, Upstream, UpstreamClient};
use aigw_proxy::mock::MockProvider;
use aigw_proxy::routing::{GroupPolicy, MemberSpec};
use aigw_proxy::{ChatProvider, GroupRoute, ProxyConfig, ProxyState, StaticRoutes};
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------------------------
// Gateway-Hilfen
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
        serde_json::from_slice(&self.body).unwrap_or_else(|e| {
            panic!(
                "kein JSON ({e}, {}): {}",
                self.status,
                String::from_utf8_lossy(&self.body)
            )
        })
    }

    fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
    }
}

async fn send(addr: SocketAddr, method: Method, path: &str, ct: &str, body: Bytes) -> Resp {
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let req = Request::builder()
        .method(method)
        .uri(format!("http://{addr}{path}"))
        .header("content-type", ct)
        .body(Full::new(body))
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

async fn post(addr: SocketAddr, path: &str, body: Value) -> Resp {
    send(
        addr,
        Method::POST,
        path,
        "application/json",
        Bytes::from(body.to_string()),
    )
    .await
}

async fn post_form(addr: SocketAddr, path: &str, form: FormBuilder) -> Resp {
    let (ct, body) = form.finish();
    send(addr, Method::POST, path, &ct, body).await
}

fn audio_form(model: &str, format: Option<&str>, file: &[u8]) -> FormBuilder {
    let mut f = FormBuilder::new();
    f.text("model", model);
    if let Some(fmt) = format {
        f.text("response_format", fmt);
    }
    f.file("file", "probe.wav", "audio/wav", file);
    f
}

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
    async fn nth(&self, n: usize) -> RequestRecord {
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

async fn mock_gateway() -> (SocketAddr, Arc<Capture>) {
    let capture = Arc::new(Capture::default());
    let state = ProxyState {
        observer: capture.clone(),
        ..ProxyState::mock()
    };
    (start(state).await, capture)
}

// ---------------------------------------------------------------------------------------------
// Mock-Anbieter
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn embeddings_via_mock() {
    let (gw, capture) = mock_gateway().await;
    let r = post(
        gw,
        "/v1/embeddings",
        json!({"model": "mock", "input": "Hallo Welt"}),
    )
    .await;
    assert_eq!(r.status, 200);
    let v = r.json();
    assert_eq!(v["object"], "list");
    let e = v["data"][0]["embedding"].as_array().unwrap();
    assert_eq!(e.len(), 8);
    let norm: f64 = e.iter().map(|x| x.as_f64().unwrap().powi(2)).sum();
    assert!((norm - 1.0).abs() < 1e-3);
    assert_eq!(v["usage"]["prompt_tokens"], 3);
    let rec = capture.nth(1).await;
    assert_eq!(rec.ingress, WireFormat::OpenAiEmbeddings);
    assert_eq!(rec.usage.input_tokens, 3);
    assert!(!rec.usage_estimated);
    assert_eq!(rec.status, 200);

    // Base64, Dimension, mehrere Eingaben.
    let r = post(
        gw,
        "/v1/embeddings",
        json!({"model": "mock", "input": ["a", "b"], "encoding_format": "base64", "dimensions": 4}),
    )
    .await;
    let v = r.json();
    assert_eq!(v["data"].as_array().unwrap().len(), 2);
    assert_eq!(v["data"][1]["index"], 1);
    let vec = base64::decode_f32(v["data"][1]["embedding"].as_str().unwrap()).unwrap();
    assert_eq!(vec.len(), 4);

    // Token-Eingabe, Fehler.
    let r = post(
        gw,
        "/v1/embeddings",
        json!({"model": "mock", "input": [[1, 2], [3]]}),
    )
    .await;
    assert_eq!(r.status, 200);
    let r = post(gw, "/v1/embeddings", json!({"model": "mock", "input": []})).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"]["param"], "input");
    let r = post(
        gw,
        "/v1/embeddings",
        json!({"model": "mock-error", "input": "x"}),
    )
    .await;
    assert_eq!(r.status, 503);
    let r = send(
        gw,
        Method::GET,
        "/v1/embeddings",
        "application/json",
        Bytes::new(),
    )
    .await;
    assert_eq!(r.status, 405);
}

#[tokio::test]
async fn images_via_mock() {
    let (gw, capture) = mock_gateway().await;
    let r = post(
        gw,
        "/v1/images/generations",
        json!({"model": "mock", "prompt": "Eine Katze", "n": 2, "response_format": "b64_json"}),
    )
    .await;
    assert_eq!(r.status, 200);
    let v = r.json();
    assert_eq!(v["data"].as_array().unwrap().len(), 2);
    assert!(
        v["data"][0]["b64_json"]
            .as_str()
            .unwrap()
            .starts_with("iVBOR")
    );
    assert_eq!(v["data"][0]["revised_prompt"], "Mock: Eine Katze");
    let rec = capture.nth(1).await;
    assert_eq!(rec.ingress, WireFormat::OpenAiImages);
    assert_eq!(rec.usage.images, Some(2));

    let r = post(
        gw,
        "/v1/images/generations",
        json!({"model": "mock", "prompt": "x", "response_format": "url"}),
    )
    .await;
    assert!(
        r.json()["data"][0]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png")
    );
    for bad in [
        json!({"model": "mock", "prompt": " "}),
        json!({"model": "mock", "prompt": "x", "n": 11}),
        json!({"model": "mock", "prompt": "x", "stream": true}),
    ] {
        let r = post(gw, "/v1/images/generations", bad.clone()).await;
        assert_eq!(r.status, 400, "{bad}");
    }
}

#[tokio::test]
async fn audio_via_mock() {
    let (gw, capture) = mock_gateway().await;
    let file = vec![0u8; 64_000];
    let r = post_form(
        gw,
        "/v1/audio/transcriptions",
        audio_form("mock", None, &file),
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["text"], "Mock-Transkript (64000 Bytes)");
    let rec = capture.nth(1).await;
    assert_eq!(rec.ingress, WireFormat::OpenAiAudio);
    assert_eq!(rec.usage.audio_ms, Some(2000));

    let r = post_form(
        gw,
        "/v1/audio/transcriptions",
        audio_form("mock", Some("verbose_json"), &file),
    )
    .await;
    let v = r.json();
    assert_eq!(v["task"], "transcribe");
    assert_eq!(v["duration"], 2.0);

    let r = post_form(
        gw,
        "/v1/audio/transcriptions",
        audio_form("mock", Some("srt"), &file),
    )
    .await;
    assert!(r.header("content-type").starts_with("application/x-subrip"));
    assert!(String::from_utf8_lossy(&r.body).contains("-->"));

    let r = post_form(
        gw,
        "/v1/audio/translations",
        audio_form("mock", Some("text"), &file),
    )
    .await;
    assert!(r.header("content-type").starts_with("text/plain"));
    assert_eq!(&r.body[..], b"Mock translation (64000 bytes)");

    // Ohne Datei, falscher Content-Type.
    let mut f = FormBuilder::new();
    f.text("model", "mock");
    assert_eq!(
        post_form(gw, "/v1/audio/transcriptions", f).await.status,
        400
    );
    let r = post(gw, "/v1/audio/transcriptions", json!({"model": "mock"})).await;
    assert_eq!(r.status, 400);
}

#[tokio::test]
async fn audio_upload_limit() {
    let state = ProxyState {
        config: ProxyConfig {
            max_audio_bytes: 1000,
            ..ProxyConfig::default()
        },
        ..ProxyState::mock()
    };
    let gw = start(state).await;
    let r = post_form(
        gw,
        "/v1/audio/transcriptions",
        audio_form("mock", None, &[0u8; 2000]),
    )
    .await;
    assert_eq!(r.status, 413);
    assert_eq!(r.json()["error"]["type"], "invalid_request_error");
    // Andere Endpunkte sind nicht betroffen.
    let r = post(
        gw,
        "/v1/embeddings",
        json!({"model": "mock", "input": "x".repeat(1500)}),
    )
    .await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn speech_via_mock_is_streamed() {
    let (gw, capture) = mock_gateway().await;
    let r = post(
        gw,
        "/v1/audio/speech",
        json!({"model": "mock", "input": "Hallo", "voice": "alloy", "response_format": "wav"}),
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-type"), "audio/wav");
    assert_eq!(&r.body[..4], b"RIFF");
    assert_eq!(r.body.len(), 44 + 5 * 800);
    let rec = capture.nth(1).await;
    assert_eq!(rec.usage.characters, Some(5));
    assert!(rec.stream);
    assert!(!rec.client_aborted);
    let r = post(
        gw,
        "/v1/audio/speech",
        json!({"model": "mock", "input": "", "voice": "alloy"}),
    )
    .await;
    assert_eq!(r.status, 400);
}

#[tokio::test]
async fn responses_via_mock() {
    let (gw, capture) = mock_gateway().await;
    let r = post(
        gw,
        "/v1/responses",
        json!({"model": "mock", "instructions": "Kurz.", "input": "Hallo"}),
    )
    .await;
    assert_eq!(r.status, 200);
    let v = r.json();
    assert_eq!(v["object"], "response");
    assert_eq!(v["status"], "completed");
    assert_eq!(v["store"], false);
    assert_eq!(v["instructions"], "Kurz.");
    assert_eq!(v["output"][0]["type"], "message");
    assert_eq!(v["output"][0]["content"][0]["text"], "Echo: Hallo");
    assert!(v["usage"]["total_tokens"].as_u64().unwrap() > 0);
    let rec = capture.nth(1).await;
    assert_eq!(rec.ingress, WireFormat::OpenAiResponses);

    // Tool-Aufruf und Rückgabe des Ergebnisses im nächsten Zug.
    let tools = json!([{"type": "function", "name": "wetter", "parameters": {"type": "object"}}]);
    let r = post(
        gw,
        "/v1/responses",
        json!({"model": "mock", "input": "tool bitte", "tools": tools}),
    )
    .await;
    let v = r.json();
    let call = v["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "function_call")
        .unwrap()
        .clone();
    assert_eq!(call["name"], "wetter");
    let r = post(
        gw,
        "/v1/responses",
        json!({"model": "mock", "tools": tools, "input": [
            {"role": "user", "content": "tool bitte"},
            {"type": "function_call", "call_id": call["call_id"], "name": "wetter", "arguments": call["arguments"]},
            {"type": "function_call_output", "call_id": call["call_id"], "output": "sonnig"},
            {"role": "user", "content": "Danke"}
        ]}),
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["output"][0]["content"][0]["text"], "Echo: Danke");

    // Zustandsbehaftete Funktionen → 400 mit klarer Meldung.
    for body in [
        json!({"model": "mock", "input": "x", "previous_response_id": "resp_1"}),
        json!({"model": "mock", "input": "x", "store": true}),
    ] {
        let r = post(gw, "/v1/responses", body).await;
        assert_eq!(r.status, 400);
        let e = r.json();
        assert_eq!(e["error"]["code"], "unsupported_parameter");
        assert!(
            e["error"]["message"]
                .as_str()
                .unwrap()
                .contains("zustandslos")
        );
    }
}

#[tokio::test]
async fn responses_stream_via_mock() {
    let (gw, capture) = mock_gateway().await;
    let r = post(
        gw,
        "/v1/responses",
        json!({"model": "mock", "input": "Hallo Welt", "stream": true, "reasoning": {"effort": "low"}}),
    )
    .await;
    assert_eq!(r.status, 200);
    assert!(r.header("content-type").starts_with("text/event-stream"));
    let events = SseParser::new().push(&r.body);
    let names: Vec<String> = events
        .iter()
        .map(|e| e.event.clone().unwrap_or_default())
        .collect();
    assert_eq!(names[0], "response.created");
    assert_eq!(names[1], "response.in_progress");
    assert!(names.contains(&"response.reasoning_summary_text.delta".to_owned()));
    assert!(names.contains(&"response.output_text.delta".to_owned()));
    assert_eq!(names.last().unwrap(), "response.completed");
    let done: Value = serde_json::from_str(&events.last().unwrap().data).unwrap();
    let out = done["response"]["output"].as_array().unwrap();
    assert_eq!(out[0]["type"], "reasoning");
    assert_eq!(out[1]["content"][0]["text"], "Echo: Hallo Welt");
    assert!(done["response"]["usage"]["output_tokens"].as_u64().unwrap() > 0);
    let rec = capture.nth(1).await;
    assert!(rec.stream);
    assert!(rec.ttft.is_some());

    // Fehler mitten im Stream → error + response.failed.
    let r = post(
        gw,
        "/v1/responses",
        json!({"model": "mock-stream-error", "input": "Hallo Welt", "stream": true}),
    )
    .await;
    let names: Vec<String> = SseParser::new()
        .push(&r.body)
        .into_iter()
        .map(|e| e.event.unwrap_or_default())
        .collect();
    assert_eq!(names[names.len() - 2..], ["error", "response.failed"]);
}

// ---------------------------------------------------------------------------------------------
// Fähigkeiten der Modellgruppen
// ---------------------------------------------------------------------------------------------

fn settings(v: Value) -> JsonObject {
    match v {
        Value::Object(o) => o,
        _ => JsonObject::new(),
    }
}

fn group(name: &str, policy: GroupPolicy, provider: Arc<dyn ChatProvider>) -> Arc<GroupRoute> {
    Arc::new(GroupRoute::new(
        name,
        RoutingStrategy::Weighted,
        policy,
        Vec::new(),
        vec![MemberSpec::new(provider)],
    ))
}

#[tokio::test]
async fn group_capabilities_are_enforced() {
    let mock: Arc<dyn ChatProvider> = Arc::new(MockProvider::new());
    let mut routes = StaticRoutes::new();
    routes.insert_route(
        "chat-only",
        group("chat-only", GroupPolicy::default(), mock.clone()),
    );
    let emb =
        GroupPolicy::from_settings(&settings(json!({"capabilities": ["embeddings"]}))).unwrap();
    routes.insert_route("emb-only", group("emb-only", emb.clone(), mock.clone()));
    // Anthropic-Deployment in einer Gruppe, die Embeddings freigibt: kein Deployment kann sie.
    let anthropic = deployment(
        "claude",
        ProviderKind::Anthropic,
        "http://127.0.0.1:9",
        "claude-x",
        &[("apiKey", "k")],
        json!({}),
    );
    let up = Upstream::from_deployment(
        &anthropic,
        &Registry::with_builtin(),
        &UpstreamClient::unguarded().unwrap(),
    )
    .unwrap();
    routes.insert_route("claude-emb", group("claude-emb", emb, Arc::new(up)));
    let gw = start(ProxyState::with_routes(Arc::new(routes))).await;

    let r = post(
        gw,
        "/v1/embeddings",
        json!({"model": "chat-only", "input": "x"}),
    )
    .await;
    assert_eq!(r.status, 400);
    let e = r.json();
    assert_eq!(e["error"]["code"], "unsupported_capability");
    assert!(
        e["error"]["message"]
            .as_str()
            .unwrap()
            .contains("settings.capabilities")
    );

    let r = post(
        gw,
        "/v1/embeddings",
        json!({"model": "emb-only", "input": "x"}),
    )
    .await;
    assert_eq!(r.status, 200);
    // Chat an eine reine Embedding-Gruppe → 400 (auch über /v1/chat/completions und Anthropic).
    let r = post(
        gw,
        "/v1/chat/completions",
        json!({"model": "emb-only", "messages": [{"role": "user", "content": "x"}]}),
    )
    .await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"]["code"], "unsupported_capability");
    let r = post(
        gw,
        "/v1/responses",
        json!({"model": "emb-only", "input": "x"}),
    )
    .await;
    assert_eq!(r.status, 400);
    let r = post(
        gw,
        "/v1/messages",
        json!({"model": "emb-only", "max_tokens": 5, "messages": [{"role": "user", "content": "x"}]}),
    )
    .await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["type"], "error");

    let r = post(
        gw,
        "/v1/embeddings",
        json!({"model": "claude-emb", "input": "x"}),
    )
    .await;
    assert_eq!(r.status, 400);
    assert!(
        r.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("kein Deployment")
    );
    let r = post(
        gw,
        "/v1/images/generations",
        json!({"model": "chat-only", "prompt": "x"}),
    )
    .await;
    assert_eq!(r.status, 400);
}

// ---------------------------------------------------------------------------------------------
// Fake-Upstreams
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Seen {
    uri: String,
    headers: HeaderMap,
    body: Bytes,
}

impl Seen {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }

    fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
    }
}

#[derive(Default)]
struct Fake {
    seen: Mutex<Vec<Seen>>,
}

impl Fake {
    fn take(&self) -> Vec<Seen> {
        std::mem::take(&mut *self.seen.lock().unwrap())
    }
}

fn json_body(v: Value) -> Response {
    ([("content-type", "application/json")], v.to_string()).into_response()
}

/// Antwort je nach Pfad (alle Anbieter in einem Fake).
async fn fake_handler(
    State(fake): State<Arc<Fake>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    fake.seen.lock().unwrap().push(Seen {
        uri: uri.to_string(),
        headers: headers.clone(),
        body: body.clone(),
    });
    let path = uri.path();
    if path.ends_with("/embeddings") {
        let v: Value = serde_json::from_slice(&body).unwrap();
        if v["model"] == "fail-400" {
            return (
                StatusCode::BAD_REQUEST,
                json!({"error": {"message": "input zu lang", "type": "invalid_request_error", "code": null}})
                    .to_string(),
            )
                .into_response();
        }
        let n = match &v["input"] {
            Value::Array(a) => a.len(),
            _ => 1,
        };
        let base64 = v["encoding_format"] == "base64";
        let data: Vec<Value> = (0..n)
            .map(|i| {
                let vec = [0.5f32, i as f32];
                let emb = if base64 {
                    json!(base64::encode_f32(&vec))
                } else {
                    json!(vec)
                };
                json!({"object": "embedding", "index": i, "embedding": emb})
            })
            .collect();
        return json_body(json!({"object": "list", "model": v["model"], "data": data,
                                "usage": {"prompt_tokens": 3 * n, "total_tokens": 3 * n}}));
    }
    if path.ends_with("/images/generations") {
        return json_body(
            json!({"created": 42, "data": [{"b64_json": "QUJD", "revised_prompt": "rp"}],
                                "usage": {"input_tokens": 5, "output_tokens": 50, "total_tokens": 55}}),
        );
    }
    if path.ends_with("/audio/transcriptions") || path.ends_with("/audio/translations") {
        let ct = headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        let parts = multipart::parse(&body, multipart::boundary(ct).unwrap()).unwrap();
        let format = parts
            .iter()
            .find(|p| p.name == "response_format")
            .and_then(|p| p.text())
            .unwrap_or("json")
            .to_owned();
        return if format == "text" {
            ([("content-type", "text/plain")], "hallo welt").into_response()
        } else {
            json_body(json!({"text": "hallo welt", "usage": {"type": "duration", "seconds": 2}}))
        };
    }
    if path.ends_with("/audio/speech") {
        let s = futures_util::stream::iter(
            [
                Bytes::from_static(b"ID3"),
                Bytes::from_static(b"abc"),
                Bytes::from_static(b"def"),
            ]
            .map(Ok::<_, std::io::Error>),
        );
        return ([("content-type", "audio/mpeg")], Body::from_stream(s)).into_response();
    }
    if path.ends_with("/invoke") {
        let v: Value = serde_json::from_slice(&body).unwrap();
        if let Some(t) = v.get("inputText") {
            let len = t.as_str().unwrap().len() as f64;
            return json_body(json!({"embedding": [len, 1.0], "inputTextTokenCount": 4}));
        }
        let texts = v["texts"].as_array().unwrap();
        let vecs: Vec<Value> = texts
            .iter()
            .map(|t| json!([t.as_str().unwrap().len() as f64, 2.0]))
            .collect();
        return (
            [
                ("content-type", "application/json"),
                ("x-amzn-bedrock-input-token-count", "9"),
            ],
            json!({"id": "x", "embeddings": {"float": vecs}, "response_type": "embeddings_by_type"})
                .to_string(),
        )
            .into_response();
    }
    if path.ends_with(":batchEmbedContents") {
        let v: Value = serde_json::from_slice(&body).unwrap();
        let embs: Vec<Value> = v["requests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| json!({"values": [r["content"]["parts"][0]["text"].as_str().unwrap().len() as f64]}))
            .collect();
        return json_body(json!({"embeddings": embs}));
    }
    if path.ends_with(":predict") {
        let v: Value = serde_json::from_slice(&body).unwrap();
        if v["instances"][0].get("prompt").is_some() {
            return json_body(json!({"predictions": [
                {"bytesBase64Encoded": "UE5H", "mimeType": "image/png"},
                {"raiFilteredReason": "gefiltert"}
            ]}));
        }
        let preds: Vec<Value> = v["instances"]
            .as_array()
            .unwrap()
            .iter()
            .map(|_| json!({"embeddings": {"values": [0.25, 0.75], "statistics": {"token_count": 5.0, "truncated": false}}}))
            .collect();
        return json_body(json!({"predictions": preds}));
    }
    (StatusCode::NOT_FOUND, "unbekannter Pfad").into_response()
}

async fn start_fake() -> (String, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    let app = Router::new()
        .fallback(fake_handler)
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), fake)
}

fn deployment(
    name: &str,
    provider: ProviderKind,
    endpoint: &str,
    model: &str,
    creds: &[(&str, &str)],
    settings: Value,
) -> DeploymentCtx {
    let Value::Object(settings) = settings else {
        panic!()
    };
    DeploymentCtx {
        deployment: Deployment {
            id: new_id(),
            metadata: ObjectMeta::named(name),
            spec: DeploymentSpec {
                model_group: name.into(),
                provider,
                endpoint: Some(endpoint.into()),
                provider_model: model.into(),
                credential: (!creds.is_empty()).then(|| "c".into()),
                weight: 1,
                priority: 0,
                capacity: Default::default(),
                enabled: true,
                settings,
            },
        },
        credential: (!creds.is_empty()).then(|| {
            Arc::new(CredentialData(
                creds
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), SecretString::new(*v)))
                    .collect::<BTreeMap<_, _>>(),
            ))
        }),
    }
}

struct Env {
    gw: SocketAddr,
    fake: Arc<Fake>,
    capture: Arc<Capture>,
}

async fn env() -> Env {
    let (base, fake) = start_fake().await;
    let registry = Registry::with_builtin();
    let client = UpstreamClient::unguarded().unwrap();
    let mut routes = StaticRoutes::new();
    let fast = json!({"timeouts": {"firstByteMs": 2000, "idleMs": 1000, "totalMs": 5000}});
    let mut add = |alias: &str, d: DeploymentCtx| {
        routes.insert(
            alias,
            Arc::new(Upstream::from_deployment(&d, &registry, &client).unwrap()),
        );
    };
    let v1 = format!("{base}/v1");
    add(
        "openai",
        deployment(
            "openai",
            ProviderKind::OpenAi,
            &v1,
            "text-embedding-3-small",
            &[("apiKey", "sk-up")],
            fast.clone(),
        ),
    );
    add(
        "openai-fail",
        deployment(
            "openai-fail",
            ProviderKind::OpenAi,
            &v1,
            "fail-400",
            &[("apiKey", "sk-up")],
            fast.clone(),
        ),
    );
    add(
        "azure",
        deployment(
            "azure",
            ProviderKind::AzureOpenAi,
            &base,
            "my-dep",
            &[("apiKey", "az-key")],
            fast.clone(),
        ),
    );
    add(
        "compat",
        deployment(
            "compat",
            ProviderKind::OpenAiCompatible,
            &v1,
            "bge-m3",
            &[],
            fast.clone(),
        ),
    );
    add(
        "ollama",
        deployment(
            "ollama",
            ProviderKind::Ollama,
            &v1,
            "nomic-embed-text",
            &[],
            fast.clone(),
        ),
    );
    add(
        "mistral",
        deployment(
            "mistral",
            ProviderKind::Mistral,
            &v1,
            "mistral-embed",
            &[("apiKey", "m-key")],
            fast.clone(),
        ),
    );
    let mut bedrock = fast.clone();
    bedrock["region"] = json!("eu-central-1");
    add(
        "titan",
        deployment(
            "titan",
            ProviderKind::Bedrock,
            &base,
            "amazon.titan-embed-text-v2:0",
            &[
                ("accessKeyId", "AKIDEXAMPLE"),
                ("secretAccessKey", "secret"),
            ],
            bedrock.clone(),
        ),
    );
    add(
        "cohere",
        deployment(
            "cohere",
            ProviderKind::Bedrock,
            &base,
            "cohere.embed-multilingual-v3",
            &[("apiKey", "br-key")],
            bedrock,
        ),
    );
    add(
        "gemini",
        deployment(
            "gemini",
            ProviderKind::Vertex,
            &format!("{base}/v1beta"),
            "gemini-embedding-001",
            &[("apiKey", "g-key")],
            fast.clone(),
        ),
    );
    let mut vertex = fast.clone();
    vertex["project"] = json!("p1");
    vertex["location"] = json!("europe-west4");
    add(
        "vertex",
        deployment(
            "vertex",
            ProviderKind::Vertex,
            &format!("{base}/v1"),
            "text-embedding-005",
            &[("accessToken", "ya29.tok")],
            vertex.clone(),
        ),
    );
    add(
        "imagen",
        deployment(
            "imagen",
            ProviderKind::Vertex,
            &format!("{base}/v1"),
            "imagen-4.0-generate-001",
            &[("accessToken", "ya29.tok")],
            vertex,
        ),
    );
    let capture = Arc::new(Capture::default());
    let state = ProxyState {
        observer: capture.clone(),
        ..ProxyState::with_routes(Arc::new(routes))
    };
    Env {
        gw: start(state).await,
        fake,
        capture,
    }
}

#[tokio::test]
async fn openai_family_embeddings() {
    let e = env().await;
    // OpenAI: Base64 zum Anbieter, Zahlen zum Client (Standard).
    let r = post(
        e.gw,
        "/v1/embeddings",
        json!({"model": "openai", "input": ["a", "b"], "dimensions": 2, "user": "u1"}),
    )
    .await;
    assert_eq!(r.status, 200);
    let v = r.json();
    assert_eq!(v["data"][1]["embedding"], json!([0.5, 1.0]));
    assert_eq!(v["usage"]["prompt_tokens"], 6);
    let seen = e.fake.take();
    assert!(seen[0].uri.ends_with("/v1/embeddings"));
    assert_eq!(seen[0].header("authorization"), "Bearer sk-up");
    let body = seen[0].json();
    assert_eq!(body["encoding_format"], "base64");
    assert_eq!(body["model"], "text-embedding-3-small");
    assert_eq!(body["dimensions"], 2);
    assert_eq!(body["user"], "u1");
    let rec = e.capture.nth(1).await;
    assert_eq!(rec.provider, Some(ProviderKind::OpenAi));
    assert_eq!(rec.usage.input_tokens, 6);

    // Azure: Deployment-URL mit api-version, api-key.
    let r = post(
        e.gw,
        "/v1/embeddings",
        json!({"model": "azure", "input": "a", "encoding_format": "base64"}),
    )
    .await;
    assert_eq!(r.status, 200);
    assert!(r.json()["data"][0]["embedding"].is_string());
    let seen = e.fake.take();
    assert!(
        seen[0]
            .uri
            .ends_with("/openai/deployments/my-dep/embeddings?api-version=2024-10-21"),
        "{}",
        seen[0].uri
    );
    assert_eq!(seen[0].header("api-key"), "az-key");

    // Kompatibel/Ollama: Zahlen, ohne encoding_format, ohne Auth.
    for model in ["compat", "ollama"] {
        let r = post(
            e.gw,
            "/v1/embeddings",
            json!({"model": model, "input": "a"}),
        )
        .await;
        assert_eq!(r.status, 200, "{model}");
        let seen = e.fake.take();
        assert!(seen[0].json().get("encoding_format").is_none());
        assert!(seen[0].headers.get("authorization").is_none());
        assert_eq!(seen[0].json()["input"], "a");
    }

    // Mistral: output_dimension statt dimensions, kein user.
    let r = post(
        e.gw,
        "/v1/embeddings",
        json!({"model": "mistral", "input": ["a"], "dimensions": 8, "user": "u"}),
    )
    .await;
    assert_eq!(r.status, 200);
    let body = e.fake.take()[0].json();
    assert_eq!(body["output_dimension"], 8);
    assert!(body.get("dimensions").is_none() && body.get("user").is_none());

    // Fehler des Anbieters → 400 im OpenAI-Format.
    let r = post(
        e.gw,
        "/v1/embeddings",
        json!({"model": "openai-fail", "input": "a"}),
    )
    .await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"]["message"], "input zu lang");
}

#[tokio::test]
async fn openai_images_and_audio() {
    let e = env().await;
    let r = post(
        e.gw,
        "/v1/images/generations",
        json!({"model": "openai", "prompt": "Katze", "size": "1024x1024", "background": "transparent"}),
    )
    .await;
    assert_eq!(r.status, 200);
    let v = r.json();
    assert_eq!(v["data"][0]["b64_json"], "QUJD");
    assert_eq!(v["usage"]["total_tokens"], 55);
    let body = e.fake.take()[0].json();
    assert_eq!(body["background"], "transparent");
    assert_eq!(body["model"], "text-embedding-3-small");
    let rec = e.capture.nth(1).await;
    assert_eq!(rec.usage.images, Some(1));
    assert_eq!(rec.usage.output_tokens, 50);

    // Azure-Bilder.
    let r = post(
        e.gw,
        "/v1/images/generations",
        json!({"model": "azure", "prompt": "Katze"}),
    )
    .await;
    assert_eq!(r.status, 200);
    assert!(
        e.fake.take()[0]
            .uri
            .contains("/openai/deployments/my-dep/images/generations?api-version=")
    );

    // Transkription: Formular mit allen Feldern und Datei kommt beim Anbieter an.
    let mut f = audio_form("openai", Some("verbose_json"), b"RIFF1234");
    f.text("language", "de")
        .text("timestamp_granularities[]", "word");
    let r = post_form(e.gw, "/v1/audio/transcriptions", f).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["text"], "hallo welt");
    let seen = e.fake.take();
    let ct = seen[0].header("content-type").to_owned();
    let parts = multipart::parse(&seen[0].body, multipart::boundary(&ct).unwrap()).unwrap();
    let field = |n: &str| parts.iter().find(|p| p.name == n).unwrap().clone();
    assert_eq!(field("model").text(), Some("text-embedding-3-small"));
    assert_eq!(field("language").text(), Some("de"));
    assert_eq!(field("timestamp_granularities[]").text(), Some("word"));
    assert_eq!(&field("file").data[..], b"RIFF1234");
    assert_eq!(field("file").filename.as_deref(), Some("probe.wav"));
    let rec = e.capture.nth(3).await;
    assert_eq!(rec.usage.audio_ms, Some(2000));

    // Übersetzung als Text über kompatiblen Anbieter.
    let r = post_form(
        e.gw,
        "/v1/audio/translations",
        audio_form("compat", Some("text"), b"x"),
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(&r.body[..], b"hallo welt");
    assert!(e.fake.take()[0].uri.ends_with("/v1/audio/translations"));

    // Sprachausgabe wird gestreamt durchgereicht.
    let r = post(
        e.gw,
        "/v1/audio/speech",
        json!({"model": "azure", "input": "Hallo", "voice": "nova"}),
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-type"), "audio/mpeg");
    assert_eq!(&r.body[..], b"ID3abcdef");
    let seen = e.fake.take();
    assert!(
        seen[0]
            .uri
            .contains("/openai/deployments/my-dep/audio/speech?api-version=")
    );
    assert_eq!(seen[0].json()["voice"], "nova");
    assert_eq!(seen[0].header("accept"), "audio/mpeg");

    // Ollama kann kein Audio → Gruppe (StaticRoutes: alle Fähigkeiten) hat kein fähiges Deployment.
    let r = post(
        e.gw,
        "/v1/audio/speech",
        json!({"model": "ollama", "input": "x", "voice": "a"}),
    )
    .await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"]["code"], "unsupported_capability");
}

#[tokio::test]
async fn bedrock_embeddings() {
    let e = env().await;
    // Titan: eine Anfrage je Eingabe, SigV4, Reihenfolge bleibt erhalten.
    let r = post(
        e.gw,
        "/v1/embeddings",
        json!({"model": "titan", "input": ["a", "bbb", "cc"], "dimensions": 256}),
    )
    .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let v = r.json();
    let firsts: Vec<f64> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["embedding"][0].as_f64().unwrap())
        .collect();
    assert_eq!(firsts, [1.0, 3.0, 2.0]);
    assert_eq!(v["usage"]["prompt_tokens"], 12);
    let seen = e.fake.take();
    assert_eq!(seen.len(), 3);
    assert!(
        seen[0]
            .uri
            .ends_with("/model/amazon.titan-embed-text-v2%3A0/invoke"),
        "{}",
        seen[0].uri
    );
    assert!(
        seen[0]
            .header("authorization")
            .starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/")
    );
    assert_eq!(seen[0].json()["dimensions"], 256);

    // Cohere: Stapel, input_type aus der Anfrage, Tokens aus dem Header.
    let r = post(
        e.gw,
        "/v1/embeddings",
        json!({"model": "cohere", "input": ["a", "bb"], "input_type": "search_query"}),
    )
    .await;
    assert_eq!(r.status, 200);
    let v = r.json();
    assert_eq!(v["data"][1]["embedding"], json!([2.0, 2.0]));
    assert_eq!(v["usage"]["prompt_tokens"], 9);
    let seen = e.fake.take();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].header("authorization"), "Bearer br-key");
    let body = seen[0].json();
    assert_eq!(body["input_type"], "search_query");
    assert_eq!(body["embedding_types"], json!(["float"]));

    // Token-IDs kann Bedrock nicht.
    let r = post(
        e.gw,
        "/v1/embeddings",
        json!({"model": "titan", "input": [1, 2, 3]}),
    )
    .await;
    assert_eq!(r.status, 400);
}

#[tokio::test]
async fn gemini_and_vertex_embeddings_and_imagen() {
    let e = env().await;
    let r = post(
        e.gw,
        "/v1/embeddings",
        json!({"model": "gemini", "input": ["ab", "abcd"], "dimensions": 3}),
    )
    .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let v = r.json();
    assert_eq!(v["data"][1]["embedding"], json!([4.0]));
    let seen = e.fake.take();
    assert!(
        seen[0]
            .uri
            .ends_with("/v1beta/models/gemini-embedding-001:batchEmbedContents")
    );
    assert_eq!(seen[0].header("x-goog-api-key"), "g-key");
    let body = seen[0].json();
    assert_eq!(body["requests"][0]["model"], "models/gemini-embedding-001");
    assert_eq!(body["requests"][0]["outputDimensionality"], 3);
    // Gemini API meldet keine Tokens → geschätzt (≈ 4 Bytes je Token).
    let rec = e.capture.nth(1).await;
    assert!(rec.usage_estimated);
    assert_eq!(rec.usage.input_tokens, 2);

    let r = post(
        e.gw,
        "/v1/embeddings",
        json!({"model": "vertex", "input": ["a", "b"], "task_type": "RETRIEVAL_QUERY"}),
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["usage"]["prompt_tokens"], 10);
    let seen = e.fake.take();
    assert!(
        seen[0].uri.ends_with(
            "/v1/projects/p1/locations/europe-west4/publishers/google/models/text-embedding-005:predict"
        ),
        "{}",
        seen[0].uri
    );
    assert_eq!(seen[0].header("authorization"), "Bearer ya29.tok");
    assert_eq!(
        seen[0].json()["instances"][1]["task_type"],
        "RETRIEVAL_QUERY"
    );

    // Imagen: Base64, gefilterte Bilder fallen weg.
    let r = post(
        e.gw,
        "/v1/images/generations",
        json!({"model": "imagen", "prompt": "Berg", "n": 2, "size": "1792x1024"}),
    )
    .await;
    assert_eq!(r.status, 200);
    let v = r.json();
    assert_eq!(v["data"].as_array().unwrap().len(), 1);
    assert_eq!(v["data"][0]["b64_json"], "UE5H");
    let body = e.fake.take()[0].json();
    assert_eq!(body["parameters"]["sampleCount"], 2);
    assert_eq!(body["parameters"]["aspectRatio"], "16:9");
}
