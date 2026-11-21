// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Plugin `schema-guard` im Hot Path (AIGW-113): strukturierte Ausgabe (OpenAI
//! `response_format`, Anthropic `output_config.format`) gegen das Schema prüfen – Ablehnung
//! mit `content_filter`/`refusal` bzw. `502`, Retry über das Gateway (endgültige Anfrage nach
//! allen Anfrage-Plugins, Abrechnung als Plugin-Modellaufruf), gepufferter und Live-Stream,
//! nicht prüfbare Schemas.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aigw_core::ir::{
    ChatRequest, ChatResponse, Choice, Message, Role, StopReason, StreamEvent, UpstreamError, Usage,
};
use aigw_core::observe::{RequestObserver, RequestRecord};
use aigw_core::upstream::UpstreamPolicy;
use aigw_core::wire::sse::SseParser;
use aigw_plugins::{Chain, Entry, OutputRejectMode, PluginChains, PluginFactory, builtin};
use aigw_proxy::{
    BoxFuture, ChatProvider, Describe, EventStream, GatewayHost, ModelRoutes, ProxyState,
    StaticRoutes,
};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};

const OPENAI: &str = "/v1/chat/completions";
const ANTHROPIC: &str = "/v1/messages";
const VALID: &str = "{\"name\": \"Ada\", \"age\": 36}";

/// Anbieter: antwortet mit gültigem JSON, wenn die letzte Nachricht eine Korrektur des
/// Gateways ist oder der Prompt `VALID` enthält, sonst mit Freitext. Merkt sich die Anfragen.
#[derive(Default)]
struct Shaper {
    seen: Mutex<Vec<ChatRequest>>,
}

impl Shaper {
    fn answer(&self, req: &ChatRequest) -> (String, u64) {
        self.seen.lock().unwrap().push(req.clone());
        let last = req
            .messages
            .last()
            .map(|m| m.content.text())
            .unwrap_or_default();
        if last.contains("did not match the required JSON format") {
            (VALID.into(), 50)
        } else if last.contains("VALID") {
            (VALID.into(), 5)
        } else {
            ("Gern! Ada ist 36.".into(), 5)
        }
    }

    fn requests(&self) -> Vec<ChatRequest> {
        self.seen.lock().unwrap().clone()
    }
}

fn reply(text: String, tokens: u64) -> ChatResponse {
    ChatResponse {
        id: "r1".into(),
        model: "shaper".into(),
        created: Some(1),
        choices: vec![Choice {
            index: 0,
            message: Message::text(Role::Assistant, text),
            stop_reason: Some(StopReason::EndTurn),
            stop_sequence: None,
            extra: Default::default(),
        }],
        usage: Some(Usage {
            input_tokens: tokens,
            output_tokens: tokens,
            ..Usage::default()
        }),
        extra: Default::default(),
        source: None,
    }
}

impl ChatProvider for Shaper {
    fn describe(&self) -> Describe<'_> {
        Describe {
            deployment: "shaper",
            deployment_id: None,
            model_group: None,
            provider: None,
            provider_model: "shaper",
        }
    }

    fn chat(self: Arc<Self>, req: ChatRequest) -> BoxFuture<Result<ChatResponse, UpstreamError>> {
        let (text, tokens) = self.answer(&req);
        Box::pin(async move { Ok(reply(text, tokens)) })
    }

    fn chat_stream(
        self: Arc<Self>,
        req: ChatRequest,
    ) -> BoxFuture<Result<EventStream, UpstreamError>> {
        let (text, _) = self.answer(&req);
        let mut evs = vec![StreamEvent::Start {
            id: "s1".into(),
            model: "shaper".into(),
            created: Some(1),
        }];
        for c in text.as_bytes().chunks(4) {
            evs.push(StreamEvent::TextDelta {
                index: 0,
                text: String::from_utf8_lossy(c).into_owned(),
            });
        }
        evs.push(StreamEvent::Stop {
            reason: StopReason::EndTurn,
            stop_sequence: None,
        });
        evs.push(StreamEvent::Usage(Usage {
            input_tokens: 5,
            output_tokens: 5,
            ..Usage::default()
        }));
        Box::pin(async move {
            let s: EventStream = Box::pin(futures_util::stream::iter(evs));
            Ok(s)
        })
    }
}

#[derive(Debug, Default)]
struct Records(Mutex<Vec<RequestRecord>>);

impl RequestObserver for Records {
    fn record(&self, record: RequestRecord) {
        self.0.lock().unwrap().push(record);
    }
}

impl Records {
    async fn last(&self) -> RequestRecord {
        for _ in 0..200 {
            if let Some(r) = self.0.lock().unwrap().last() {
                return r.clone();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("kein Datensatz");
    }
}

struct Gw {
    addr: SocketAddr,
    shaper: Arc<Shaper>,
    records: Arc<Records>,
}

/// Gateway mit `schema-guard` (Konfiguration `cfg`) und optional weiteren, inneren Plugins.
async fn gateway(cfg: Value, reject: OutputRejectMode, inner: Vec<Entry>) -> Gw {
    let shaper = Arc::new(Shaper::default());
    let records = Arc::new(Records::default());
    let mut r = StaticRoutes::new();
    r.insert("mock", Arc::clone(&shaper) as Arc<dyn ChatProvider>);
    let routes: Arc<dyn ModelRoutes> = Arc::new(r);
    let mut st = ProxyState {
        observer: Arc::clone(&records) as Arc<dyn RequestObserver>,
        ..ProxyState::with_routes(routes)
    };
    let host = GatewayHost::with_defaults(
        Arc::clone(&st.routes),
        Arc::clone(st.limits.counters()),
        None,
        UpstreamPolicy::strict(),
    )
    .unwrap();
    let factory = builtin::schema_guard::Factory::new(Arc::new(host));
    let sg = Entry::new("sg", factory.build(&cfg).unwrap()).with_output_reject(reject);
    let mut entries = vec![Arc::new(sg)];
    entries.extend(inner.into_iter().map(Arc::new));
    st.plugins = Arc::new(PluginChains::fixed(Chain::new(entries)));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, aigw_proxy::router(st)).await.unwrap();
    });
    Gw {
        addr,
        shaper,
        records,
    }
}

struct Resp {
    status: StatusCode,
    headers: hyper::HeaderMap,
    body: Bytes,
}

impl Resp {
    fn raw(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.raw()))
    }

    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .map(|v| v.to_str().unwrap().to_owned())
    }

    /// Text aus JSON (OpenAI/Anthropic) oder SSE.
    fn text(&self) -> String {
        if let Ok(v) = serde_json::from_slice::<Value>(&self.body) {
            if let Some(t) = v["choices"][0]["message"]["content"].as_str() {
                return t.to_owned();
            }
            if let Some(t) = v["content"][0]["text"].as_str() {
                return t.to_owned();
            }
            return String::new();
        }
        let mut out = String::new();
        for f in SseParser::new().push(&self.body) {
            let Ok(v) = serde_json::from_str::<Value>(&f.data) else {
                continue;
            };
            if let Some(t) = v["choices"][0]["delta"]["content"].as_str() {
                out.push_str(t);
            }
            if let Some(t) = v["delta"]["text"].as_str() {
                out.push_str(t);
            }
        }
        out
    }
}

async fn post(addr: SocketAddr, path: &str, body: Value) -> Resp {
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("http://{addr}{path}"))
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
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

fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {"name": {"type": "string"}, "age": {"type": "integer"}},
        "required": ["name", "age"],
        "additionalProperties": false
    })
}

/// Anfrage mit strukturierter Ausgabe im jeweiligen Eingangsformat.
fn body(path: &str, prompt: &str, stream: bool, schema: Value) -> Value {
    let mut b = json!({"model": "mock", "stream": stream, "max_tokens": 256,
                       "messages": [{"role": "user", "content": prompt}]});
    if path == OPENAI {
        b["response_format"] = json!({"type": "json_schema",
            "json_schema": {"name": "person", "schema": schema, "strict": true}});
    } else {
        b["output_config"] = json!({"format": {"type": "json_schema", "schema": schema}});
    }
    b
}

fn finish_reason(r: &Resp, path: &str) -> String {
    let raw = r.raw();
    if r.header("content-type")
        .is_some_and(|c| c.starts_with("text/event-stream"))
    {
        for f in SseParser::new().push(&r.body) {
            let Ok(v) = serde_json::from_str::<Value>(&f.data) else {
                continue;
            };
            if let Some(s) = v["choices"][0]["finish_reason"].as_str() {
                return s.to_owned();
            }
            if let Some(s) = v["delta"]["stop_reason"].as_str() {
                return s.to_owned();
            }
        }
        panic!("kein Ende im Stream: {raw}");
    }
    let v = r.json();
    if path == OPENAI {
        v["choices"][0]["finish_reason"]
            .as_str()
            .unwrap()
            .to_owned()
    } else {
        v["stop_reason"].as_str().unwrap().to_owned()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_output_and_requests_without_schema_pass() {
    let gw = gateway(json!({}), OutputRejectMode::Finish, Vec::new()).await;
    for path in [OPENAI, ANTHROPIC] {
        let r = post(gw.addr, path, body(path, "VALID bitte", false, schema())).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.raw());
        assert_eq!(r.text(), VALID);
        assert!(r.header("x-aigw-guardrail").is_none());
    }
    // Ohne strukturierte Ausgabe: Freitext bleibt.
    let r = post(
        gw.addr,
        OPENAI,
        json!({"model": "mock", "messages": [{"role": "user", "content": "Hallo"}]}),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.text(), "Gern! Ada ist 36.");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_output_rejected_in_both_formats() {
    let gw = gateway(json!({}), OutputRejectMode::Finish, Vec::new()).await;
    for (path, reason) in [(OPENAI, "content_filter"), (ANTHROPIC, "refusal")] {
        let r = post(gw.addr, path, body(path, "Wer ist Ada?", false, schema())).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.raw());
        assert_eq!(finish_reason(&r, path), reason);
        assert_eq!(r.text(), "");
        assert_eq!(
            r.header("x-aigw-guardrail").as_deref(),
            Some("schema-guard")
        );
    }
    let rec = gw.records.last().await;
    assert_eq!(rec.guardrails[0].plugin, "schema-guard");
    assert_eq!(rec.guardrails[0].category, "invalid-json");
    assert_eq!(rec.guardrails[0].action, "reject");

    let gw = gateway(json!({}), OutputRejectMode::Error, Vec::new()).await;
    let r = post(
        gw.addr,
        OPENAI,
        body(OPENAI, "Wer ist Ada?", false, schema()),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_GATEWAY, "{}", r.raw());
    assert_eq!(r.json()["error"]["code"], "schema_validation_failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsafe_request_schema_rejected_before_upstream() {
    let gw = gateway(json!({}), OutputRejectMode::Finish, Vec::new()).await;
    let bad = json!({"$ref": "http://169.254.169.254/latest/meta-data"});
    for path in [OPENAI, ANTHROPIC] {
        let r = post(gw.addr, path, body(path, "VALID", false, bad.clone())).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.raw());
        assert!(r.raw().contains("nicht prüfbar"), "{}", r.raw());
    }
    if let Some(code) = post(gw.addr, OPENAI, body(OPENAI, "VALID", false, bad))
        .await
        .json()["error"]["code"]
        .as_str()
    {
        assert_eq!(code, "response_schema_rejected");
    }
    assert!(gw.shaper.requests().is_empty(), "kein Upstream-Aufruf");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_fixes_output_and_bills_as_plugin_call() {
    let gw = gateway(
        json!({"action": "retry"}),
        OutputRejectMode::Finish,
        Vec::new(),
    )
    .await;
    for path in [OPENAI, ANTHROPIC] {
        let r = post(gw.addr, path, body(path, "Wer ist Ada?", false, schema())).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.raw());
        assert_eq!(r.text(), VALID);
        assert_eq!(
            r.header("x-aigw-schema-guard").as_deref(),
            Some("retried=1")
        );
        // Usage der Antwort = erster Aufruf.
        let v = r.json();
        let input = v["usage"]["prompt_tokens"]
            .as_u64()
            .or_else(|| v["usage"]["input_tokens"].as_u64());
        assert_eq!(input, Some(5), "{v}");
    }
    let rec = gw.records.last().await;
    assert_eq!(rec.usage.input_tokens, 5);
    assert_eq!(rec.plugin_usage.input_tokens, 50);
    assert!(
        rec.guardrails
            .iter()
            .any(|h| h.category == "invalid-json" && h.action == "retry")
    );
    // Wiederholung: nicht gestreamt, mit fehlerhafter Antwort und Korrektur.
    let reqs = gw.shaper.requests();
    let retry = &reqs[reqs.len() - 1];
    assert_eq!(retry.stream, Some(false));
    assert_eq!(retry.messages.len(), 3);
    assert_eq!(retry.messages[1].content.text(), "Gern! Ada ist 36.");
    assert!(retry.response_format.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_uses_final_request_after_inner_plugins() {
    // pii (innen, nach schema-guard) maskiert die Eingabe – die Wiederholung darf die
    // Originaldaten nicht an das Modell schicken.
    let pii = Entry::new(
        "pii",
        builtin::pii::Factory
            .build(&json!({"action": "mask", "direction": "input"}))
            .unwrap(),
    );
    let gw = gateway(
        json!({"action": "retry"}),
        OutputRejectMode::Finish,
        vec![pii],
    )
    .await;
    let r = post(
        gw.addr,
        OPENAI,
        body(OPENAI, "Ich bin bob@example.com", false, schema()),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.raw());
    assert_eq!(r.text(), VALID);
    let reqs = gw.shaper.requests();
    assert_eq!(reqs.len(), 2);
    for req in &reqs {
        let first = req.messages[0].content.text();
        assert!(!first.contains("bob@example.com"), "{first}");
        assert!(first.contains("[EMAIL_1]"), "{first}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn buffered_stream_supports_retry() {
    let gw = gateway(
        json!({"action": "retry", "stream": "buffer"}),
        OutputRejectMode::Finish,
        Vec::new(),
    )
    .await;
    for path in [OPENAI, ANTHROPIC] {
        let r = post(gw.addr, path, body(path, "Wer ist Ada?", true, schema())).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.raw());
        assert!(r.header("x-aigw-buffered").is_some());
        assert_eq!(r.text(), VALID, "{}", r.raw());
        assert_eq!(
            r.header("x-aigw-schema-guard").as_deref(),
            Some("retried=1")
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_stream_checked_at_end() {
    let gw = gateway(json!({}), OutputRejectMode::Finish, Vec::new()).await;
    for (path, reason) in [(OPENAI, "content_filter"), (ANTHROPIC, "refusal")] {
        let r = post(gw.addr, path, body(path, "Wer ist Ada?", true, schema())).await;
        assert_eq!(r.status, StatusCode::OK);
        assert!(r.header("x-aigw-buffered").is_none());
        assert_eq!(finish_reason(&r, path), reason, "{}", r.raw());
        let r = post(gw.addr, path, body(path, "VALID", true, schema())).await;
        assert_eq!(r.text(), VALID);
        assert_ne!(finish_reason(&r, path), reason);
    }
    // flag: Stream unverändert, Treffer gezählt.
    let gw = gateway(
        json!({"action": "flag"}),
        OutputRejectMode::Finish,
        Vec::new(),
    )
    .await;
    let r = post(
        gw.addr,
        OPENAI,
        body(OPENAI, "Wer ist Ada?", true, schema()),
    )
    .await;
    assert_eq!(finish_reason(&r, OPENAI), "stop");
    let rec = gw.records.last().await;
    assert_eq!(rec.guardrails[0].action, "flag");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_object_mode() {
    let gw = gateway(
        json!({"action": "flag"}),
        OutputRejectMode::Finish,
        Vec::new(),
    )
    .await;
    let r = post(
        gw.addr,
        OPENAI,
        json!({"model": "mock", "response_format": {"type": "json_object"},
               "messages": [{"role": "user", "content": "Wer ist Ada?"}]}),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header("x-aigw-schema-guard").as_deref(), Some("invalid"));
    let r = post(
        gw.addr,
        OPENAI,
        json!({"model": "mock", "response_format": {"type": "json_object"},
               "messages": [{"role": "user", "content": "VALID"}]}),
    )
    .await;
    assert!(r.header("x-aigw-schema-guard").is_none());
}
