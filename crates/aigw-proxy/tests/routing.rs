// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Routing-Tests (AIGW-24/25/26) gegen einen lokalen Fake-Upstream (OpenAI-kompatibel):
//! Gewichtsverteilung, Prioritätsgruppen, Strategien, Retry auf anderes Deployment, kein
//! Retry nach dem ersten Byte, `Retry-After`, Fallback-Ketten, Circuit Breaker, Health Checks
//! und Hot Reload. Keine externen Aufrufe.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aigw_core::ir::ErrorClass;
use aigw_core::observe::{RequestObserver, RequestRecord};
use aigw_core::resource::{
    Amount, Deployment, DeploymentSpec, Fallback, FallbackTrigger, JsonObject, ModelGroup,
    ModelGroupSpec, ObjectMeta, Price, PriceSpec, RoutingStrategy, TokenPrices, new_id,
};
use aigw_core::snapshot::{DeploymentCtx, ModelGroupCtx, Snapshot, SnapshotHandle};
use aigw_core::types::ProviderKind;
use aigw_providers::{Registry, UpstreamClient};
use aigw_proxy::routing::{BreakerState, GroupPolicy, MemberSpec};
use aigw_proxy::{GroupRoute, ModelRoutes, ProxyState, SnapshotRoutes};
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------------------------
// Fake-Upstream: Verhalten je Anbieter-Modell (= Deployment), zur Laufzeit umschaltbar.
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Ok,
    Fail,
    RateLimitOnce,
    ContextLength,
    ContentFilter,
    MidStream,
    FirstError,
    Slow(u64),
}

#[derive(Default)]
struct Fake {
    modes: Mutex<HashMap<String, Mode>>,
    hits: Mutex<Vec<String>>,
}

impl Fake {
    fn set(&self, model: &str, mode: Mode) {
        self.modes.lock().unwrap().insert(model.to_owned(), mode);
    }

    fn hits(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }

    fn count(&self, model: &str) -> usize {
        self.hits().iter().filter(|h| *h == model).count()
    }

    fn clear(&self) {
        self.hits.lock().unwrap().clear();
    }
}

fn chunk(content: &str) -> String {
    format!(
        "data: {}\n\n",
        json!({"id": "c1", "object": "chat.completion.chunk", "created": 1, "model": "m",
               "choices": [{"index": 0, "delta": {"content": content}}]})
    )
}

fn sse(parts: Vec<String>) -> Response {
    let s = futures_util::stream::iter(
        parts
            .into_iter()
            .map(|p| Ok::<_, std::io::Error>(Bytes::from(p))),
    );
    (
        [("content-type", "text/event-stream")],
        Body::from_stream(s),
    )
        .into_response()
}

fn err(status: u16, headers: &[(&'static str, &'static str)], body: Value) -> Response {
    let mut res = (StatusCode::from_u16(status).unwrap(), body.to_string()).into_response();
    for (k, v) in headers {
        res.headers_mut().insert(*k, v.parse().unwrap());
    }
    res
}

async fn fake_handler(State(fake): State<Arc<Fake>>, body: Bytes) -> Response {
    let body: Value = serde_json::from_slice(&body).unwrap();
    let model = body["model"].as_str().unwrap_or_default().to_owned();
    let stream = body["stream"].as_bool().unwrap_or(false);
    fake.hits.lock().unwrap().push(model.clone());
    let mode = {
        let mut modes = fake.modes.lock().unwrap();
        let m = modes.get(&model).copied().unwrap_or(Mode::Ok);
        if m == Mode::RateLimitOnce {
            modes.insert(model.clone(), Mode::Ok);
        }
        m
    };
    match mode {
        Mode::Fail => err(
            500,
            &[],
            json!({"error": {"message": "boom", "type": "server_error"}}),
        ),
        Mode::RateLimitOnce => err(
            429,
            &[("retry-after", "1")],
            json!({"error": {"message": "slow down", "code": "rate_limit_exceeded"}}),
        ),
        Mode::ContextLength => err(
            400,
            &[],
            json!({"error": {"message": "maximum context length is 10 tokens", "code": "context_length_exceeded"}}),
        ),
        Mode::ContentFilter => err(
            400,
            &[],
            json!({"error": {"message": "filtered", "code": "content_filter"}}),
        ),
        Mode::MidStream if stream => sse(vec![
            chunk("a"),
            "data: {\"error\":{\"message\":\"exploded\",\"type\":\"server_error\"}}\n\n".to_owned(),
        ]),
        Mode::FirstError if stream => sse(vec![
            "data: {\"error\":{\"message\":\"overloaded\",\"type\":\"server_error\"}}\n\n"
                .to_owned(),
        ]),
        Mode::Slow(ms) => {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            ok(&model, stream)
        }
        _ => ok(&model, stream),
    }
}

fn ok(model: &str, stream: bool) -> Response {
    if stream {
        return sse(vec![
            chunk(&format!("von {model}")),
            format!(
                "data: {}\n\n",
                json!({"id": "c1", "object": "chat.completion.chunk", "created": 1, "model": "m",
                       "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})
            ),
            format!(
                "data: {}\n\n",
                json!({"id": "c1", "object": "chat.completion.chunk", "created": 1, "model": "m",
                       "choices": [], "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}})
            ),
            "data: [DONE]\n\n".to_owned(),
        ]);
    }
    (
        [("content-type", "application/json")],
        json!({"id": "c1", "object": "chat.completion", "created": 1, "model": "m",
               "choices": [{"index": 0, "message": {"role": "assistant", "content": format!("von {model}")},
                            "finish_reason": "stop"}],
               "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}})
        .to_string(),
    )
        .into_response()
}

async fn start_fake() -> (String, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    let app = Router::new()
        .route("/v1/chat/completions", post(fake_handler))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/v1"), fake)
}

// ---------------------------------------------------------------------------------------------
// Konfiguration
// ---------------------------------------------------------------------------------------------

struct Dep {
    name: &'static str,
    weight: u32,
    priority: u32,
}

fn dep(name: &'static str) -> Dep {
    Dep {
        name,
        weight: 1,
        priority: 0,
    }
}

fn deployment(
    group: &str,
    d: &Dep,
    base: &str,
    id: Option<aigw_core::resource::Id>,
) -> DeploymentCtx {
    DeploymentCtx {
        deployment: Deployment {
            id: id.unwrap_or_else(new_id),
            metadata: ObjectMeta::named(d.name),
            spec: DeploymentSpec {
                model_group: group.into(),
                provider: ProviderKind::OpenAiCompatible,
                endpoint: Some(base.into()),
                // Das Anbieter-Modell = Name des Deployments steuert den Fake-Upstream.
                provider_model: d.name.into(),
                credential: None,
                weight: d.weight,
                priority: d.priority,
                capacity: Default::default(),
                enabled: true,
                settings: JsonObject::new(),
            },
        },
        credential: None,
    }
}

fn obj(v: Value) -> JsonObject {
    match v {
        Value::Object(o) => o,
        _ => JsonObject::new(),
    }
}

fn group(
    name: &str,
    routing: RoutingStrategy,
    settings: Value,
    fallbacks: Vec<Fallback>,
    deps: Vec<DeploymentCtx>,
) -> Arc<ModelGroupCtx> {
    Arc::new(ModelGroupCtx {
        group: ModelGroup {
            id: new_id(),
            metadata: ObjectMeta::named(name),
            spec: ModelGroupSpec {
                aliases: vec![format!("{name}-alias")],
                capabilities: Vec::new(),
                routing,
                fallbacks,
                limits: Vec::new(),
                settings: obj(settings),
            },
        },
        deployments: deps,
    })
}

fn snapshot(version: i64, groups: Vec<Arc<ModelGroupCtx>>) -> Snapshot {
    let mut s = Snapshot {
        version,
        ..Snapshot::default()
    };
    for g in groups {
        s.model_groups
            .insert(g.group.metadata.name.clone(), g.clone());
        for a in &g.group.spec.aliases {
            s.model_groups.insert(a.clone(), g.clone());
        }
    }
    s
}

/// Schnelle Tests: kurzer Backoff, Breaker nur wo gewollt.
fn fast(extra: Value) -> Value {
    let mut base = json!({
        "retry": {"maxAttempts": 3, "initialBackoffMs": 5, "maxBackoffMs": 20},
        "circuitBreaker": {"enabled": false}
    });
    if let (Value::Object(b), Value::Object(e)) = (&mut base, extra) {
        for (k, v) in e {
            b.insert(k, v);
        }
    }
    base
}

#[derive(Debug, Default)]
struct Capture(Mutex<Vec<RequestRecord>>);

impl RequestObserver for Capture {
    fn record(&self, record: RequestRecord) {
        self.0.lock().unwrap().push(record);
    }
}

impl Capture {
    async fn last(&self) -> RequestRecord {
        for _ in 0..200 {
            if let Some(r) = self.0.lock().unwrap().last() {
                return r.clone();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("kein Datensatz");
    }

    fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
}

struct Env {
    gw: SocketAddr,
    routes: Arc<SnapshotRoutes>,
    capture: Arc<Capture>,
}

async fn gateway(routes: Arc<SnapshotRoutes>) -> Env {
    let capture = Arc::new(Capture::default());
    let mut state = ProxyState::with_routes(routes.clone() as Arc<dyn ModelRoutes>);
    state.observer = capture.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, aigw_proxy::router(state))
            .await
            .unwrap();
    });
    Env {
        gw,
        routes,
        capture,
    }
}

fn new_routes() -> Arc<SnapshotRoutes> {
    Arc::new(SnapshotRoutes::new(
        Registry::with_builtin(),
        UpstreamClient::unguarded().unwrap(),
    ))
}

async fn env(snap: &Snapshot) -> Env {
    let routes = new_routes();
    routes.rebuild(snap);
    gateway(routes).await
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Reply {
    fn deployment(&self) -> &str {
        self.headers
            .get("x-aigw-deployment")
            .map_or("", |v| v.to_str().unwrap())
    }

    fn attempts(&self) -> usize {
        self.headers
            .get("x-aigw-attempts")
            .map_or(0, |v| v.to_str().unwrap().parse().unwrap())
    }

    fn fallback(&self) -> Option<&str> {
        self.headers
            .get("x-aigw-fallback")
            .map(|v| v.to_str().unwrap())
    }
}

impl Env {
    async fn chat(&self, model: &str, stream: bool) -> Reply {
        let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
        let body = json!({"model": model, "stream": stream,
                          "messages": [{"role": "user", "content": "Hallo"}]});
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("http://{}/v1/chat/completions", self.gw))
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(body.to_string())))
            .unwrap();
        let res = client.request(req).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let body = res.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Auswahl
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn weighted_distribution_is_statistically_correct() {
    let (base, fake) = start_fake().await;
    let mut heavy = dep("heavy");
    heavy.weight = 3;
    let snap = snapshot(
        1,
        vec![group(
            "g",
            RoutingStrategy::Weighted,
            fast(json!({})),
            vec![],
            vec![
                deployment("g", &dep("light"), &base, None),
                deployment("g", &heavy, &base, None),
            ],
        )],
    );
    let routes = new_routes();
    routes.rebuild(&snap);

    // Auswahl ohne Netzwerk: 100 000 Ziehungen, Anteil 75 % ± 1 % (≈ 7 σ).
    let route = routes.route("g").unwrap();
    let n = 100_000;
    let heavy_picks = (0..n)
        .filter(|_| {
            route
                .select_for_bench(false)
                .unwrap()
                .provider()
                .describe()
                .deployment
                == "heavy"
        })
        .count();
    let share = heavy_picks as f64 / n as f64;
    assert!((share - 0.75).abs() < 0.01, "Anteil heavy = {share}");

    // Ende zu Ende über den Alias: 400 Anfragen, Anteil 75 % ± 8 % (≈ 4 σ).
    let e = gateway(routes).await;
    for _ in 0..400 {
        let r = e.chat("g-alias", false).await;
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(r.attempts(), 1);
    }
    let share = fake.count("heavy") as f64 / 400.0;
    assert!((share - 0.75).abs() < 0.08, "Anteil heavy = {share}");
}

#[tokio::test]
async fn priority_groups_and_retry_on_other_deployment() {
    let (base, fake) = start_fake().await;
    let mut payg = dep("payg");
    payg.priority = 1;
    let snap = snapshot(
        1,
        vec![group(
            "g",
            RoutingStrategy::Weighted,
            fast(
                json!({"circuitBreaker": {"minRequests": 2, "failureRate": 0.25, "openSecs": 60}}),
            ),
            vec![],
            vec![
                deployment("g", &dep("ptu"), &base, None),
                deployment("g", &payg, &base, None),
            ],
        )],
    );
    let e = env(&snap).await;

    // Gruppe 0 zuerst.
    for _ in 0..5 {
        let r = e.chat("g", false).await;
        assert_eq!(r.deployment(), "ptu");
    }
    assert_eq!(fake.count("payg"), 0);

    // PTU fällt aus → Retry auf PAYG (anderes Deployment, ohne Backoff-Wartezeit).
    fake.set("ptu", Mode::Fail);
    fake.clear();
    let r = e.chat("g", false).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.deployment(), "payg");
    assert_eq!(r.attempts(), 2);
    assert_eq!(fake.hits(), vec!["ptu", "payg"]);
    let rec = e.capture.last().await;
    assert_eq!(rec.attempts.len(), 2);
    assert_eq!(rec.attempts[0].deployment, "ptu");
    assert_eq!(rec.attempts[0].error, Some(ErrorClass::ServerError));
    assert_eq!(rec.attempts[0].upstream_status, Some(500));
    assert_eq!(rec.attempts[1].deployment, "payg");
    assert_eq!(rec.attempts[1].error, None);
    assert_eq!(rec.provider_model.as_deref(), Some("payg"));

    // Zweiter Fehler → Breaker von PTU offen → Gruppe 0 gesperrt, direkt Gruppe 1.
    let _ = e.chat("g", false).await;
    let ptu = snap.model_groups["g"].deployments[0].deployment.id;
    assert_eq!(
        e.routes.deployment_state(ptu).unwrap().breaker_state(),
        BreakerState::Open
    );
    fake.clear();
    let r = e.chat("g", true).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.attempts(), 1);
    assert_eq!(fake.hits(), vec!["payg"]);
    assert!(r.body.contains("von payg"));
}

#[tokio::test]
async fn least_latency_least_busy_and_cost() {
    let (base, fake) = start_fake().await;
    fake.set("slow", Mode::Slow(80));
    fake.set("busy-a", Mode::Slow(300));
    fake.set("busy-b", Mode::Slow(300));
    let mut snap = snapshot(
        1,
        vec![
            group(
                "lat",
                RoutingStrategy::LeastLatency,
                fast(json!({})),
                vec![],
                vec![
                    deployment("lat", &dep("slow"), &base, None),
                    deployment("lat", &dep("quick"), &base, None),
                ],
            ),
            group(
                "busy",
                RoutingStrategy::LeastBusy,
                fast(json!({})),
                vec![],
                vec![
                    deployment("busy", &dep("busy-a"), &base, None),
                    deployment("busy", &dep("busy-b"), &base, None),
                ],
            ),
            group(
                "cost",
                RoutingStrategy::Cost,
                fast(json!({})),
                vec![],
                vec![
                    deployment("cost", &dep("pricey"), &base, None),
                    deployment("cost", &dep("cheap"), &base, None),
                    deployment("cost", &dep("unpriced"), &base, None),
                ],
            ),
        ],
    );
    for (model, input, output) in [("pricey", "5", "15"), ("cheap", "0.5", "1.5")] {
        snap.prices.insert(
            (Some(ProviderKind::OpenAiCompatible), model.to_owned()),
            vec![Arc::new(Price {
                id: new_id(),
                metadata: ObjectMeta::named(model),
                spec: PriceSpec {
                    provider: Some(ProviderKind::OpenAiCompatible),
                    model: model.into(),
                    valid_from: time::OffsetDateTime::UNIX_EPOCH,
                    per_million_tokens: TokenPrices {
                        input: Some(input.parse::<Amount>().unwrap()),
                        output: Some(output.parse::<Amount>().unwrap()),
                        ..TokenPrices::default()
                    },
                    per_request: None,
                    per_image: None,
                    per_audio_minute: None,
                    per_million_characters: None,
                },
            })],
        );
    }
    let e = env(&snap).await;

    // least-latency: nach dem Kennenlernen beider nur noch das schnelle Deployment.
    for _ in 0..4 {
        e.chat("lat", false).await;
    }
    fake.clear();
    for _ in 0..10 {
        assert_eq!(e.chat("lat", false).await.deployment(), "quick");
    }
    assert_eq!(fake.count("slow"), 0);

    // least-busy: zwei gleichzeitige Anfragen landen auf verschiedenen Deployments.
    let e = Arc::new(e);
    let first = {
        let e = e.clone();
        tokio::spawn(async move { e.chat("busy", false).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    let second = e.chat("busy", false).await;
    let first = first.await.unwrap();
    assert_ne!(first.deployment(), second.deployment());

    // cost: immer das günstigste.
    for _ in 0..10 {
        assert_eq!(e.chat("cost", false).await.deployment(), "cheap");
    }
}

#[tokio::test]
#[allow(clippy::print_stdout)]
async fn selection_overhead_is_small() {
    // Messung des Auswahl-Aufwands (ns je Auswahl) je Strategie; Grenze großzügig gegen
    // Ausreißer auf geteilten Maschinen, die Messwerte stehen in der Ausgabe
    // (`cargo test --release -p aigw-proxy --test routing selection -- --nocapture`).
    for (strategy, size) in [
        (RoutingStrategy::Weighted, 1u32),
        (RoutingStrategy::Weighted, 4),
        (RoutingStrategy::LeastLatency, 4),
        (RoutingStrategy::LeastBusy, 4),
        (RoutingStrategy::Cost, 4),
        (RoutingStrategy::Weighted, 16),
    ] {
        let members = (0..size)
            .map(|i| {
                let mut m = MemberSpec::new(Arc::new(aigw_proxy::mock::MockProvider::new()));
                m.weight = i + 1;
                m.priority = i / 2;
                m.cost = Some(u64::from(i % 3));
                m
            })
            .collect();
        let route = GroupRoute::new("bench", strategy, GroupPolicy::default(), vec![], members);
        let n = 200_000u32;
        let start = Instant::now();
        for _ in 0..n {
            std::hint::black_box(route.select_for_bench(false));
        }
        let ns = start.elapsed().as_nanos() / u128::from(n);
        println!("Auswahl {strategy:?}, {size} Deployment(s): {ns} ns");
        assert!(ns < 20_000, "{ns} ns je Auswahl");
    }
}

// ---------------------------------------------------------------------------------------------
// Retries
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn no_retry_after_first_byte_but_before() {
    let (base, fake) = start_fake().await;
    let snap = snapshot(
        1,
        vec![group(
            "g",
            RoutingStrategy::Weighted,
            fast(json!({})),
            vec![],
            vec![
                deployment("g", &dep("first"), &base, None),
                deployment("g", &dep("second"), &base, None),
            ],
        )],
    );
    let e = env(&snap).await;

    // Fehler mitten im Stream (nach dem ersten Ereignis): kein Retry, Fehlerereignis.
    fake.set("first", Mode::MidStream);
    fake.set("second", Mode::MidStream);
    let r = e.chat("g", true).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.attempts(), 1);
    assert!(r.body.contains("exploded"), "{}", r.body);
    assert_eq!(fake.hits().len(), 1);

    // Fehler als erstes Ereignis (noch nichts an den Client): Retry auf anderem Deployment.
    fake.set("first", Mode::FirstError);
    fake.set("second", Mode::FirstError);
    fake.clear();
    let r = e.chat("g", true).await;
    assert_eq!(r.attempts(), 3);
    assert!(r.status.is_server_error(), "{}", r.status);
    let hits = fake.hits();
    assert_eq!(hits.len(), 3);
    assert_ne!(hits[0], hits[1], "zweiter Versuch auf anderem Deployment");

    fake.set("second", Mode::Ok);
    fake.clear();
    for _ in 0..5 {
        let r = e.chat("g", true).await;
        assert_eq!(r.status, StatusCode::OK);
        assert!(r.body.contains("von second"));
        assert!(!r.body.contains("error"));
    }
}

#[tokio::test]
async fn retry_after_is_respected_and_budget_limits() {
    let (base, fake) = start_fake().await;
    fake.set("only", Mode::RateLimitOnce);
    let snap = snapshot(
        1,
        vec![
            group(
                "g",
                RoutingStrategy::Weighted,
                fast(json!({"retry": {"maxAttempts": 2, "maxRetryAfterMs": 2000}})),
                vec![],
                vec![deployment("g", &dep("only"), &base, None)],
            ),
            group(
                "impatient",
                RoutingStrategy::Weighted,
                fast(json!({"retry": {"maxAttempts": 2, "maxRetryAfterMs": 100}})),
                vec![],
                vec![deployment("impatient", &dep("rl"), &base, None)],
            ),
        ],
    );
    let e = env(&snap).await;
    let t = Instant::now();
    let r = e.chat("g", false).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.attempts(), 2);
    assert!(
        t.elapsed() >= Duration::from_millis(950),
        "{:?}",
        t.elapsed()
    );

    // Retry-After länger als erlaubt → kein Warten, 429 mit Retry-After an den Client.
    fake.set("rl", Mode::RateLimitOnce);
    let t = Instant::now();
    let r = e.chat("impatient", false).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.attempts(), 1);
    assert_eq!(r.headers["retry-after"], "1");
    assert!(t.elapsed() < Duration::from_millis(500));
}

// ---------------------------------------------------------------------------------------------
// Fallbacks
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn fallback_chain_by_error_class() {
    let (base, fake) = start_fake().await;
    fake.set("small", Mode::ContextLength);
    fake.set("down", Mode::Fail);
    fake.set("filtered", Mode::ContentFilter);
    let fb = |g: &str, on: Vec<FallbackTrigger>| Fallback {
        model_group: g.into(),
        on,
    };
    let snap = snapshot(
        1,
        vec![
            group(
                "small",
                RoutingStrategy::Weighted,
                fast(json!({})),
                vec![
                    fb("other", vec![FallbackTrigger::RateLimit]),
                    fb("large", vec![FallbackTrigger::ContextLength]),
                ],
                vec![deployment("small", &dep("small"), &base, None)],
            ),
            group(
                "large",
                RoutingStrategy::Weighted,
                fast(json!({})),
                vec![],
                vec![deployment("large", &dep("large"), &base, None)],
            ),
            group(
                "other",
                RoutingStrategy::Weighted,
                fast(json!({})),
                vec![],
                vec![deployment("other", &dep("other"), &base, None)],
            ),
            group(
                "primary",
                RoutingStrategy::Weighted,
                fast(json!({"retry": {"maxAttempts": 2}})),
                vec![fb("down2", vec![]), fb("large", vec![])],
                vec![deployment("primary", &dep("down"), &base, None)],
            ),
            group(
                "down2",
                RoutingStrategy::Weighted,
                fast(json!({"retry": {"maxAttempts": 1}})),
                vec![],
                vec![deployment("down2", &dep("down"), &base, None)],
            ),
            group(
                "strict",
                RoutingStrategy::Weighted,
                fast(json!({})),
                vec![fb("large", vec![FallbackTrigger::ContextLength])],
                vec![deployment("strict", &dep("filtered"), &base, None)],
            ),
        ],
    );
    let e = env(&snap).await;

    // Kontextlänge → kein Retry, Fallback auf die größere Gruppe (RateLimit-Fallback übersprungen).
    let r = e.chat("small", false).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.fallback(), Some("large"));
    assert_eq!(r.deployment(), "large");
    assert_eq!(r.attempts(), 2);
    assert_eq!(fake.hits(), vec!["small", "large"]);
    let rec = e.capture.last().await;
    assert_eq!(rec.model_group.as_deref(), Some("large"));
    assert_eq!(rec.attempts[0].error, Some(ErrorClass::ContextLength));
    assert_eq!(rec.attempts[1].model_group.as_deref(), Some("large"));

    // Kette über zwei Gruppen: primary (2 Versuche) → down2 (1) → large.
    fake.clear();
    let r = e.chat("primary", true).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.attempts(), 4);
    assert_eq!(r.fallback(), Some("large"));
    assert_eq!(fake.hits(), vec!["down", "down", "down", "large"]);

    // Content-Filter: kein Retry, kein Fallback (nur bei Kontextlänge konfiguriert).
    fake.clear();
    let r = e.chat("strict", false).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.attempts(), 1);
    assert_eq!(r.fallback(), None);
    assert_eq!(fake.hits(), vec!["filtered"]);
}

// ---------------------------------------------------------------------------------------------
// Circuit Breaker und Health Checks
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn breaker_opens_rejects_and_closes_after_probe() {
    let (base, fake) = start_fake().await;
    fake.set("flaky", Mode::Fail);
    let snap = snapshot(
        1,
        vec![group(
            "g",
            RoutingStrategy::Weighted,
            json!({
                "retry": {"maxAttempts": 1},
                "circuitBreaker": {"minRequests": 3, "failureRate": 0.5, "windowSecs": 10, "openSecs": 1}
            }),
            vec![],
            vec![deployment("g", &dep("flaky"), &base, None)],
        )],
    );
    let id = snap.model_groups["g"].deployments[0].deployment.id;
    let e = env(&snap).await;
    for _ in 0..3 {
        assert_eq!(
            e.chat("g", false).await.status,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
    let state = e.routes.deployment_state(id).unwrap();
    assert_eq!(state.breaker_state(), BreakerState::Open);

    // Offen: sofort 503 ohne Upstream-Aufruf, mit Retry-After.
    fake.clear();
    let r = e.chat("g", false).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(r.body.contains("no_deployment_available"), "{}", r.body);
    assert_eq!(r.headers["retry-after"], "1");
    assert_eq!(r.attempts(), 0);
    assert!(fake.hits().is_empty());

    // Nach der Offen-Dauer: Probe gelingt → geschlossen.
    fake.set("flaky", Mode::Ok);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let r = e.chat("g", false).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(state.breaker_state(), BreakerState::Closed);
    assert_eq!(e.chat("g", false).await.status, StatusCode::OK);
}

#[tokio::test]
async fn health_checks_open_and_close_the_breaker() {
    let (base, fake) = start_fake().await;
    fake.set("hc", Mode::Fail);
    let snap = snapshot(
        1,
        vec![group(
            "g",
            RoutingStrategy::Weighted,
            json!({
                "circuitBreaker": {"openSecs": 60},
                "healthCheck": {"intervalSecs": 1, "timeoutMs": 1000, "unhealthyThreshold": 2}
            }),
            vec![],
            vec![deployment("g", &dep("hc"), &base, None)],
        )],
    );
    let id = snap.model_groups["g"].deployments[0].deployment.id;
    let handle = SnapshotHandle::new(snap);
    let routes = new_routes();
    let _task = routes.spawn(handle);
    let state = routes.deployment_state(id).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while state.breaker_state() != BreakerState::Open {
        assert!(Instant::now() < deadline, "Breaker öffnet nicht");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(fake.count("hc") >= 2);
    fake.set("hc", Mode::Ok);
    while state.breaker_state() != BreakerState::Closed {
        assert!(Instant::now() < deadline, "Breaker schließt nicht");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------------------------
// Hot Reload
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn hot_reload_changes_routes_and_keeps_state() {
    let (base, fake) = start_fake().await;
    let keep_id = new_id();
    let v1 = snapshot(
        1,
        vec![group(
            "g",
            RoutingStrategy::Weighted,
            fast(json!({})),
            vec![],
            vec![deployment("g", &dep("old"), &base, Some(keep_id))],
        )],
    );
    let handle = SnapshotHandle::new(v1);
    let routes = new_routes();
    let _task = routes.spawn(handle.clone());
    let e = gateway(routes.clone()).await;
    assert_eq!(e.chat("g", false).await.deployment(), "old");
    let state_before = routes.deployment_state(keep_id).unwrap();
    assert!(state_before.ewma_latency().is_some());

    // v2: gleiches Deployment (gleiche ID, nur Gewicht/Priorität geändert) + neues mit
    // Vorrang; neue Gruppe „h“.
    let mut old = dep("old");
    old.priority = 1;
    old.weight = 7;
    let v2 = snapshot(
        2,
        vec![
            group(
                "g",
                RoutingStrategy::Weighted,
                fast(json!({})),
                vec![],
                vec![
                    deployment("g", &old, &base, Some(keep_id)),
                    deployment("g", &dep("new"), &base, None),
                ],
            ),
            group(
                "h",
                RoutingStrategy::Weighted,
                fast(json!({})),
                vec![],
                vec![deployment("h", &dep("h1"), &base, None)],
            ),
        ],
    );
    assert_eq!(e.chat("h", false).await.status, StatusCode::NOT_FOUND);
    handle.store(v2);
    let deadline = Instant::now() + Duration::from_secs(5);
    while routes.version() != 2 {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    fake.clear();
    assert_eq!(e.chat("g", false).await.deployment(), "new");
    assert_eq!(e.chat("h", false).await.deployment(), "h1");
    let state_after = routes.deployment_state(keep_id).unwrap();
    assert!(
        Arc::ptr_eq(&state_before, &state_after),
        "Zustand des unveränderten Deployments bleibt erhalten"
    );

    // v3: Endpunkt des Deployments geändert → neuer Zustand; Gruppe h entfernt.
    let v3 = snapshot(
        3,
        vec![group(
            "g",
            RoutingStrategy::Weighted,
            fast(json!({})),
            vec![],
            vec![deployment(
                "g",
                &dep("old"),
                &format!("{base}/"),
                Some(keep_id),
            )],
        )],
    );
    handle.store(v3);
    while routes.version() != 3 {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!Arc::ptr_eq(
        &state_before,
        &routes.deployment_state(keep_id).unwrap()
    ));
    assert_eq!(e.chat("h", false).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn mock_deployments_and_unsupported_providers() {
    let snap = snapshot(
        1,
        vec![group(
            "m",
            RoutingStrategy::Weighted,
            fast(json!({})),
            vec![],
            vec![
                DeploymentCtx {
                    deployment: Deployment {
                        id: new_id(),
                        metadata: ObjectMeta::named("mock-broken"),
                        spec: DeploymentSpec {
                            model_group: "m".into(),
                            provider: ProviderKind::Mock,
                            endpoint: None,
                            provider_model: "mock-error".into(),
                            credential: None,
                            weight: 1,
                            priority: 0,
                            capacity: Default::default(),
                            enabled: true,
                            settings: JsonObject::new(),
                        },
                    },
                    credential: None,
                },
                DeploymentCtx {
                    deployment: Deployment {
                        id: new_id(),
                        metadata: ObjectMeta::named("mock-ok"),
                        spec: DeploymentSpec {
                            model_group: "m".into(),
                            provider: ProviderKind::Mock,
                            endpoint: None,
                            provider_model: "mock".into(),
                            credential: None,
                            weight: 1,
                            priority: 1,
                            capacity: Default::default(),
                            enabled: true,
                            settings: JsonObject::new(),
                        },
                    },
                    credential: None,
                },
                DeploymentCtx {
                    deployment: Deployment {
                        id: new_id(),
                        metadata: ObjectMeta::named("bedrock"),
                        spec: DeploymentSpec {
                            model_group: "m".into(),
                            provider: ProviderKind::Bedrock,
                            endpoint: None,
                            provider_model: "x".into(),
                            credential: None,
                            weight: 1,
                            priority: 0,
                            capacity: Default::default(),
                            enabled: true,
                            settings: JsonObject::new(),
                        },
                    },
                    credential: None,
                },
            ],
        )],
    );
    let e = env(&snap).await;
    let r = e.chat("m", false).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.deployment(), "mock-ok");
    assert_eq!(r.attempts(), 2);
    assert!(r.body.contains("Echo: Hallo"));
    let rec = e.capture.last().await;
    assert_eq!(rec.provider, Some(ProviderKind::Mock));
    assert_eq!(e.routes.route("m").unwrap().members().len(), 2);
    e.capture.clear();
}
