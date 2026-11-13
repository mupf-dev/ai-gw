// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Limits, Budgets, Kosten und Deployment-Kapazität über die HTTP-Schnittstelle
//! (AIGW-30, AIGW-31). Zähler `local`; zusätzlich gegen Redis (`AIGW_TEST_REDIS_URL`) und
//! `postgres-sync` (`AIGW_TEST_POSTGRES_URL`) mit zwei „Instanzen“.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aigw_billing::{BudgetEvent, BudgetEventKind, BudgetEventSink, Budgets, Calendar};
use aigw_core::config::LimitsConfig;
use aigw_core::ir::{
    ApiError, ChatRequest, ChatResponse, Choice, Content, ErrorClass, ErrorKind, Message, Role,
    StopReason, StreamEvent, UpstreamError, Usage,
};
use aigw_core::observe::{RequestObserver, RequestRecord};
use aigw_core::resource::{
    self as r, Amount, ApiKeySpec, ApiKeyStatus, Budget, BudgetPeriod, Capacity, JsonObject,
    LimitMetric, LimitSpec, LimitWindow, ModelGroupSpec, Object, ObjectMeta, OrganizationSpec,
    PriceSpec, RoutingStrategy, TeamSpec, TokenPrices, new_id,
};
use aigw_core::snapshot::{KeyCtx, ModelGroupCtx, Snapshot, SnapshotHandle};
use aigw_core::types::ProviderKind;
use aigw_counters::{CounterBackend, LocalCounters, Window};
use aigw_proxy::provider::{BoxFuture, ChatProvider, Describe, EventStream};
use aigw_proxy::routing::{GroupPolicy, MemberSpec};
use aigw_proxy::{
    GroupRoute, Limiter, ProxyState, SnapshotAuthenticator, StaticRoutes, auth::hash_key,
};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{HeaderMap, Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};

const SECRET: &str = "sk-limits-test";

// ---------------------------------------------------------------------------------------------
// Testanbieter, Beobachter, Ereignisse
// ---------------------------------------------------------------------------------------------

/// Antwortet mit fester Usage (7 Eingabe, 3 Ausgabe); die ersten `fail` Aufrufe scheitern
/// retrybar; `endless` streamt ohne Ende (für Abbrüche).
struct TestProvider {
    name: String,
    fail: AtomicU32,
    calls: AtomicU32,
    endless: bool,
}

impl TestProvider {
    fn new(name: &str) -> Arc<Self> {
        Arc::new(Self {
            name: name.into(),
            fail: AtomicU32::new(0),
            calls: AtomicU32::new(0),
            endless: false,
        })
    }

    fn failing_once(name: &str) -> Arc<Self> {
        let p = Self::new(name);
        p.fail.store(1, Ordering::SeqCst);
        p
    }

    fn endless(name: &str) -> Arc<Self> {
        Arc::new(Self {
            name: name.into(),
            fail: AtomicU32::new(0),
            calls: AtomicU32::new(0),
            endless: true,
        })
    }

    fn take_failure(&self) -> Option<UpstreamError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.fail
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |f| f.checked_sub(1))
            .ok()
            .map(|_| {
                UpstreamError::new(
                    ErrorClass::Overloaded,
                    ApiError::new(ErrorKind::Overloaded, "überlastet"),
                )
            })
    }
}

fn usage() -> Usage {
    Usage {
        input_tokens: 7,
        output_tokens: 3,
        ..Usage::default()
    }
}

impl ChatProvider for TestProvider {
    fn describe(&self) -> Describe<'_> {
        Describe {
            deployment: &self.name,
            deployment_id: None,
            model_group: Some("m"),
            provider: Some(ProviderKind::OpenAi),
            provider_model: "gpt-test",
        }
    }

    fn chat(self: Arc<Self>, req: ChatRequest) -> BoxFuture<Result<ChatResponse, UpstreamError>> {
        let fail = self.take_failure();
        Box::pin(async move {
            if let Some(e) = fail {
                return Err(e);
            }
            Ok(ChatResponse {
                id: "r1".into(),
                model: req.model,
                created: None,
                choices: vec![Choice {
                    index: 0,
                    message: Message {
                        role: Role::Assistant,
                        content: Content::Text("ok".into()),
                        name: None,
                        extra: Default::default(),
                    },
                    stop_reason: Some(StopReason::EndTurn),
                    stop_sequence: None,
                    extra: Default::default(),
                }],
                usage: Some(usage()),
                extra: Default::default(),
                source: None,
            })
        })
    }

    fn chat_stream(
        self: Arc<Self>,
        req: ChatRequest,
    ) -> BoxFuture<Result<EventStream, UpstreamError>> {
        let fail = self.take_failure();
        let endless = self.endless;
        Box::pin(async move {
            if let Some(e) = fail {
                return Err(e);
            }
            let start = futures_util::stream::iter([StreamEvent::Start {
                id: "s1".into(),
                model: req.model,
                created: None,
            }]);
            let s: EventStream = if endless {
                // Alle 10 ms ein Textstück (4 Bytes ≈ 1 Token), ohne Ende.
                let ticks = futures_util::stream::unfold((), |()| async {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    Some((
                        StreamEvent::TextDelta {
                            index: 0,
                            text: "abcd".into(),
                        },
                        (),
                    ))
                });
                Box::pin(futures_util::StreamExt::chain(start, ticks))
            } else {
                Box::pin(futures_util::StreamExt::chain(
                    start,
                    futures_util::stream::iter([
                        StreamEvent::TextDelta {
                            index: 0,
                            text: "ok".into(),
                        },
                        StreamEvent::Stop {
                            reason: StopReason::EndTurn,
                            stop_sequence: None,
                        },
                        StreamEvent::Usage(usage()),
                    ]),
                ))
            };
            Ok(s)
        })
    }
}

#[derive(Debug, Default)]
struct Capture(Mutex<Vec<RequestRecord>>);

impl RequestObserver for Capture {
    fn record(&self, record: RequestRecord) {
        self.0.lock().unwrap().push(record);
    }
}

impl Capture {
    async fn wait(&self, n: usize) -> Vec<RequestRecord> {
        for _ in 0..400 {
            {
                let v = self.0.lock().unwrap();
                if v.len() >= n {
                    return v.clone();
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("weniger als {n} Datensätze");
    }
}

#[derive(Debug, Default)]
struct Events(Mutex<Vec<BudgetEvent>>);

impl BudgetEventSink for Events {
    fn on_event(&self, ev: &BudgetEvent) {
        self.0.lock().unwrap().push(ev.clone());
    }
}

// ---------------------------------------------------------------------------------------------
// Aufbau: Snapshot mit Organisation, Team, Key, Modellgruppe „m“, Preis
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Fixture {
    key: ApiKeySpec,
    team: TeamSpec,
    org: OrganizationSpec,
    group: ModelGroupSpec,
    prices: Vec<PriceSpec>,
}

struct Ids {
    key: r::Id,
    team: r::Id,
    org: r::Id,
    group: r::Id,
}

fn lim(metric: LimitMetric, limit: u64) -> LimitSpec {
    LimitSpec {
        metric,
        limit,
        window: LimitWindow::Minute,
        model_group: None,
    }
}

fn obj<S>(name: &str, spec: S) -> Object<S> {
    Object {
        id: new_id(),
        metadata: ObjectMeta::named(name),
        spec,
    }
}

impl Fixture {
    fn build(self) -> (SnapshotHandle, Ids) {
        let org = Arc::new(obj("o", self.org));
        let team = Arc::new(obj(
            "t",
            TeamSpec {
                organization: "o".into(),
                ..self.team
            },
        ));
        let key = r::ApiKey {
            id: new_id(),
            metadata: ObjectMeta::named("k"),
            spec: ApiKeySpec {
                team: Some("t".into()),
                ..self.key
            },
            status: ApiKeyStatus {
                prefix: "sk".into(),
                state: r::ApiKeyState::Active,
                last_used_at: None,
            },
        };
        let group = Arc::new(ModelGroupCtx {
            group: obj("m", self.group),
            deployments: Vec::new(),
        });
        let ids = Ids {
            key: key.id,
            team: team.id,
            org: org.id,
            group: group.group.id,
        };
        let mut snap = Snapshot {
            version: 1,
            ..Snapshot::default()
        };
        snap.keys.insert(
            hash_key(SECRET),
            Arc::new(KeyCtx {
                key,
                team: Some(team),
                organization: Some(org),
                expires_at: None,
            }),
        );
        snap.model_groups.insert("m".into(), group);
        for p in self.prices {
            snap.prices
                .entry((p.provider, p.model.clone()))
                .or_default()
                .push(Arc::new(obj("p", p)));
        }
        for list in snap.prices.values_mut() {
            list.sort_by_key(|p| p.spec.valid_from);
        }
        (SnapshotHandle::new(snap), ids)
    }
}

fn single_route(p: Arc<TestProvider>) -> Arc<StaticRoutes> {
    let mut routes = StaticRoutes::new();
    routes.insert_route("m", Arc::new(GroupRoute::single("m", p)));
    Arc::new(routes)
}

struct Gw {
    addr: SocketAddr,
    counters: Arc<LocalCounters>,
    capture: Arc<Capture>,
}

async fn serve(state: ProxyState) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, aigw_proxy::router(state))
            .await
            .unwrap();
    });
    addr
}

async fn gateway(
    snap: &SnapshotHandle,
    routes: Arc<StaticRoutes>,
    counters: Arc<dyn CounterBackend>,
    budgets: Option<Arc<Budgets>>,
) -> (SocketAddr, Arc<Capture>) {
    let capture = Arc::new(Capture::default());
    let state = ProxyState {
        auth: Arc::new(SnapshotAuthenticator::new(snap.clone())),
        observer: capture.clone(),
        limits: Arc::new(Limiter::new(counters, budgets, &LimitsConfig::default())),
        ..ProxyState::with_routes(routes)
    };
    (serve(state).await, capture)
}

async fn local_gateway(fx: Fixture) -> (Gw, Ids) {
    let (snap, ids) = fx.build();
    let counters = Arc::new(LocalCounters::new());
    let (addr, capture) = gateway(
        &snap,
        single_route(TestProvider::new("d")),
        counters.clone(),
        None,
    )
    .await;
    (
        Gw {
            addr,
            counters,
            capture,
        },
        ids,
    )
}

struct Resp {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|v| v.to_str().unwrap())
    }
}

async fn post(addr: SocketAddr, path: &str, body: Value) -> Resp {
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let mut req = Request::builder()
        .method(Method::POST)
        .uri(format!("http://{addr}{path}"))
        .header("content-type", "application/json");
    req = if path == "/v1/messages" {
        req.header("x-api-key", SECRET)
            .header("anthropic-version", "2023-06-01")
    } else {
        req.header("authorization", format!("Bearer {SECRET}"))
    };
    let res = client
        .request(
            req.body(Full::new(serde_json::to_vec(&body).unwrap().into()))
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

/// OpenAI-Anfrage: „hello“ (Heuristik: 2 Tokens, + 3 je Nachricht + 3 Antworteinleitung = 8)
/// + `max_tokens` 10 → Reservierung 18 Tokens.
async fn chat(addr: SocketAddr) -> Resp {
    post(
        addr,
        "/v1/chat/completions",
        json!({"model": "m", "max_tokens": 10, "messages": [{"role": "user", "content": "hello"}]}),
    )
    .await
}

async fn wait_counter(c: &LocalCounters, key: &str, want: u64) {
    for _ in 0..200 {
        if c.current(key, Window::Minute) == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(c.current(key, Window::Minute), want, "{key}");
}

// ---------------------------------------------------------------------------------------------
// AIGW-30: Limits
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn request_limits_apply_on_every_level() {
    let rpm = || vec![lim(LimitMetric::Requests, 2)];
    let cases: [(&str, Fixture); 4] = [
        (
            "key",
            Fixture {
                key: ApiKeySpec {
                    limits: rpm(),
                    ..ApiKeySpec::default()
                },
                ..Fixture::default()
            },
        ),
        (
            "team",
            Fixture {
                team: TeamSpec {
                    limits: rpm(),
                    ..TeamSpec::default()
                },
                ..Fixture::default()
            },
        ),
        (
            "org",
            Fixture {
                org: OrganizationSpec {
                    limits: rpm(),
                    ..OrganizationSpec::default()
                },
                ..Fixture::default()
            },
        ),
        (
            "group",
            Fixture {
                group: ModelGroupSpec {
                    limits: rpm(),
                    ..ModelGroupSpec::default()
                },
                ..Fixture::default()
            },
        ),
    ];
    for (level, fx) in cases {
        let (gw, ids) = local_gateway(fx).await;
        let r1 = chat(gw.addr).await;
        assert_eq!(r1.status, StatusCode::OK, "{level}");
        assert_eq!(r1.header("x-ratelimit-limit-requests"), Some("2"));
        assert_eq!(r1.header("x-ratelimit-remaining-requests"), Some("1"));
        assert!(r1.header("x-ratelimit-reset-requests").is_some());
        assert_eq!(chat(gw.addr).await.status, StatusCode::OK, "{level}");
        let r3 = chat(gw.addr).await;
        assert_eq!(r3.status, StatusCode::TOO_MANY_REQUESTS, "{level}");
        let v = r3.json();
        assert_eq!(v["error"]["code"], "rate_limit_exceeded", "{level}: {v}");
        assert_eq!(r3.header("x-ratelimit-remaining-requests"), Some("0"));
        let retry: u64 = r3.header("retry-after").unwrap().parse().unwrap();
        assert!((1..=120).contains(&retry), "{level}: Retry-After {retry}");
        let id = match level {
            "key" => format!("key:{}", ids.key),
            "team" => format!("team:{}", ids.team),
            "org" => format!("org:{}", ids.org),
            _ => format!("group:{}", ids.group),
        };
        assert_eq!(
            gw.counters
                .current(&format!("{id}:requests"), Window::Minute),
            2,
            "{level}: abgelehnte Anfrage nicht gezählt"
        );
        // Abgelehnte Anfragen erscheinen im Log (429), ohne Kosten.
        let recs = gw.capture.wait(3).await;
        assert_eq!(recs[2].status, 429);
        assert_eq!(recs[2].cost, None);
    }
}

#[tokio::test]
async fn reservation_is_atomic_across_levels() {
    let (gw, ids) = local_gateway(Fixture {
        key: ApiKeySpec {
            limits: vec![
                lim(LimitMetric::Requests, 5),
                lim(LimitMetric::Tokens, 1000),
            ],
            ..ApiKeySpec::default()
        },
        team: TeamSpec {
            limits: vec![lim(LimitMetric::Requests, 1)],
            ..TeamSpec::default()
        },
        ..Fixture::default()
    })
    .await;
    assert_eq!(chat(gw.addr).await.status, StatusCode::OK);
    let r = chat(gw.addr).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    // Abgelehnt vom Team-Limit: auch beim Key nichts gebucht.
    let k = format!("key:{}", ids.key);
    assert_eq!(
        gw.counters
            .current(&format!("{k}:requests"), Window::Minute),
        1
    );
    wait_counter(&gw.counters, &format!("{k}:tokens"), 10).await;
}

#[tokio::test]
async fn tokens_are_reserved_and_settled_per_metric() {
    let (gw, ids) = local_gateway(Fixture {
        key: ApiKeySpec {
            limits: vec![
                lim(LimitMetric::Tokens, 1000),
                lim(LimitMetric::InputTokens, 1000),
                lim(LimitMetric::OutputTokens, 1000),
            ],
            ..ApiKeySpec::default()
        },
        ..Fixture::default()
    })
    .await;
    let r = chat(gw.addr).await;
    assert_eq!(r.status, StatusCode::OK);
    // Bei Erfolg: knappster Token-Rest nach der Reservierung (1000 − 18).
    assert_eq!(r.header("x-ratelimit-limit-tokens"), Some("1000"));
    assert_eq!(r.header("x-ratelimit-remaining-tokens"), Some("982"));
    let k = format!("key:{}", ids.key);
    wait_counter(&gw.counters, &format!("{k}:tokens"), 10).await;
    wait_counter(&gw.counters, &format!("{k}:input-tokens"), 7).await;
    wait_counter(&gw.counters, &format!("{k}:output-tokens"), 3).await;
}

#[tokio::test]
async fn anthropic_clients_get_anthropic_headers_and_errors() {
    let (gw, _) = local_gateway(Fixture {
        group: ModelGroupSpec {
            limits: vec![
                lim(LimitMetric::Tokens, 20),
                lim(LimitMetric::Requests, 100),
            ],
            ..ModelGroupSpec::default()
        },
        ..Fixture::default()
    })
    .await;
    let body =
        json!({"model": "m", "max_tokens": 10, "messages": [{"role": "user", "content": "hello"}]});
    let r = post(gw.addr, "/v1/messages", body.clone()).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert_eq!(r.header("anthropic-ratelimit-tokens-limit"), Some("20"));
    assert_eq!(r.header("anthropic-ratelimit-tokens-remaining"), Some("2"));
    assert_eq!(r.header("anthropic-ratelimit-requests-limit"), Some("100"));
    let reset = r.header("anthropic-ratelimit-tokens-reset").unwrap();
    assert!(
        time::OffsetDateTime::parse(reset, &time::format_description::well_known::Rfc3339).is_ok(),
        "{reset}"
    );
    assert!(r.header("x-ratelimit-limit-tokens").is_none());

    // Zweite Anfrage: 10 (abgeglichen) bzw. 18 (noch reserviert) + 18 > 20.
    let r = post(gw.addr, "/v1/messages", body).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    let v = r.json();
    assert_eq!(v["type"], "error");
    assert_eq!(v["error"]["type"], "rate_limit_error");
    assert!(r.header("retry-after").is_some());
    assert_eq!(r.header("anthropic-ratelimit-tokens-limit"), Some("20"));
    assert!(r.header("anthropic-ratelimit-tokens-remaining").is_some());
    // Nicht geprüfte Metrik (Anfragen) fehlt bei Ablehnung.
    assert!(r.header("anthropic-ratelimit-requests-limit").is_none());
}

#[tokio::test]
async fn stream_abort_settles_with_estimated_usage() {
    let (snap, ids) = Fixture {
        key: ApiKeySpec {
            limits: vec![lim(LimitMetric::Tokens, 100_000)],
            ..ApiKeySpec::default()
        },
        ..Fixture::default()
    }
    .build();
    let counters = Arc::new(LocalCounters::new());
    let (addr, capture) = gateway(
        &snap,
        single_route(TestProvider::endless("d")),
        counters.clone(),
        None,
    )
    .await;
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let body = json!({"model": "m", "max_tokens": 1000, "stream": true,
        "messages": [{"role": "user", "content": "hello"}]});
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("http://{addr}/v1/chat/completions"))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {SECRET}"))
        .body(Full::new(serde_json::to_vec(&body).unwrap().into()))
        .unwrap();
    let res = client.request(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let key = format!("key:{}:tokens", ids.key);
    // Während des Streams: volle Reservierung (8 + 1000).
    assert_eq!(counters.current(&key, Window::Minute), 1008);
    let mut body = res.into_body();
    for _ in 0..5 {
        body.frame().await.unwrap().unwrap();
    }
    drop(body);
    drop(client);
    let recs = capture.wait(1).await;
    let rec = &recs[0];
    assert!(rec.client_aborted);
    assert!(rec.usage_estimated);
    let actual = rec.usage.total_tokens();
    assert!((9..1008).contains(&actual), "geschätzt {actual}");
    wait_counter(&counters, &key, actual).await;
}

#[tokio::test]
async fn retries_use_one_reservation_per_client_request() {
    let (snap, ids) = Fixture {
        key: ApiKeySpec {
            limits: vec![
                lim(LimitMetric::Requests, 100),
                lim(LimitMetric::Tokens, 1000),
            ],
            ..ApiKeySpec::default()
        },
        ..Fixture::default()
    }
    .build();
    let a = TestProvider::failing_once("a");
    let b = TestProvider::new("b");
    let cap = Capacity {
        rpm: Some(100),
        tpm: Some(10_000),
    };
    let members = vec![
        MemberSpec {
            capacity: cap,
            ..MemberSpec::new(a.clone())
        },
        MemberSpec {
            priority: 1,
            capacity: cap,
            ..MemberSpec::new(b.clone())
        },
    ];
    let route = GroupRoute::new(
        "m",
        RoutingStrategy::Weighted,
        GroupPolicy::from_settings(&JsonObject::new()).unwrap(),
        Vec::new(),
        members,
    );
    let mut routes = StaticRoutes::new();
    routes.insert_route("m", Arc::new(route));
    let counters = Arc::new(LocalCounters::new());
    let (addr, capture) = gateway(&snap, Arc::new(routes), counters.clone(), None).await;
    let r = chat(addr).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header("x-aigw-attempts"), Some("2"));
    assert_eq!(
        (
            a.calls.load(Ordering::SeqCst),
            b.calls.load(Ordering::SeqCst)
        ),
        (1, 1)
    );
    let recs = capture.wait(1).await;
    assert_eq!(recs[0].attempts.len(), 2);
    // Eine Reservierung der Client-Anfrage, aber Kapazität je Versuch.
    let k = format!("key:{}", ids.key);
    assert_eq!(
        counters.current(&format!("{k}:requests"), Window::Minute),
        1
    );
    wait_counter(&counters, &format!("{k}:tokens"), 10).await;
    assert_eq!(counters.current("dep:a:requests", Window::Minute), 1);
    assert_eq!(counters.current("dep:b:requests", Window::Minute), 1);
    // Fehlgeschlagener Versuch gibt seine Token-Reservierung zurück.
    wait_counter(&counters, "dep:a:tokens", 0).await;
    wait_counter(&counters, "dep:b:tokens", 10).await;
}

#[tokio::test]
async fn deployment_capacity_is_shared_through_counters() {
    let (snap, _) = Fixture::default().build();
    // Ein Zähler-Backend, zwei „Instanzen“ mit eigenem Routing-Zustand.
    let counters = Arc::new(LocalCounters::new());
    let routes = || {
        let members = vec![
            MemberSpec {
                capacity: Capacity {
                    rpm: Some(1),
                    tpm: None,
                },
                ..MemberSpec::new(TestProvider::new("ptu"))
            },
            MemberSpec {
                priority: 1,
                ..MemberSpec::new(TestProvider::new("payg"))
            },
        ];
        let mut routes = StaticRoutes::new();
        routes.insert_route(
            "m",
            Arc::new(GroupRoute::new(
                "m",
                RoutingStrategy::Weighted,
                GroupPolicy::from_settings(&JsonObject::new()).unwrap(),
                Vec::new(),
                members,
            )),
        );
        Arc::new(routes)
    };
    let (i1, _) = gateway(&snap, routes(), counters.clone(), None).await;
    let (i2, _) = gateway(&snap, routes(), counters.clone(), None).await;
    assert_eq!(chat(i1).await.header("x-aigw-deployment"), Some("ptu"));
    // Instanz 2 sieht die Kapazität von „ptu“ als verbraucht.
    assert_eq!(chat(i2).await.header("x-aigw-deployment"), Some("payg"));
    assert_eq!(chat(i1).await.header("x-aigw-deployment"), Some("payg"));
    assert_eq!(counters.current("dep:ptu:requests", Window::Minute), 1);
}

// ---------------------------------------------------------------------------------------------
// AIGW-31: Kosten und Budgets
// ---------------------------------------------------------------------------------------------

fn amount(s: &str) -> Amount {
    s.parse().unwrap()
}

fn price(valid_from: time::OffsetDateTime, per_million: &str) -> PriceSpec {
    PriceSpec {
        provider: None,
        model: "gpt-test".into(),
        valid_from,
        per_million_tokens: TokenPrices {
            input: Some(amount(per_million)),
            output: Some(amount(per_million)),
            ..TokenPrices::default()
        },
        per_request: None,
        per_image: None,
    }
}

#[tokio::test]
async fn cost_and_budget_with_warnings_and_hard_limit() {
    let now = time::OffsetDateTime::now_utc();
    let (snap, ids) = Fixture {
        key: ApiKeySpec {
            budget: Some(Budget {
                amount: amount("0.025"),
                period: BudgetPeriod::Month,
                warn_at_percent: vec![50],
                hard_limit: true,
                enforcement: aigw_core::resource::BudgetEnforcement::Soft,
            }),
            ..ApiKeySpec::default()
        },
        // Team-Budget ohne harte Grenze: nur gebucht.
        team: TeamSpec {
            budget: Some(Budget {
                amount: amount("100"),
                period: BudgetPeriod::Day,
                warn_at_percent: vec![],
                hard_limit: false,
                enforcement: Default::default(),
            }),
            ..TeamSpec::default()
        },
        // Alter Preis, aktueller Preis (1000 je 1 Mio. → 10 Tokens = 0,01), künftiger Preis.
        prices: vec![
            price(now - time::Duration::days(30), "1"),
            price(now - time::Duration::hours(1), "1000"),
            price(now + time::Duration::days(1), "999999"),
        ],
        ..Fixture::default()
    }
    .build();
    let events = Arc::new(Events::default());
    let budgets = Arc::new(Budgets::new(Calendar::utc(), None).with_events(events.clone()));
    let (addr, capture) = gateway(
        &snap,
        single_route(TestProvider::new("d")),
        Arc::new(LocalCounters::new()),
        Some(budgets.clone()),
    )
    .await;

    assert_eq!(chat(addr).await.status, StatusCode::OK); // 0,01 (40 %)
    assert!(events.0.lock().unwrap().is_empty());
    assert_eq!(chat(addr).await.status, StatusCode::OK); // 0,02 (80 %) → Warnung 50 %
    assert_eq!(
        events.0.lock().unwrap()[0].kind,
        BudgetEventKind::Warning { percent: 50 }
    );
    assert_eq!(chat(addr).await.status, StatusCode::OK); // 0,03 → erschöpft
    let r = chat(addr).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.json()["error"]["code"], "budget_exceeded");
    let retry: u64 = r.header("retry-after").unwrap().parse().unwrap();
    assert!(retry > 0 && retry <= 31 * 86_400);

    let kinds: Vec<_> = events.0.lock().unwrap().iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        [
            BudgetEventKind::Warning { percent: 50 },
            BudgetEventKind::Exhausted
        ]
    );
    let recs = capture.wait(4).await;
    for rec in &recs[..3] {
        assert_eq!(rec.cost, Some(amount("0.01")), "Preis zum Anfragezeitpunkt");
    }
    assert_eq!(recs[3].cost, None);
    assert_eq!(recs[3].status, 429);
    // Team-Budget mitgebucht.
    let team_budget = Budget {
        amount: amount("100"),
        period: BudgetPeriod::Day,
        warn_at_percent: vec![],
        hard_limit: false,
        enforcement: Default::default(),
    };
    let (key, _) = budgets.key(
        &aigw_billing::BudgetTarget {
            level: aigw_billing::BudgetLevel::Team,
            target: ids.team,
            budget: &team_budget,
        },
        i64::try_from(now.unix_timestamp_nanos() / 1_000_000).unwrap(),
    );
    assert_eq!(budgets.used(&key), Some(amount("0.03")));
}

#[tokio::test]
async fn streams_are_priced_too() {
    let (snap, _) = Fixture {
        prices: vec![price(
            time::OffsetDateTime::now_utc() - time::Duration::hours(1),
            "1000",
        )],
        ..Fixture::default()
    }
    .build();
    let (addr, capture) = gateway(
        &snap,
        single_route(TestProvider::new("d")),
        Arc::new(LocalCounters::new()),
        None,
    )
    .await;
    let r = post(
        addr,
        "/v1/chat/completions",
        json!({"model": "m", "stream": true, "messages": [{"role": "user", "content": "hello"}]}),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let recs = capture.wait(1).await;
    assert_eq!(recs[0].cost, Some(amount("0.01")));
}

// ---------------------------------------------------------------------------------------------
// Entfernte Zähler-Backends: zwei Instanzen teilen ein Limit
// ---------------------------------------------------------------------------------------------

async fn shared_limit(
    c1: Arc<dyn CounterBackend>,
    c2: Arc<dyn CounterBackend>,
    sync: impl AsyncFn(),
) {
    let (snap, _) = Fixture {
        key: ApiKeySpec {
            limits: vec![lim(LimitMetric::Requests, 3)],
            ..ApiKeySpec::default()
        },
        team: TeamSpec {
            limits: vec![lim(LimitMetric::Tokens, 1_000)],
            ..TeamSpec::default()
        },
        ..Fixture::default()
    }
    .build();
    let (i1, _) = gateway(&snap, single_route(TestProvider::new("d")), c1, None).await;
    let (i2, _) = gateway(&snap, single_route(TestProvider::new("d")), c2, None).await;
    assert_eq!(chat(i1).await.status, StatusCode::OK);
    assert_eq!(chat(i1).await.status, StatusCode::OK);
    sync().await;
    assert_eq!(chat(i2).await.status, StatusCode::OK);
    sync().await;
    assert_eq!(chat(i2).await.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(chat(i1).await.status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn redis_backend_shares_limits() {
    let Ok(url) = std::env::var("AIGW_TEST_REDIS_URL") else {
        eprintln!("AIGW_TEST_REDIS_URL nicht gesetzt – Redis-Test übersprungen");
        return;
    };
    let opts = || aigw_counters::RedisOptions {
        key_prefix: format!("aigwlimits:{}:", new_id().simple()),
        ..aigw_counters::RedisOptions::default()
    };
    let o = opts();
    let a = aigw_counters::RedisCounters::connect(&url, o.clone())
        .await
        .unwrap();
    let b = aigw_counters::RedisCounters::connect(&url, o)
        .await
        .unwrap();
    assert!(!a.is_degraded());
    shared_limit(a, b, async || {}).await;
}

#[tokio::test]
async fn postgres_sync_backend_shares_limits() {
    let Ok(base) = std::env::var("AIGW_TEST_POSTGRES_URL") else {
        eprintln!("AIGW_TEST_POSTGRES_URL nicht gesetzt – postgres-sync-Test übersprungen");
        return;
    };
    let db = format!("aigw_limits_{}", new_id().simple());
    let admin = sqlx::PgPool::connect(&base).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {db}")))
        .execute(&admin)
        .await
        .unwrap();
    let (prefix, _) = base.rsplit_once('/').unwrap();
    let url = format!("{prefix}/{db}");
    let store = aigw_store::Store::connect(&url).await.unwrap();
    store.migrate().await.unwrap();
    let pool = || async { sqlx::PgPool::connect(&url).await.unwrap() };
    let opts = aigw_counters::PgSyncOptions::default();
    let a = Arc::new(aigw_counters::PgSyncCounters::new_manual(
        pool().await,
        opts.clone(),
    ));
    let b = Arc::new(aigw_counters::PgSyncCounters::new_manual(
        pool().await,
        opts,
    ));
    let (sa, sb) = (Arc::clone(&a), Arc::clone(&b));
    shared_limit(a, b, async || {
        // Beide Richtungen: schreiben, dann den fremden Stand lesen.
        sa.sync_now().await.unwrap();
        sb.sync_now().await.unwrap();
        sa.sync_now().await.unwrap();
    })
    .await;
    store.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE {db} WITH (FORCE)"
    )))
    .execute(&admin)
    .await
    .unwrap();
}

// ---------------------------------------------------------------------------------------------
// AIGW-36: Plugin-Reject/-Respond nach der Admission
// ---------------------------------------------------------------------------------------------

/// Lehnt „verboten“ ab und beantwortet „ping“ selbst (ohne Usage).
struct GatePlugin;

impl aigw_plugins::Plugin for GatePlugin {
    fn name(&self) -> &'static str {
        "gate"
    }
    fn phases(&self) -> aigw_plugins::Phases {
        aigw_plugins::Phases::REQUEST
    }
    fn on_request<'a>(
        &'a self,
        ctx: &'a mut aigw_plugins::ReqCtx<'_>,
    ) -> aigw_plugins::Hook<'a, aigw_plugins::Flow> {
        let text = ctx.request.last_user_text().unwrap_or_default();
        aigw_plugins::Hook::ok(if text.contains("verboten") {
            aigw_plugins::Flow::Reject(ApiError::new(ErrorKind::PermissionDenied, "nein"))
        } else if text == "ping" {
            aigw_plugins::Flow::Respond(Box::new(ChatResponse {
                id: "p".into(),
                model: "m".into(),
                created: None,
                choices: vec![Choice {
                    index: 0,
                    message: Message {
                        role: Role::Assistant,
                        content: Content::Text("pong".into()),
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
            }))
        } else {
            aigw_plugins::Flow::Continue
        })
    }
}

#[tokio::test]
async fn plugin_reject_and_respond_release_the_token_reservation() {
    let (snap, ids) = Fixture {
        key: ApiKeySpec {
            limits: vec![
                lim(LimitMetric::Requests, 100),
                lim(LimitMetric::Tokens, 1000),
            ],
            ..ApiKeySpec::default()
        },
        ..Fixture::default()
    }
    .build();
    let counters = Arc::new(LocalCounters::new());
    let provider = TestProvider::new("d");
    let state = ProxyState {
        auth: Arc::new(SnapshotAuthenticator::new(snap.clone())),
        limits: Arc::new(Limiter::new(
            counters.clone(),
            None,
            &LimitsConfig::default(),
        )),
        plugins: Arc::new(aigw_proxy::PluginChains::fixed(aigw_plugins::Chain::new(
            vec![Arc::new(aigw_plugins::Entry::new(
                "gate",
                Arc::new(GatePlugin),
            ))],
        ))),
        ..ProxyState::with_routes(single_route(provider.clone()))
    };
    let addr = serve(state).await;
    let k = format!("key:{}", ids.key);
    for (text, stream, status) in [
        ("das ist verboten", false, StatusCode::FORBIDDEN),
        ("ping", false, StatusCode::OK),
        ("ping", true, StatusCode::OK),
    ] {
        let r = post(
            addr,
            "/v1/chat/completions",
            json!({"model": "m", "max_tokens": 10, "stream": stream,
                   "messages": [{"role": "user", "content": text}]}),
        )
        .await;
        assert_eq!(r.status, status, "{text}");
    }
    // Drei Anfragen zählen, die Token-Reservierungen (je 18) sind vollständig freigegeben.
    wait_counter(&counters, &format!("{k}:requests"), 3).await;
    wait_counter(&counters, &format!("{k}:tokens"), 0).await;
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);

    // Normale Anfrage: Reservierung wird mit der echten Usage abgeglichen (wie ohne Plugin).
    assert_eq!(chat(addr).await.status, StatusCode::OK);
    wait_counter(&counters, &format!("{k}:tokens"), 10).await;
}

// ---------------------------------------------------------------------------------------------
// AIGW-71 (F106): strikte Budgets – geschätzte Kosten vorab reservieren
// ---------------------------------------------------------------------------------------------

/// Snapshot mit Team-Budget 0,1 (strict) und Preis 1000 je 1 Mio. Tokens; die Gruppe „m“
/// erhält ein Deployment (OpenAI „gpt-test“), damit die Reservierung einen Preis findet.
/// Schätzung je Anfrage: (8 + 10) Tokens = 0,018; tatsächlich 10 Tokens = 0,01.
fn strict_snapshot() -> (SnapshotHandle, Ids) {
    let now = time::OffsetDateTime::now_utc();
    let (snap, ids) = Fixture {
        team: TeamSpec {
            budget: Some(Budget {
                amount: amount("0.1"),
                period: BudgetPeriod::Month,
                warn_at_percent: vec![],
                hard_limit: true,
                enforcement: aigw_core::resource::BudgetEnforcement::Strict,
            }),
            ..TeamSpec::default()
        },
        prices: vec![price(now - time::Duration::hours(1), "1000")],
        ..Fixture::default()
    }
    .build();
    let mut s = (*snap.load()).clone();
    let g = s.model_groups.get("m").unwrap();
    let mut g = (**g).clone();
    g.deployments.push(aigw_core::snapshot::DeploymentCtx {
        deployment: Object {
            id: new_id(),
            metadata: ObjectMeta::named("d"),
            spec: r::DeploymentSpec {
                model_group: "m".into(),
                provider: ProviderKind::OpenAi,
                endpoint: None,
                provider_model: "gpt-test".into(),
                credential: None,
                weight: 1,
                priority: 0,
                capacity: Capacity::default(),
                enabled: true,
                settings: JsonObject::new(),
            },
        },
        credential: None,
    });
    s.model_groups.insert("m".into(), Arc::new(g));
    (SnapshotHandle::new(s), ids)
}

/// `n` parallele Anfragen gegen alle `addrs` (reihum); liefert (zugelassen, abgelehnt).
async fn parallel_chats(addrs: &[SocketAddr], n: usize) -> (usize, usize) {
    let mut tasks = Vec::new();
    for i in 0..n {
        let addr = addrs[i % addrs.len()];
        tasks.push(tokio::spawn(async move { chat(addr).await }));
    }
    let (mut ok, mut rej) = (0, 0);
    for t in tasks {
        let r = t.await.unwrap();
        if r.status == StatusCode::OK {
            ok += 1;
        } else {
            assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
            assert_eq!(r.json()["error"]["code"], "budget_exceeded");
            rej += 1;
        }
    }
    (ok, rej)
}

fn total_cost(recs: &[RequestRecord]) -> Amount {
    Amount::from_nanos(recs.iter().filter_map(|r| r.cost).map(|c| c.nanos()).sum())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn strict_budget_parallel_requests_never_exceed_locally() {
    let (snap, _) = strict_snapshot();
    let budgets = Arc::new(Budgets::new(Calendar::utc(), None));
    let (addr, capture) = gateway(
        &snap,
        single_route(TestProvider::new("d")),
        Arc::new(LocalCounters::new()),
        Some(budgets),
    )
    .await;
    let (ok, rej) = parallel_chats(&[addr], 40).await;
    let recs = capture.wait(40).await;
    let total = total_cost(&recs);
    assert!(total <= amount("0.1"), "{total} ({ok} zugelassen)");
    assert!(ok >= 5 && rej > 0, "{ok}/{rej}");
    // Nach dem Abgleich passt eine weitere Anfrage nur, solange der Rest die Schätzung trägt.
    let r = chat(addr).await;
    if total.nanos() + amount("0.018").nanos() > amount("0.1").nanos() {
        assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    }
}

async fn strict_two_instances(shared: Arc<dyn aigw_billing::SharedBudgets>) {
    let (snap, _) = strict_snapshot();
    let ledger = Arc::new(aigw_billing::MemoryLedger::default());
    let mk = || {
        Arc::new(
            Budgets::new(Calendar::utc(), Some(ledger.clone())).with_shared(Arc::clone(&shared)),
        )
    };
    let (a1, c1) = gateway(
        &snap,
        single_route(TestProvider::new("d")),
        Arc::new(LocalCounters::new()),
        Some(mk()),
    )
    .await;
    let (a2, c2) = gateway(
        &snap,
        single_route(TestProvider::new("d")),
        Arc::new(LocalCounters::new()),
        Some(mk()),
    )
    .await;
    let (ok, _) = parallel_chats(&[a1, a2], 40).await;
    for _ in 0..200 {
        if c1.wait(0).await.len() + c2.wait(0).await.len() >= 40 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let mut recs = c1.wait(0).await;
    recs.extend(c2.wait(0).await);
    let total = total_cost(&recs);
    assert!(total <= amount("0.1"), "{total} ({ok} zugelassen)");
    assert!(ok >= 5, "{ok}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn strict_budget_exact_across_instances_with_shared_counter() {
    strict_two_instances(Arc::new(aigw_billing::MemoryShared::default())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn strict_budget_exact_across_instances_with_redis() {
    let Ok(url) = std::env::var("AIGW_TEST_REDIS_URL") else {
        eprintln!("AIGW_TEST_REDIS_URL nicht gesetzt – Redis-Budget-Test übersprungen");
        return;
    };
    let prefix = format!("aigwbudget:{}:", new_id().simple());
    strict_two_instances(Arc::new(
        aigw_proxy::RedisBudgets::new(&url, &prefix).unwrap(),
    ))
    .await;
}

#[tokio::test]
async fn redis_budgets_reserve_and_adjust() {
    use aigw_billing::{BudgetKey, BudgetLevel, SharedBudgets, SharedItem, SharedReserve};
    let Ok(url) = std::env::var("AIGW_TEST_REDIS_URL") else {
        eprintln!("AIGW_TEST_REDIS_URL nicht gesetzt – übersprungen");
        return;
    };
    let rb =
        aigw_proxy::RedisBudgets::new(&url, &format!("aigwrb:{}:", new_id().simple())).unwrap();
    let key = BudgetKey {
        level: BudgetLevel::Team,
        target: new_id(),
        period: BudgetPeriod::Total,
        period_start_ms: 0,
    };
    let item = |amount| SharedItem {
        key,
        base: 40,
        limit: 100,
        amount,
        expire_at_ms: None,
    };
    assert_eq!(
        rb.reserve(&[item(50)]).await.unwrap(),
        SharedReserve::Reserved
    );
    assert_eq!(
        rb.reserve(&[item(20)]).await.unwrap(),
        SharedReserve::Rejected {
            index: 0,
            current: 90
        }
    );
    rb.adjust(&[(key, -30)]).await.unwrap();
    assert_eq!(
        rb.reserve(&[item(20)]).await.unwrap(),
        SharedReserve::Reserved
    );
}
