// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! OTLP-Export Ende-zu-Ende gegen einen lokalen Fake-Collector (axum), der die Protobuf-Bodies
//! annimmt und hier unabhängig vom Encoder dekodiert wird (generischer Wire-Format-Parser,
//! Feldnummern nach `opentelemetry-proto`).
//!
//! - Traces über den echten Proxy (Router + `Upstream` gegen einen Fake-OpenAI-Server):
//!   Wurzel-Span mit GenAI-Attributen, Kind-Spans auth/plugins/route/upstream, TTFT-Ereignis,
//!   eingehender `traceparent` (gesampelt und nicht gesampelt) und Weitergabe an den Upstream.
//! - Logs: Request-Log → `OtlpLogExporter` → `/v1/logs`, Fehlerzählung bei HTTP 500,
//!   Wiederholung bei 429/503 mit `Retry-After` (F102).
//!
//! Keine externen Aufrufe. Die Tests nutzen eine Single-Thread-Runtime, damit der
//! Test-Subscriber (`set_default`, thread-lokal) auch für die Server-Tasks gilt.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aigw_core::config::{ObserveConfig, OtlpConfig, OtlpRetryConfig, OtlpTracesConfig};
use aigw_core::ir::Usage;
use aigw_core::observe::RequestRecord;
use aigw_core::resource::{
    CredentialData, Deployment, DeploymentSpec, JsonObject, ObjectMeta, SecretString, new_id,
};
use aigw_core::snapshot::DeploymentCtx;
use aigw_core::types::ProviderKind;
use aigw_core::wire::WireFormat;
use aigw_observe::ObserveBuilder;
use aigw_observe::otel::{OtlpLogExporter, OtlpMetrics, test_subscriber};
use aigw_providers::{Registry, Upstream, UpstreamClient};
use aigw_proxy::{ProxyState, StaticRoutes};
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------------------------
// Protobuf-Wire-Format (unabhängig vom Encoder)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum W {
    V(u64),
    F64(u64),
    F32(u32),
    B(Vec<u8>),
}

fn varint(b: &mut &[u8]) -> u64 {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let byte = b[0];
        *b = &b[1..];
        v |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return v;
        }
        shift += 7;
    }
}

fn parse(mut b: &[u8]) -> Vec<(u32, W)> {
    let mut out = Vec::new();
    while !b.is_empty() {
        let key = varint(&mut b);
        let field = u32::try_from(key >> 3).unwrap();
        let w = match key & 7 {
            0 => W::V(varint(&mut b)),
            1 => {
                let v = u64::from_le_bytes(b[..8].try_into().unwrap());
                b = &b[8..];
                W::F64(v)
            }
            2 => {
                let len = usize::try_from(varint(&mut b)).unwrap();
                let v = b[..len].to_vec();
                b = &b[len..];
                W::B(v)
            }
            5 => {
                let v = u32::from_le_bytes(b[..4].try_into().unwrap());
                b = &b[4..];
                W::F32(v)
            }
            t => panic!("unerwarteter Wire-Typ {t}"),
        };
        out.push((field, w));
    }
    out
}

fn bytes_of(m: &[(u32, W)], f: u32) -> Vec<Vec<u8>> {
    m.iter()
        .filter(|(n, _)| *n == f)
        .map(|(_, w)| match w {
            W::B(b) => b.clone(),
            other => panic!("Feld {f}: erwartet LEN, erhalten {other:?}"),
        })
        .collect()
}

fn one_bytes(m: &[(u32, W)], f: u32) -> Option<Vec<u8>> {
    bytes_of(m, f).into_iter().next()
}

fn string(m: &[(u32, W)], f: u32) -> String {
    one_bytes(m, f)
        .map(|b| String::from_utf8(b).unwrap())
        .unwrap_or_default()
}

fn uint(m: &[(u32, W)], f: u32) -> u64 {
    m.iter()
        .find(|(n, _)| *n == f)
        .map(|(_, w)| match w {
            W::V(v) | W::F64(v) => *v,
            W::F32(v) => u64::from(*v),
            W::B(_) => panic!("Feld {f}: LEN statt Zahl"),
        })
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq)]
enum Attr {
    S(String),
    B(bool),
    I(i64),
    D(f64),
}

fn attrs(m: &[(u32, W)], f: u32) -> HashMap<String, Attr> {
    bytes_of(m, f)
        .into_iter()
        .map(|kv| {
            let kv = parse(&kv);
            let key = string(&kv, 1);
            let v = parse(&one_bytes(&kv, 2).unwrap());
            let (n, w) = v.first().cloned().unwrap();
            let val = match (n, w) {
                (1, W::B(s)) => Attr::S(String::from_utf8(s).unwrap()),
                (2, W::V(b)) => Attr::B(b != 0),
                (3, W::V(i)) => Attr::I(i as i64),
                (4, W::F64(d)) => Attr::D(f64::from_bits(d)),
                other => panic!("AnyValue {other:?}"),
            };
            (key, val)
        })
        .collect()
}

#[derive(Debug, Clone)]
struct Span {
    trace_id: String,
    span_id: String,
    parent: Option<String>,
    name: String,
    kind: u64,
    start: u64,
    end: u64,
    attrs: HashMap<String, Attr>,
    events: Vec<(String, HashMap<String, Attr>)>,
    status: u64,
}

impl Span {
    fn s(&self, k: &str) -> &str {
        match self.attrs.get(k) {
            Some(Attr::S(s)) => s,
            other => panic!("{}: Attribut {k} = {other:?}", self.name),
        }
    }

    fn i(&self, k: &str) -> i64 {
        match self.attrs.get(k) {
            Some(Attr::I(i)) => *i,
            other => panic!("{}: Attribut {k} = {other:?}", self.name),
        }
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

struct Resource {
    attrs: HashMap<String, Attr>,
    scope: String,
}

/// `ExportTraceServiceRequest` → Ressource + Spans.
fn decode_traces(body: &[u8]) -> (Resource, Vec<Span>) {
    let req = parse(body);
    let rs = parse(&one_bytes(&req, 1).unwrap());
    let resource = parse(&one_bytes(&rs, 1).unwrap());
    let ss = parse(&one_bytes(&rs, 2).unwrap());
    let scope = parse(&one_bytes(&ss, 1).unwrap());
    let spans = bytes_of(&ss, 2)
        .into_iter()
        .map(|s| {
            let s = parse(&s);
            let status = one_bytes(&s, 15).map(|b| uint(&parse(&b), 3)).unwrap_or(0);
            Span {
                trace_id: hex(&one_bytes(&s, 1).unwrap()),
                span_id: hex(&one_bytes(&s, 2).unwrap()),
                parent: one_bytes(&s, 4).map(|b| hex(&b)),
                name: string(&s, 5),
                kind: uint(&s, 6),
                start: uint(&s, 7),
                end: uint(&s, 8),
                attrs: attrs(&s, 9),
                events: bytes_of(&s, 11)
                    .into_iter()
                    .map(|e| {
                        let e = parse(&e);
                        (string(&e, 2), attrs(&e, 3))
                    })
                    .collect(),
                status,
            }
        })
        .collect();
    (
        Resource {
            attrs: attrs(&resource, 1),
            scope: string(&scope, 1),
        },
        spans,
    )
}

// ---------------------------------------------------------------------------------------------
// Fake-Collector und Fake-Upstream
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Post {
    path: String,
    headers: HeaderMap,
    body: Bytes,
}

#[derive(Default)]
struct Collector {
    posts: Mutex<Vec<Post>>,
    fail: std::sync::atomic::AtomicBool,
    /// Vorgegebene Antworten (Status, `Retry-After`) für die nächsten Aufrufe.
    script: Mutex<std::collections::VecDeque<(u16, Option<&'static str>)>>,
}

async fn collect(
    State(c): State<Arc<Collector>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    c.posts.lock().unwrap().push(Post {
        path: uri.path().to_owned(),
        headers,
        body,
    });
    if let Some((status, retry_after)) = c.script.lock().unwrap().pop_front() {
        let mut resp = (StatusCode::from_u16(status).unwrap(), "nicht jetzt").into_response();
        if let Some(ra) = retry_after {
            resp.headers_mut()
                .insert("retry-after", http::HeaderValue::from_static(ra));
        }
        return resp;
    }
    if c.fail.load(std::sync::atomic::Ordering::SeqCst) {
        return (StatusCode::INTERNAL_SERVER_ERROR, "kaputt").into_response();
    }
    ([("content-type", "application/x-protobuf")], Bytes::new()).into_response()
}

async fn serve(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

async fn start_collector() -> (SocketAddr, Arc<Collector>) {
    let c = Arc::new(Collector::default());
    let app = Router::new()
        .route("/v1/traces", post(collect))
        .route("/v1/logs", post(collect))
        .with_state(Arc::clone(&c));
    (serve(app).await, c)
}

#[derive(Default)]
struct Upstream0 {
    seen: Mutex<Vec<HeaderMap>>,
}

fn chunk(delta: Value, finish: Option<&str>) -> String {
    format!(
        "data: {}\n\n",
        json!({"id": "c1", "object": "chat.completion.chunk", "created": 1, "model": "up-model",
               "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
    )
}

async fn upstream_handler(
    State(u): State<Arc<Upstream0>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    u.seen.lock().unwrap().push(headers);
    let body: Value = serde_json::from_slice(&body).unwrap();
    if body["stream"].as_bool().unwrap_or(false) {
        let parts = vec![
            chunk(json!({"role": "assistant", "content": "Hal"}), None),
            chunk(json!({"content": "lo"}), None),
            chunk(json!({}), Some("stop")),
            format!(
                "data: {}\n\n",
                json!({"id": "c1", "object": "chat.completion.chunk", "created": 1, "model": "up-model",
                       "choices": [], "usage": {"prompt_tokens": 7, "completion_tokens": 2, "total_tokens": 9}})
            ),
            "data: [DONE]\n\n".to_owned(),
        ];
        let s = futures_util::stream::iter(
            parts
                .into_iter()
                .map(|p| Ok::<_, std::io::Error>(Bytes::from(p))),
        );
        return (
            [("content-type", "text/event-stream")],
            Body::from_stream(s),
        )
            .into_response();
    }
    (
        [("content-type", "application/json")],
        json!({"id": "c1", "object": "chat.completion", "created": 1, "model": "up-model",
               "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hallo"},
                            "finish_reason": "stop"}],
               "usage": {"prompt_tokens": 11, "completion_tokens": 2, "total_tokens": 13}})
        .to_string(),
    )
        .into_response()
}

fn deployment(endpoint: &str) -> DeploymentCtx {
    DeploymentCtx {
        deployment: Deployment {
            id: new_id(),
            metadata: ObjectMeta::named("dep-ok"),
            spec: DeploymentSpec {
                model_group: "gruppe".into(),
                provider: ProviderKind::OpenAi,
                endpoint: Some(endpoint.into()),
                provider_model: "gpt-up".into(),
                credential: Some("c".into()),
                weight: 1,
                priority: 0,
                capacity: Default::default(),
                enabled: true,
                // Cloud-Anbieter geben den Trace-Kontext standardmäßig nicht weiter (F98).
                settings: JsonObject::from_iter([(
                    "propagateTraceContext".to_owned(),
                    Value::Bool(true),
                )]),
                budget: None,
            },
        },
        credential: Some(Arc::new(CredentialData(BTreeMap::from([(
            "apiKey".to_owned(),
            SecretString::new("sk-upstream"),
        )])))),
    }
}

/// Proxy mit einem Deployment (`ok`) vor dem Fake-Upstream.
/// Sammelt die Datensätze des Proxys (Trace-ID im Request-Log, F100).
#[derive(Debug, Default)]
struct Records(Mutex<Vec<RequestRecord>>);

impl aigw_core::observe::RequestObserver for Records {
    fn record(&self, record: RequestRecord) {
        self.0.lock().unwrap().push(record);
    }
}

static RECORDS: std::sync::OnceLock<Arc<Records>> = std::sync::OnceLock::new();

fn records() -> Arc<Records> {
    Arc::clone(RECORDS.get_or_init(Arc::default))
}

async fn start_gateway() -> (SocketAddr, Arc<Upstream0>) {
    let up = Arc::new(Upstream0::default());
    let up_addr = serve(
        Router::new()
            .route("/v1/chat/completions", post(upstream_handler))
            .with_state(Arc::clone(&up)),
    )
    .await;
    let d = deployment(&format!("http://{up_addr}/v1"));
    let upstream = Upstream::from_deployment(
        &d,
        &Registry::with_builtin(),
        &UpstreamClient::unguarded().unwrap(),
    )
    .unwrap();
    let mut routes = StaticRoutes::new();
    routes.insert("ok", Arc::new(upstream));
    let mut state = ProxyState::with_routes(Arc::new(routes));
    state.observer = records();
    let gw = serve(aigw_proxy::router(state)).await;
    (gw, up)
}

async fn post_json(addr: SocketAddr, body: Value, headers: &[(&str, &str)]) -> (u16, String) {
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let mut req = hyper::Request::post(format!("http://{addr}/v1/chat/completions"))
        .header("content-type", "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = client
        .request(req.body(Full::new(Bytes::from(body.to_string()))).unwrap())
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

fn otlp_cfg(collector: SocketAddr) -> OtlpConfig {
    OtlpConfig {
        endpoint: format!("http://{collector}"),
        headers: BTreeMap::from([("x-collector-token".to_owned(), "t0k".to_owned())]),
        traces: OtlpTracesConfig {
            enabled: true,
            flush_interval_ms: 50,
            ..OtlpTracesConfig::default()
        },
        ..OtlpConfig::default()
    }
}

const TRACE_ID: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const PARENT_ID: &str = "00f067aa0ba902b7";

// ---------------------------------------------------------------------------------------------
// Traces
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn traces_through_proxy_with_genai_attributes_and_propagation() {
    let (subscriber, activator) = test_subscriber();
    let _guard = tracing::subscriber::set_default(subscriber);
    let (collector_addr, collector) = start_collector().await;
    let metrics = Arc::new(OtlpMetrics::default());
    let exporter = activator
        .activate(&otlp_cfg(collector_addr), Arc::clone(&metrics))
        .unwrap();
    assert!(activator.is_active());
    let (gw, up) = start_gateway().await;

    // 1) Nicht gestreamt, mit gesampeltem eingehendem traceparent.
    let tp = format!("00-{TRACE_ID}-{PARENT_ID}-01");
    let (status, _) = post_json(
        gw,
        json!({"model": "ok", "max_tokens": 50, "messages": [{"role": "user", "content": "Hallo"}]}),
        &[("traceparent", &tp)],
    )
    .await;
    assert_eq!(status, 200);
    // 2) Gestreamt, ohne traceparent (neuer Trace, Sampling 1.0).
    let (status, body) = post_json(
        gw,
        json!({"model": "ok", "stream": true, "messages": [{"role": "user", "content": "Hallo"}]}),
        &[],
    )
    .await;
    assert_eq!(status, 200);
    assert!(body.contains("[DONE]"));
    // 3) Eingehend nicht gesampelt: nichts exportieren, aber Kontext weitergeben.
    let unsampled = "00-11111111111111111111111111111111-2222222222222222-00";
    let (status, _) = post_json(
        gw,
        json!({"model": "ok", "messages": [{"role": "user", "content": "Hallo"}]}),
        &[("traceparent", unsampled)],
    )
    .await;
    assert_eq!(status, 200);
    // 4) Unbekanntes Modell: Fehler am Wurzel- und Routing-Span.
    let (status, _) = post_json(
        gw,
        json!({"model": "fehlt", "messages": [{"role": "user", "content": "Hallo"}]}),
        &[],
    )
    .await;
    assert_eq!(status, 404);

    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(exporter.shutdown(Duration::from_secs(5)).await);

    let posts = collector.posts.lock().unwrap().clone();
    assert!(!posts.is_empty());
    let mut spans = Vec::new();
    for p in &posts {
        assert_eq!(p.path, "/v1/traces");
        assert_eq!(p.headers["content-type"], "application/x-protobuf");
        assert_eq!(p.headers["x-collector-token"], "t0k");
        let (res, s) = decode_traces(&p.body);
        assert_eq!(res.attrs["service.name"], Attr::S("aigw".into()));
        assert_eq!(res.scope, "aigw");
        spans.extend(s);
    }
    assert_eq!(metrics.spans_exported(), spans.len() as u64);
    assert_eq!(metrics.spans_dropped(), 0);
    let seen = up.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 3, "drei Upstream-Aufrufe");

    // F100: Trace-ID im Request-Log – auch ohne Sampling (Korrelation mit dem Client-Trace).
    let traced: Vec<Option<String>> = records()
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.trace_id.clone())
        .collect();
    assert!(traced.contains(&Some(TRACE_ID.to_owned())), "{traced:?}");
    assert!(traced.contains(&Some("11111111111111111111111111111111".to_owned())));

    // --- 1) Wurzel-Span übernimmt den eingehenden Kontext.
    let root = spans
        .iter()
        .find(|s| s.trace_id == TRACE_ID && s.parent.as_deref() == Some(PARENT_ID))
        .expect("Wurzel-Span mit eingehendem Kontext");
    assert_eq!(root.name, "chat ok");
    assert_eq!(root.kind, 2, "SERVER");
    assert!(root.end >= root.start && root.start > 0);
    assert_eq!(root.s("gen_ai.operation.name"), "chat");
    assert_eq!(root.s("gen_ai.request.model"), "ok");
    assert_eq!(root.s("gen_ai.response.model"), "up-model");
    assert_eq!(root.s("gen_ai.system"), "openai");
    assert_eq!(root.s("gen_ai.provider.name"), "openai");
    assert_eq!(root.i("gen_ai.usage.input_tokens"), 11);
    assert_eq!(root.i("gen_ai.usage.output_tokens"), 2);
    assert_eq!(root.i("gen_ai.request.max_tokens"), 50);
    assert_eq!(root.i("http.response.status_code"), 200);
    assert_eq!(root.s("aigw.model_group"), "gruppe");
    assert_eq!(root.s("aigw.deployment"), "dep-ok");
    assert_eq!(root.s("aigw.ingress"), "openai-chat");
    assert_eq!(root.s("aigw.request_id").len(), 36);
    assert!(!root.attrs.contains_key("otel.traceparent"));
    assert_eq!(root.status, 0);

    let children: Vec<&Span> = spans
        .iter()
        .filter(|s| s.parent.as_deref() == Some(root.span_id.as_str()))
        .collect();
    let names: Vec<&str> = children.iter().map(|s| s.name.as_str()).collect();
    for n in ["auth", "plugins", "route", "chat gpt-up"] {
        assert!(names.contains(&n), "Kind-Span {n} fehlt: {names:?}");
    }
    assert!(children.iter().all(|c| c.trace_id == TRACE_ID));
    let route = children.iter().find(|s| s.name == "route").unwrap();
    assert_eq!(route.s("aigw.model_group"), "ok");
    assert_eq!(root.i("aigw.attempts"), 1);
    let attempt = children.iter().find(|s| s.name == "chat gpt-up").unwrap();
    assert_eq!(attempt.kind, 3, "CLIENT");
    assert_eq!(attempt.s("gen_ai.system"), "openai");
    assert_eq!(attempt.s("gen_ai.provider.name"), "openai");
    assert_eq!(attempt.s("gen_ai.request.model"), "gpt-up");
    assert_eq!(attempt.s("server.address"), "127.0.0.1");
    assert_eq!(attempt.i("http.response.status_code"), 200);
    // Upstream bekam den Kontext des Versuchs-Spans.
    assert_eq!(
        seen[0]["traceparent"],
        format!("00-{TRACE_ID}-{}-01", attempt.span_id)
    );

    // --- 2) Stream: neuer Trace, TTFT-Ereignis.
    let stream_root = spans
        .iter()
        .find(|s| s.parent.is_none() && s.name == "chat ok")
        .expect("Wurzel-Span des Streams");
    assert_ne!(stream_root.trace_id, TRACE_ID);
    assert_eq!(stream_root.attrs["aigw.stream"], Attr::B(true));
    let (ev_name, ev_attrs) = stream_root
        .events
        .iter()
        .find(|(n, _)| n == "first_token")
        .expect("TTFT-Ereignis");
    assert_eq!(ev_name, "first_token");
    assert!(matches!(ev_attrs.get("aigw.ttft_ms"), Some(Attr::D(ms)) if *ms >= 0.0));
    assert!(matches!(
        stream_root.attrs.get("aigw.ttft_ms"),
        Some(Attr::D(_))
    ));
    let tp1 = seen[1]["traceparent"].to_str().unwrap();
    assert!(tp1.starts_with(&format!("00-{}-", stream_root.trace_id)));
    assert!(tp1.ends_with("-01"));

    // --- 3) Nicht gesampelt: kein Export, aber Weitergabe mit Flag 00.
    assert!(
        !spans
            .iter()
            .any(|s| s.trace_id == "11111111111111111111111111111111")
    );
    let tp2 = seen[2]["traceparent"].to_str().unwrap();
    assert!(
        tp2.starts_with("00-11111111111111111111111111111111-"),
        "{tp2}"
    );
    assert!(tp2.ends_with("-00"));
    assert!(!tp2.contains("2222222222222222"));

    // --- 4) Fehler.
    let failed = spans
        .iter()
        .find(|s| s.parent.is_none() && s.name == "chat fehlt")
        .expect("Wurzel-Span des Fehlers");
    assert_eq!(failed.status, 2, "ERROR");
    assert_eq!(failed.s("error.type"), "not_found");
    assert_eq!(failed.i("http.response.status_code"), 404);
    let failed_route = spans
        .iter()
        .find(|s| s.parent.as_deref() == Some(failed.span_id.as_str()) && s.name == "route")
        .unwrap();
    assert_eq!(failed_route.status, 2);
}

#[tokio::test]
async fn traceparent_ignored_when_disabled_and_no_propagation() {
    let (subscriber, activator) = test_subscriber();
    let _guard = tracing::subscriber::set_default(subscriber);
    let (collector_addr, collector) = start_collector().await;
    let mut cfg = otlp_cfg(collector_addr);
    cfg.traces.accept_traceparent = false;
    cfg.traces.propagate = false;
    let exporter = activator
        .activate(&cfg, Arc::new(OtlpMetrics::default()))
        .unwrap();
    let (gw, up) = start_gateway().await;
    let tp = format!("00-{TRACE_ID}-{PARENT_ID}-01");
    let (status, _) = post_json(
        gw,
        json!({"model": "ok", "messages": [{"role": "user", "content": "Hallo"}]}),
        &[("traceparent", &tp)],
    )
    .await;
    assert_eq!(status, 200);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(exporter.shutdown(Duration::from_secs(5)).await);
    assert!(!up.seen.lock().unwrap()[0].contains_key("traceparent"));
    let spans: Vec<Span> = collector
        .posts
        .lock()
        .unwrap()
        .iter()
        .flat_map(|p| decode_traces(&p.body).1)
        .collect();
    let root = spans.iter().find(|s| s.name == "chat ok").unwrap();
    assert_ne!(root.trace_id, TRACE_ID);
    assert!(root.parent.is_none());
}

#[tokio::test]
async fn inactive_layer_forwards_no_traceparent() {
    // Layer eingehängt, aber nicht aktiviert: keine Spans, kein traceparent.
    let (subscriber, activator) = test_subscriber();
    let _guard = tracing::subscriber::set_default(subscriber);
    assert!(!activator.is_active());
    let (gw, up) = start_gateway().await;
    let (status, _) = post_json(
        gw,
        json!({"model": "ok", "messages": [{"role": "user", "content": "Hallo"}]}),
        &[("traceparent", &format!("00-{TRACE_ID}-{PARENT_ID}-01"))],
    )
    .await;
    assert_eq!(status, 200);
    assert!(!up.seen.lock().unwrap()[0].contains_key("traceparent"));
}

// ---------------------------------------------------------------------------------------------
// Logs
// ---------------------------------------------------------------------------------------------

fn record(status: u16) -> RequestRecord {
    let mut r = RequestRecord::new(new_id().to_string(), WireFormat::AnthropicMessages, status);
    r.model_alias = Some("claude".into());
    r.model_group = Some("claude".into());
    r.provider = Some(ProviderKind::Anthropic);
    r.provider_model = Some("claude-x".into());
    r.duration = Duration::from_millis(321);
    r.ttft = Some(Duration::from_millis(45));
    r.usage = Usage {
        input_tokens: 100,
        output_tokens: 20,
        ..Usage::default()
    };
    r.api_key_id = Some(new_id());
    r
}

#[tokio::test]
async fn request_log_exported_as_otlp_logs() {
    let (collector_addr, collector) = start_collector().await;
    let mut otlp = otlp_cfg(collector_addr);
    otlp.service_name = "gw-test".into();
    let metrics = Arc::new(OtlpMetrics::default());
    let exporter = OtlpLogExporter::new(&otlp, Arc::clone(&metrics)).unwrap();
    let handle = ObserveBuilder::new(ObserveConfig {
        flush_interval_ms: 20,
        ..ObserveConfig::default()
    })
    .exporter(Arc::new(exporter))
    .start();
    let obs = handle.observer();
    let mut ok = record(200);
    // F100/F47/F161: Trace-ID als LogRecord.trace_id, Fehlerklasse und Guardrails als Attribute
    ok.trace_id = Some(TRACE_ID.to_owned());
    ok.error_class = Some(aigw_core::ir::ErrorClass::ContentFilter);
    ok.guardrails = vec![aigw_core::observe::GuardrailHit {
        plugin: "pii",
        category: "email".into(),
        action: "mask",
        count: 3,
    }];
    let ok_id = ok.request_id.clone();
    obs.request_started();
    obs.record(ok);
    obs.request_started();
    obs.record(record(529));
    assert!(handle.shutdown().await);

    let posts = collector.posts.lock().unwrap().clone();
    let mut logs = Vec::new();
    for p in &posts {
        assert_eq!(p.path, "/v1/logs");
        assert_eq!(p.headers["content-type"], "application/x-protobuf");
        let req = parse(&p.body);
        let rl = parse(&one_bytes(&req, 1).unwrap());
        let res = attrs(&parse(&one_bytes(&rl, 1).unwrap()), 1);
        assert_eq!(res["service.name"], Attr::S("gw-test".into()));
        let sl = parse(&one_bytes(&rl, 2).unwrap());
        for l in bytes_of(&sl, 2) {
            logs.push(parse(&l));
        }
    }
    assert_eq!(logs.len(), 2);
    assert_eq!(metrics.logs_exported(), 2);
    let ok = logs
        .iter()
        .find(|l| attrs(l, 6)["aigw.request_id"] == Attr::S(ok_id.clone()))
        .unwrap();
    let a = attrs(ok, 6);
    assert_eq!(string(ok, 12), "aigw.request");
    assert_eq!(uint(ok, 2), 9, "INFO");
    assert_eq!(string(ok, 3), "INFO");
    assert!(uint(ok, 1) > 1_600_000_000_000_000_000);
    assert!(uint(ok, 11) >= uint(ok, 1));
    assert_eq!(a["gen_ai.system"], Attr::S("anthropic".into()));
    assert_eq!(a["gen_ai.provider.name"], Attr::S("anthropic".into()));
    assert_eq!(a["gen_ai.request.model"], Attr::S("claude".into()));
    assert_eq!(a["gen_ai.usage.input_tokens"], Attr::I(100));
    assert_eq!(a["gen_ai.usage.output_tokens"], Attr::I(20));
    assert_eq!(a["http.response.status_code"], Attr::I(200));
    assert_eq!(a["aigw.latency_ms"], Attr::I(321));
    assert_eq!(a["aigw.ttft_ms"], Attr::I(45));
    assert_eq!(a["aigw.endpoint"], Attr::S("anthropic-messages".into()));
    assert!(a.contains_key("aigw.api_key_id"));
    assert_eq!(hex(&one_bytes(ok, 9).unwrap()), TRACE_ID);
    assert_eq!(a["aigw.error_class"], Attr::S("content_filter".into()));
    assert!(matches!(&a["aigw.guardrails"], Attr::S(s) if s.contains("\"count\":3")));
    let err = logs
        .iter()
        .find(|l| attrs(l, 6)["http.response.status_code"] == Attr::I(529))
        .unwrap();
    assert_eq!(uint(err, 2), 17, "ERROR");
}

#[tokio::test]
async fn collector_errors_are_counted() {
    let (collector_addr, collector) = start_collector().await;
    collector
        .fail
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let metrics = Arc::new(OtlpMetrics::default());
    let exporter = OtlpLogExporter::new(&otlp_cfg(collector_addr), Arc::clone(&metrics)).unwrap();
    let handle = ObserveBuilder::new(ObserveConfig {
        flush_interval_ms: 20,
        ..ObserveConfig::default()
    })
    .exporter(Arc::new(exporter))
    .start();
    let log_metrics = Arc::clone(handle.metrics());
    handle.observer().record(record(200));
    assert!(handle.shutdown().await);
    assert_eq!(log_metrics.snapshot().export_errors, 1);
    assert_eq!(metrics.logs_exported(), 0);
    let mut out = String::new();
    metrics.render_prometheus(&mut out);
    assert!(out.contains("aigw_otlp_export_errors_total{signal=\"logs\"} 1\n"));
}

/// Exportiert einen Log-Eintrag über einen frischen Exporter; liefert (Exporter-Metriken,
/// Export-Fehler des Request-Logs).
async fn export_one(collector_addr: SocketAddr) -> (Arc<OtlpMetrics>, u64) {
    let mut cfg = otlp_cfg(collector_addr);
    cfg.retry = OtlpRetryConfig {
        max_attempts: 4,
        initial_backoff_ms: 10,
        max_backoff_ms: 2_000,
    };
    let metrics = Arc::new(OtlpMetrics::default());
    let exporter = OtlpLogExporter::new(&cfg, Arc::clone(&metrics)).unwrap();
    let handle = ObserveBuilder::new(ObserveConfig {
        flush_interval_ms: 20,
        ..ObserveConfig::default()
    })
    .exporter(Arc::new(exporter))
    .start();
    let log_metrics = Arc::clone(handle.metrics());
    handle.observer().record(record(200));
    assert!(handle.shutdown().await);
    let errors = log_metrics.snapshot().export_errors;
    (metrics, errors)
}

#[tokio::test]
async fn transient_collector_errors_are_retried_with_retry_after() {
    let (collector_addr, collector) = start_collector().await;
    // 503 mit Retry-After 1 s, dann 429 ohne → Backoff, dann Erfolg.
    collector
        .script
        .lock()
        .unwrap()
        .extend([(503, Some("1")), (429, None)]);
    let started = std::time::Instant::now();
    let (metrics, errors) = export_one(collector_addr).await;
    assert!(
        started.elapsed() >= Duration::from_millis(1000),
        "Retry-After beachtet"
    );
    assert_eq!(errors, 0);
    assert_eq!(metrics.logs_exported(), 1);
    assert_eq!(collector.posts.lock().unwrap().len(), 3);
    let mut out = String::new();
    metrics.render_prometheus(&mut out);
    assert!(
        out.contains("aigw_otlp_export_retries_total{signal=\"logs\"} 2\n"),
        "{out}"
    );
    assert!(out.contains("aigw_otlp_export_errors_total{signal=\"logs\"} 0\n"));
}

#[tokio::test]
async fn retries_are_bounded() {
    // Retry-After länger als max_backoff: sofort verwerfen (kein Blockieren des Exports).
    let (collector_addr, collector) = start_collector().await;
    collector
        .script
        .lock()
        .unwrap()
        .push_back((429, Some("3600")));
    let (metrics, errors) = export_one(collector_addr).await;
    assert_eq!(errors, 1);
    assert_eq!(metrics.logs_exported(), 0);
    assert_eq!(collector.posts.lock().unwrap().len(), 1);

    // Dauerhaft 503: höchstens max_attempts Versuche.
    let (collector_addr, collector) = start_collector().await;
    collector.script.lock().unwrap().extend([(503, None); 10]);
    let (_, errors) = export_one(collector_addr).await;
    assert_eq!(errors, 1);
    assert_eq!(collector.posts.lock().unwrap().len(), 4);

    // 400 ist kein vorübergehender Fehler: keine Wiederholung.
    let (collector_addr, collector) = start_collector().await;
    collector.script.lock().unwrap().push_back((400, None));
    let (_, errors) = export_one(collector_addr).await;
    assert_eq!(errors, 1);
    assert_eq!(collector.posts.lock().unwrap().len(), 1);
}
