// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Proxy-/Routing-Punkte aus der Fragerunde (AIGW-70) gegen lokale Fake-Upstreams:
//! präzise Modellfehler (F39), Fallback nur auf freigegebene Gruppen (F80), 429 bei reiner
//! Drosselung (F83), Modellname der Antwort (F49), `anthropic-beta` der Clients (F60),
//! Reasoning für OpenAI-Clients (F10), `n > 1` nativ und per parallelen Anfragen auch im
//! Stream und in der Responses API (F12), Zeitlimits je Modellgruppe (F44), Trace-Weitergabe
//! je Anbieterart (F98), gesperrter Mock-Anbieter (F85) und `client_request_id` (F11).
//! Keine externen Aufrufe.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aigw_core::observe::{RequestObserver, RequestRecord};
use aigw_core::resource::{
    self as r, ApiKeySpec, ApiKeyStatus, Capacity, CredentialData, Deployment, DeploymentSpec,
    Fallback, JsonObject, ModelGroup, ModelGroupSpec, ObjectMeta, SecretString, new_id,
};
use aigw_core::snapshot::{DeploymentCtx, KeyCtx, ModelGroupCtx, Snapshot, SnapshotHandle};
use aigw_core::types::ProviderKind;
use aigw_providers::{Registry, Upstream, UpstreamClient};
use aigw_proxy::auth::hash_key;
use aigw_proxy::{ModelRoutes, ProxyState, SnapshotAuthenticator, SnapshotRoutes};
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
// Fake-Upstreams
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Fake {
    /// (Anbieter-Modell, `n` der Anfrage) je Aufruf.
    hits: Mutex<Vec<(String, Option<u64>)>>,
    /// `anthropic-beta` je Aufruf an `/v1/messages`.
    betas: Mutex<Vec<Option<String>>>,
    /// `traceparent` je Aufruf.
    traceparents: Mutex<Vec<Option<String>>>,
}

impl Fake {
    fn hits(&self) -> Vec<(String, Option<u64>)> {
        self.hits.lock().unwrap().clone()
    }
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

fn data(v: &Value) -> String {
    format!("data: {v}\n\n")
}

/// OpenAI-kompatibler Fake: Modell `echo-2024`, Reasoning in `reasoning_content`, bei `n`
/// (nur wenn mitgeschickt) entsprechend viele Varianten.
async fn chat_handler(State(fake): State<Arc<Fake>>, headers: HeaderMap, body: Bytes) -> Response {
    let body: Value = serde_json::from_slice(&body).unwrap();
    let model = body["model"].as_str().unwrap_or_default().to_owned();
    let n = body["n"].as_u64();
    let stream = body["stream"].as_bool().unwrap_or(false);
    fake.hits.lock().unwrap().push((model.clone(), n));
    fake.traceparents.lock().unwrap().push(
        headers
            .get("traceparent")
            .map(|v| v.to_str().unwrap().to_owned()),
    );
    match model.as_str() {
        "fail" => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": {"message": "boom", "type": "server_error"}}).to_string(),
            )
                .into_response();
        }
        "limited" => {
            let mut res = (
                StatusCode::TOO_MANY_REQUESTS,
                json!({"error": {"message": "slow down", "code": "rate_limit_exceeded"}})
                    .to_string(),
            )
                .into_response();
            res.headers_mut()
                .insert("retry-after", "30".parse().unwrap());
            return res;
        }
        "slow" => tokio::time::sleep(Duration::from_millis(1500)).await,
        _ => {}
    }
    let choices = n.unwrap_or(1);
    if stream {
        let mut parts = Vec::new();
        for i in 0..choices {
            parts.push(data(&json!({"id": "c1", "object": "chat.completion.chunk",
                "created": 1, "model": "echo-2024",
                "choices": [{"index": i, "delta": {"role": "assistant",
                                                   "reasoning_content": "nachgedacht"}}]})));
        }
        for i in 0..choices {
            parts.push(data(&json!({"id": "c1", "object": "chat.completion.chunk",
                "created": 1, "model": "echo-2024",
                "choices": [{"index": i, "delta": {"content": format!("v{i} von {model}")}}]})));
        }
        for i in 0..choices {
            parts.push(data(&json!({"id": "c1", "object": "chat.completion.chunk",
                "created": 1, "model": "echo-2024",
                "choices": [{"index": i, "delta": {}, "finish_reason": "stop"}]})));
        }
        parts.push(data(&json!({"id": "c1", "object": "chat.completion.chunk",
            "created": 1, "model": "echo-2024", "choices": [],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2 * choices,
                      "total_tokens": 3 + 2 * choices}})));
        parts.push("data: [DONE]\n\n".to_owned());
        return sse(parts);
    }
    let list: Vec<Value> = (0..choices)
        .map(|i| {
            json!({"index": i, "finish_reason": "stop",
                   "message": {"role": "assistant", "content": format!("v{i} von {model}"),
                               "reasoning_content": "nachgedacht"}})
        })
        .collect();
    (
        [("content-type", "application/json")],
        json!({"id": "c1", "object": "chat.completion", "created": 1, "model": "echo-2024",
               "choices": list,
               "usage": {"prompt_tokens": 3, "completion_tokens": 2 * choices,
                         "total_tokens": 3 + 2 * choices}})
        .to_string(),
    )
        .into_response()
}

/// Anthropic-Fake (`/v1/messages`): merkt sich `anthropic-beta`.
async fn messages_handler(State(fake): State<Arc<Fake>>, headers: HeaderMap) -> Response {
    fake.betas.lock().unwrap().push(
        headers
            .get("anthropic-beta")
            .map(|v| v.to_str().unwrap().to_owned()),
    );
    (
        [("content-type", "application/json")],
        json!({"id": "msg_1", "type": "message", "role": "assistant", "model": "claude-up",
               "content": [{"type": "text", "text": "hallo"}], "stop_reason": "end_turn",
               "usage": {"input_tokens": 3, "output_tokens": 1}})
        .to_string(),
    )
        .into_response()
}

async fn start_fake() -> (String, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    let app = Router::new()
        .route("/v1/chat/completions", post(chat_handler))
        .route("/v1/messages", post(messages_handler))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), fake)
}

// ---------------------------------------------------------------------------------------------
// Konfiguration
// ---------------------------------------------------------------------------------------------

fn obj(v: Value) -> JsonObject {
    match v {
        Value::Object(o) => o,
        _ => JsonObject::new(),
    }
}

fn dep(
    group: &str,
    provider: ProviderKind,
    base: &str,
    model: &str,
    settings: Value,
) -> DeploymentCtx {
    let endpoint = match provider {
        ProviderKind::Anthropic | ProviderKind::Mock => base.to_owned(),
        _ => format!("{base}/v1"),
    };
    DeploymentCtx {
        deployment: Deployment {
            id: new_id(),
            metadata: ObjectMeta::named(format!("{group}-{model}")),
            spec: DeploymentSpec {
                model_group: group.into(),
                provider,
                endpoint: Some(endpoint),
                provider_model: model.into(),
                credential: Some("c".into()),
                weight: 1,
                priority: 0,
                capacity: Capacity::default(),
                enabled: true,
                settings: obj(settings),
            },
        },
        credential: Some(Arc::new(CredentialData(BTreeMap::from([(
            "apiKey".to_owned(),
            SecretString::new("sk-up"),
        )])))),
    }
}

fn group(
    name: &str,
    settings: Value,
    fallbacks: &[&str],
    deps: Vec<DeploymentCtx>,
) -> Arc<ModelGroupCtx> {
    Arc::new(ModelGroupCtx {
        group: ModelGroup {
            id: new_id(),
            metadata: ObjectMeta::named(name),
            spec: ModelGroupSpec {
                aliases: vec![format!("{name}-alias")],
                fallbacks: fallbacks
                    .iter()
                    .map(|g| Fallback {
                        model_group: (*g).to_owned(),
                        on: Vec::new(),
                    })
                    .collect(),
                settings: obj(settings),
                ..ModelGroupSpec::default()
            },
        },
        deployments: deps,
    })
}

/// Key `sk-<name>` mit Freigaben (leer = alle) und Metadaten.
fn key(name: &str, allowed: &[&str], metadata: &[(&str, &str)]) -> (String, KeyCtx) {
    let ctx = KeyCtx {
        key: r::ApiKey {
            id: new_id(),
            metadata: ObjectMeta::named(name),
            spec: ApiKeySpec {
                allowed_models: allowed.iter().map(|m| (*m).to_owned()).collect(),
                metadata: metadata
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
                ..ApiKeySpec::default()
            },
            status: ApiKeyStatus {
                prefix: "sk".into(),
                state: r::ApiKeyState::Active,
                last_used_at: None,
            },
        },
        team: None,
        organization: None,
        expires_at: None,
    };
    (format!("sk-{name}"), ctx)
}

fn snapshot(groups: Vec<Arc<ModelGroupCtx>>, keys: Vec<(String, KeyCtx)>) -> Snapshot {
    let mut s = Snapshot {
        version: 1,
        ..Snapshot::default()
    };
    for g in groups {
        s.model_groups
            .insert(g.group.metadata.name.clone(), g.clone());
        for a in &g.group.spec.aliases {
            s.model_groups.insert(a.clone(), g.clone());
        }
    }
    for (secret, k) in keys {
        s.keys.insert(hash_key(&secret), Arc::new(k));
    }
    s
}

#[derive(Debug, Default)]
struct Capture(Mutex<Vec<RequestRecord>>);

impl RequestObserver for Capture {
    fn record(&self, record: RequestRecord) {
        self.0.lock().unwrap().push(record);
    }

    /// Wie der Request-Logger: `payloadMode` an Key/Team/Organisation, sonst aus (F30).
    fn payload_mode(&self, key: Option<&KeyCtx>) -> aigw_core::observe::PayloadMode {
        aigw_core::observe::effective_payload_mode(key, aigw_core::observe::PayloadMode::Off)
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
}

struct Gw {
    addr: SocketAddr,
    capture: Arc<Capture>,
}

async fn gateway(snap: Snapshot) -> Gw {
    gateway_with(snap, true).await
}

async fn gateway_with(snap: Snapshot, allow_mock: bool) -> Gw {
    let handle = SnapshotHandle::new(snap);
    let routes = Arc::new(
        SnapshotRoutes::new(
            Registry::with_builtin(),
            UpstreamClient::unguarded().unwrap(),
        )
        .with_mock(allow_mock),
    );
    routes.rebuild(&handle.load());
    let capture = Arc::new(Capture::default());
    let mut state = ProxyState::with_routes(routes as Arc<dyn ModelRoutes>);
    state.auth = Arc::new(SnapshotAuthenticator::new(handle));
    state.observer = capture.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, aigw_proxy::router(state))
            .await
            .unwrap();
    });
    Gw { addr, capture }
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|_| panic!("kein JSON: {}", self.body))
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|v| v.to_str().unwrap())
    }

    /// `data:`-Zeilen eines SSE-Bodys als JSON (ohne `[DONE]`).
    fn frames(&self) -> Vec<Value> {
        self.body
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter(|d| *d != "[DONE]")
            .map(|d| serde_json::from_str(d).unwrap())
            .collect()
    }
}

impl Gw {
    async fn post(&self, path: &str, key: &str, body: Value, headers: &[(&str, &str)]) -> Reply {
        let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
        let mut req = Request::builder()
            .method(Method::POST)
            .uri(format!("http://{}{path}", self.addr))
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {key}"));
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let res = client
            .request(req.body(Full::new(Bytes::from(body.to_string()))).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let body = res.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        }
    }

    async fn chat(&self, key: &str, body: Value) -> Reply {
        self.post("/v1/chat/completions", key, body, &[]).await
    }
}

fn msg(model: &str) -> Value {
    json!({"model": model, "messages": [{"role": "user", "content": "Hallo"}]})
}

/// Ohne Retries, ohne Breaker.
fn quick() -> Value {
    json!({"retry": {"maxAttempts": 1}, "circuitBreaker": {"enabled": false}})
}

// ---------------------------------------------------------------------------------------------
// F39 / F80 / F83
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn model_errors_are_precise_for_restricted_keys() {
    let (base, _) = start_fake().await;
    let snap = snapshot(
        vec![
            group(
                "a",
                quick(),
                &[],
                vec![dep(
                    "a",
                    ProviderKind::OpenAiCompatible,
                    &base,
                    "echo",
                    json!({}),
                )],
            ),
            group(
                "b",
                quick(),
                &[],
                vec![dep(
                    "b",
                    ProviderKind::OpenAiCompatible,
                    &base,
                    "echo",
                    json!({}),
                )],
            ),
        ],
        vec![key("eng", &["a"], &[])],
    );
    let gw = gateway(snap).await;
    let r = gw.chat("sk-eng", msg("gibt-es-nicht")).await;
    assert_eq!(r.status, 404, "{}", r.body);
    assert_eq!(r.json()["error"]["code"], "model_not_found");
    let r = gw.chat("sk-eng", msg("b")).await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert_eq!(r.json()["error"]["code"], "model_not_allowed");
    let r = gw.chat("sk-eng", msg("a-alias")).await;
    assert_eq!(r.status, 200, "{}", r.body);
}

#[tokio::test]
async fn fallback_skips_groups_not_granted_to_the_key() {
    let (base, fake) = start_fake().await;
    let oc = |g: &str, m: &str| dep(g, ProviderKind::OpenAiCompatible, &base, m, json!({}));
    let snap = snapshot(
        vec![
            group(
                "primary",
                quick(),
                &["secret", "public"],
                vec![oc("primary", "fail")],
            ),
            group(
                "primary-open",
                json!({"retry": {"maxAttempts": 1}, "circuitBreaker": {"enabled": false},
                       "fallbackIgnoresGrants": true}),
                &["secret", "public"],
                vec![oc("primary-open", "fail")],
            ),
            group("secret", quick(), &[], vec![oc("secret", "echo-secret")]),
            group("public", quick(), &[], vec![oc("public", "echo-public")]),
        ],
        vec![
            key("limited", &["primary", "primary-open", "public"], &[]),
            key("all", &[], &[]),
        ],
    );
    let gw = gateway(snap).await;

    // Nicht freigegebene Fallback-Gruppe wird übersprungen (F80).
    let r = gw.chat("sk-limited", msg("primary")).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.header("x-aigw-fallback"), Some("public"));
    assert_eq!(r.header("x-aigw-fallback-skipped"), Some("secret"));
    assert!(!fake.hits().iter().any(|(m, _)| m == "echo-secret"));
    let rec = gw.capture.last().await;
    assert_eq!(rec.attempts.len(), 2);

    // Key ohne Einschränkung: erste Fallback-Gruppe, nichts übersprungen.
    let r = gw.chat("sk-all", msg("primary")).await;
    assert_eq!(r.header("x-aigw-fallback"), Some("secret"));
    assert!(r.header("x-aigw-fallback-skipped").is_none());

    // settings.fallbackIgnoresGrants: Freigaben der Ziele werden nicht geprüft.
    let r = gw.chat("sk-limited", msg("primary-open")).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.header("x-aigw-fallback"), Some("secret"));
    assert!(r.header("x-aigw-fallback-skipped").is_none());
}

#[tokio::test]
async fn only_throttled_deployments_give_429_with_retry_after() {
    let (base, _) = start_fake().await;
    // Kapazität: 1 Anfrage je Minute.
    let mut capped = dep(
        "capped",
        ProviderKind::OpenAiCompatible,
        &base,
        "echo",
        json!({}),
    );
    capped.deployment.spec.capacity.rpm = Some(1);
    let snap = snapshot(
        vec![
            group("capped", quick(), &[], vec![capped]),
            group(
                "limited",
                json!({"retry": {"maxAttempts": 1, "maxRetryAfterMs": 100},
                       "circuitBreaker": {"enabled": true, "minRequests": 100}}),
                &[],
                vec![dep(
                    "limited",
                    ProviderKind::OpenAiCompatible,
                    &base,
                    "limited",
                    json!({}),
                )],
            ),
            group(
                "broken",
                json!({"retry": {"maxAttempts": 1},
                       "circuitBreaker": {"enabled": true, "minRequests": 1, "failureRate": 0.5,
                                          "openSecs": 60}}),
                &[],
                vec![dep(
                    "broken",
                    ProviderKind::OpenAiCompatible,
                    &base,
                    "fail",
                    json!({}),
                )],
            ),
        ],
        vec![key("all", &[], &[])],
    );
    let gw = gateway(snap).await;

    // Kapazität erschöpft → 429 (F83).
    assert_eq!(gw.chat("sk-all", msg("capped")).await.status, 200);
    let r = gw.chat("sk-all", msg("capped")).await;
    assert_eq!(r.status, 429, "{}", r.body);
    assert_eq!(r.json()["error"]["code"], "deployments_rate_limited");
    assert!(r.header("retry-after").unwrap().parse::<u64>().unwrap() >= 1);

    // Retry-After des Anbieters: erst dessen 429, danach gesperrt → eigenes 429.
    let r = gw.chat("sk-all", msg("limited")).await;
    assert_eq!(r.status, 429, "{}", r.body);
    let r = gw.chat("sk-all", msg("limited")).await;
    assert_eq!(r.status, 429, "{}", r.body);
    assert_eq!(r.json()["error"]["code"], "deployments_rate_limited");
    let secs: u64 = r.header("retry-after").unwrap().parse().unwrap();
    assert!((20..=30).contains(&secs), "{secs}");

    // Offener Circuit Breaker bleibt 503.
    assert!(
        gw.chat("sk-all", msg("broken"))
            .await
            .status
            .is_server_error()
    );
    let r = gw.chat("sk-all", msg("broken")).await;
    assert_eq!(r.status, 503, "{}", r.body);
    assert_eq!(r.json()["error"]["code"], "no_deployment_available");
}

// ---------------------------------------------------------------------------------------------
// F49 / F60 / F10
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn response_model_and_model_header() {
    let (base, _) = start_fake().await;
    let oc = |g: &str| dep(g, ProviderKind::OpenAiCompatible, &base, "echo", json!({}));
    let snap = snapshot(
        vec![
            group("up", quick(), &[], vec![oc("up")]),
            group(
                "req",
                json!({"retry": {"maxAttempts": 1}, "responseModel": "requested"}),
                &[],
                vec![oc("req")],
            ),
        ],
        vec![key("all", &[], &[])],
    );
    let gw = gateway(snap).await;
    let r = gw.chat("sk-all", msg("up")).await;
    assert_eq!(r.json()["model"], "echo-2024");
    assert_eq!(r.header("x-aigw-model"), Some("echo-2024"));
    let r = gw.chat("sk-all", msg("req-alias")).await;
    assert_eq!(r.json()["model"], "req-alias");
    assert_eq!(r.header("x-aigw-model"), Some("echo-2024"));
    // Stream: Modell im ersten Chunk.
    let mut body = msg("req");
    body["stream"] = json!(true);
    let r = gw.chat("sk-all", body).await;
    assert_eq!(r.header("x-aigw-model"), Some("echo-2024"));
    assert!(r.frames().iter().all(|f| f["model"] == "req"), "{}", r.body);
    // Anthropic-Eingang: ebenfalls umgeschrieben.
    let r = gw
        .post(
            "/v1/messages",
            "sk-all",
            json!({"model": "req", "max_tokens": 10,
                   "messages": [{"role": "user", "content": "Hallo"}]}),
            &[],
        )
        .await;
    assert_eq!(r.json()["model"], "req");
}

#[tokio::test]
async fn client_betas_follow_group_policy() {
    let (base, fake) = start_fake().await;
    let an = |g: &str| dep(g, ProviderKind::Anthropic, &base, "claude-x", json!({}));
    let snap = snapshot(
        vec![
            group("all", quick(), &[], vec![an("all")]),
            group(
                "list",
                json!({"retry": {"maxAttempts": 1}, "clientBetas": "allowlist",
                       "betaAllowlist": ["beta-ok"]}),
                &[],
                vec![an("list")],
            ),
            group(
                "none",
                json!({"retry": {"maxAttempts": 1}, "clientBetas": "deny"}),
                &[],
                vec![an("none")],
            ),
        ],
        vec![key("all", &[], &[])],
    );
    let gw = gateway(snap).await;
    for model in ["all", "list", "none"] {
        let r = gw
            .post(
                "/v1/messages",
                "sk-all",
                json!({"model": model, "max_tokens": 10,
                       "messages": [{"role": "user", "content": "Hallo"}]}),
                &[("anthropic-beta", "beta-ok,beta-evil")],
            )
            .await;
        assert_eq!(r.status, 200, "{model}: {}", r.body);
    }
    let betas = fake.betas.lock().unwrap().clone();
    assert_eq!(
        betas,
        [
            Some("beta-ok,beta-evil".to_owned()),
            Some("beta-ok".to_owned()),
            None
        ]
    );
}

/// F30: Payload-Mitschnitt je Key mit Vererbung (Key → Team → Organisation → global aus);
/// F47: feine Fehlerklasse im Datensatz.
#[tokio::test]
async fn payload_mode_per_key_and_error_class() {
    use aigw_core::observe::PayloadMode;
    let (base, _) = start_fake().await;
    let snap = snapshot(
        vec![group(
            "g",
            quick(),
            &[],
            vec![dep(
                "g",
                ProviderKind::OpenAiCompatible,
                &base,
                "echo",
                json!({}),
            )],
        )],
        vec![
            key("plain", &[], &[]),
            {
                let (s, mut k) = key("orgfull", &[], &[]);
                k.organization = Some(Arc::new(r::Organization {
                    id: new_id(),
                    metadata: ObjectMeta::named("o"),
                    spec: r::OrganizationSpec {
                        payload_mode: Some(PayloadMode::Full),
                        ..r::OrganizationSpec::default()
                    },
                }));
                (s, k)
            },
            {
                let (s, mut k) = key("keyoff", &[], &[]);
                k.key.spec.payload_mode = Some(PayloadMode::Off);
                k.team = Some(Arc::new(r::Team {
                    id: new_id(),
                    metadata: ObjectMeta::named("t"),
                    spec: r::TeamSpec {
                        payload_mode: Some(PayloadMode::Metadata),
                        ..r::TeamSpec::default()
                    },
                }));
                (s, k)
            },
        ],
    );
    let gw = gateway(snap).await;
    gw.chat("sk-plain", msg("g")).await;
    assert!(gw.capture.last().await.payload.is_none());
    gw.capture.0.lock().unwrap().clear();
    gw.chat("sk-orgfull", msg("g")).await;
    let rec = gw.capture.last().await;
    let p = rec.payload.expect("Organisation: full");
    assert_eq!(p.mode, PayloadMode::Full);
    assert!(!p.request_body.is_empty() && !p.response_body.is_empty());
    gw.capture.0.lock().unwrap().clear();
    gw.chat("sk-keyoff", msg("g")).await;
    assert!(
        gw.capture.last().await.payload.is_none(),
        "Key überschreibt Team"
    );
    gw.capture.0.lock().unwrap().clear();
    // F47: unbekanntes Modell → Fehlerklasse im Datensatz
    let r = gw.chat("sk-plain", msg("gibt-es-nicht")).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let rec = gw.capture.last().await;
    assert_eq!(rec.error_class, Some(aigw_core::ir::ErrorClass::NotFound));
}

#[tokio::test]
async fn reasoning_reaches_openai_clients_unless_disabled() {
    let (base, _) = start_fake().await;
    // vLLM liefert Reasoning als `reasoning_content` (→ IR-Reasoning).
    let vl = |g: &str| dep(g, ProviderKind::Vllm, &base, "echo", json!({}));
    let snap = snapshot(
        vec![
            group("on", quick(), &[], vec![vl("on")]),
            group(
                "off",
                json!({"retry": {"maxAttempts": 1}, "exposeReasoning": false}),
                &[],
                vec![vl("off")],
            ),
        ],
        vec![
            key("all", &[], &[]),
            // F185: Spec-Feld statt Metadatum, mit Vererbung von der Organisation.
            {
                let (s, mut k) = key("quiet", &[], &[]);
                k.key.spec.expose_reasoning = Some(false);
                (s, k)
            },
            {
                let (s, mut k) = key("orgquiet", &[], &[("aigw.exposeReasoning", "true")]);
                k.organization = Some(Arc::new(r::Organization {
                    id: new_id(),
                    metadata: ObjectMeta::named("o"),
                    spec: r::OrganizationSpec {
                        expose_reasoning: Some(false),
                        ..r::OrganizationSpec::default()
                    },
                }));
                (s, k)
            },
        ],
    );
    let gw = gateway(snap).await;
    let r = gw.chat("sk-all", msg("on")).await;
    let m = &r.json()["choices"][0]["message"];
    assert_eq!(m["reasoning_content"], "nachgedacht", "{}", r.body);
    assert_eq!(m["content"], "v0 von echo");
    let r = gw.chat("sk-all", msg("off")).await;
    assert!(
        r.json()["choices"][0]["message"]
            .get("reasoning_content")
            .is_none()
    );
    for k in ["sk-quiet", "sk-orgquiet"] {
        let r = gw.chat(k, msg("on")).await;
        assert!(
            r.json()["choices"][0]["message"]
                .get("reasoning_content")
                .is_none(),
            "{k}"
        );
    }

    let mut body = msg("on");
    body["stream"] = json!(true);
    let r = gw.chat("sk-all", body.clone()).await;
    assert!(
        r.frames()
            .iter()
            .any(|f| f["choices"][0]["delta"]["reasoning_content"] == "nachgedacht"),
        "{}",
        r.body
    );
    body["model"] = json!("off");
    let r = gw.chat("sk-all", body).await;
    assert!(!r.body.contains("reasoning_content"), "{}", r.body);
}

// ---------------------------------------------------------------------------------------------
// F12: n > 1
// ---------------------------------------------------------------------------------------------

fn indices(frames: &[Value]) -> Vec<u64> {
    frames
        .iter()
        .filter_map(|f| f["choices"].get(0))
        .filter(|c| {
            c["delta"]["content"]
                .as_str()
                .is_some_and(|t| !t.is_empty())
        })
        .map(|c| c["index"].as_u64().unwrap())
        .collect()
}

#[tokio::test]
async fn n_choices_native_and_fanned_out() {
    let (base, fake) = start_fake().await;
    let snap = snapshot(
        vec![
            // vLLM kann `n` selbst.
            group(
                "native",
                quick(),
                &[],
                vec![dep("native", ProviderKind::Vllm, &base, "nat", json!({}))],
            ),
            // Generisch kompatibel: parallele Anfragen.
            group(
                "fan",
                quick(),
                &[],
                vec![dep(
                    "fan",
                    ProviderKind::OpenAiCompatible,
                    &base,
                    "fan",
                    json!({}),
                )],
            ),
            // Eingebauter Mock (ohne `n`).
            group(
                "mock",
                quick(),
                &[],
                vec![dep("mock", ProviderKind::Mock, &base, "mock", json!({}))],
            ),
        ],
        vec![key("all", &[], &[])],
    );
    let gw = gateway(snap).await;
    let with_n = |model: &str, n: u64, stream: bool| {
        let mut b = msg(model);
        b["n"] = json!(n);
        b["stream"] = json!(stream);
        b["stream_options"] = json!({"include_usage": true});
        b
    };

    // Nativ: eine Anfrage mit n=3.
    let r = gw.chat("sk-all", with_n("native", 3, false)).await;
    let v = r.json();
    assert_eq!(v["choices"].as_array().unwrap().len(), 3, "{}", r.body);
    assert_eq!(v["choices"][2]["index"], 2);
    assert_eq!(v["usage"]["completion_tokens"], 6);
    assert_eq!(fake.hits().iter().filter(|(m, _)| m == "nat").count(), 1);
    assert!(fake.hits().iter().any(|(m, n)| m == "nat" && *n == Some(3)));

    // Parallel: drei Anfragen ohne `n`, Usage summiert.
    let r = gw.chat("sk-all", with_n("fan", 3, false)).await;
    let v = r.json();
    let idx: Vec<u64> = v["choices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["index"].as_u64().unwrap())
        .collect();
    assert_eq!(idx, [0, 1, 2], "{}", r.body);
    assert_eq!(v["usage"]["prompt_tokens"], 9);
    assert_eq!(v["usage"]["completion_tokens"], 6);
    let fan: Vec<_> = fake
        .hits()
        .into_iter()
        .filter(|(m, _)| m == "fan")
        .collect();
    assert_eq!(fan.len(), 3);
    assert!(fan.iter().all(|(_, n)| n.is_none()));
    let rec = gw.capture.last().await;
    assert_eq!(rec.usage.input_tokens, 9);

    // Gestreamt, nativ und parallel: jede Variante mit eigenem index, Usage summiert.
    for model in ["native", "fan"] {
        let r = gw.chat("sk-all", with_n(model, 2, true)).await;
        assert_eq!(r.status, 200, "{}", r.body);
        let frames = r.frames();
        let mut idx = indices(&frames);
        idx.sort_unstable();
        assert_eq!(idx, [0, 1], "{model}: {}", r.body);
        let finishes = frames
            .iter()
            .filter(|f| f["choices"][0]["finish_reason"] == "stop")
            .count();
        assert_eq!(finishes, 2, "{model}: {}", r.body);
        let usage = frames.iter().find(|f| f["usage"].is_object()).unwrap();
        assert_eq!(usage["usage"]["completion_tokens"], 4, "{model}");
    }

    // Mock (ohne `n`) gestreamt.
    let r = gw.chat("sk-all", with_n("mock", 2, true)).await;
    let mut idx = indices(&r.frames());
    idx.dedup();
    idx.sort_unstable();
    idx.dedup();
    assert_eq!(idx, [0, 1], "{}", r.body);

    // Grenzen.
    assert_eq!(gw.chat("sk-all", with_n("fan", 0, false)).await.status, 400);
    assert_eq!(
        gw.chat("sk-all", with_n("fan", 17, false)).await.status,
        400
    );
}

#[tokio::test]
async fn n_choices_in_responses_api() {
    let (base, _) = start_fake().await;
    let snap = snapshot(
        vec![group(
            "fan",
            quick(),
            &[],
            vec![dep(
                "fan",
                ProviderKind::OpenAiCompatible,
                &base,
                "fan",
                json!({}),
            )],
        )],
        vec![key("all", &[], &[])],
    );
    let gw = gateway(snap).await;
    let r = gw
        .post(
            "/v1/responses",
            "sk-all",
            json!({"model": "fan", "input": "Hallo", "n": 2}),
            &[],
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let msgs: Vec<Value> = r.json()["output"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["type"] == "message")
        .cloned()
        .collect();
    assert_eq!(msgs.len(), 2, "{}", r.body);

    // Gestreamt: Varianten nacheinander, jede als eigenes Item.
    let r = gw
        .post(
            "/v1/responses",
            "sk-all",
            json!({"model": "fan", "input": "Hallo", "n": 2, "stream": true}),
            &[],
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let done: Vec<Value> = r
        .frames()
        .into_iter()
        .filter(|f| f["type"] == "response.output_item.done" && f["item"]["type"] == "message")
        .collect();
    assert_eq!(done.len(), 2, "{}", r.body);
    let completed = r
        .frames()
        .into_iter()
        .find(|f| f["type"] == "response.completed")
        .unwrap();
    assert_eq!(completed["response"]["usage"]["output_tokens"], 4);
}

// ---------------------------------------------------------------------------------------------
// F44 / F98 / F85 / F11
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn group_timeouts_apply_to_deployments() {
    let (base, _) = start_fake().await;
    let snap = snapshot(
        vec![group(
            "slowgroup",
            json!({"retry": {"maxAttempts": 1}, "circuitBreaker": {"enabled": false},
                   "timeouts": {"totalMs": 200}}),
            &[],
            vec![dep(
                "slowgroup",
                ProviderKind::OpenAiCompatible,
                &base,
                "slow",
                json!({}),
            )],
        )],
        vec![key("all", &[], &[])],
    );
    let gw = gateway(snap).await;
    let t = std::time::Instant::now();
    let r = gw.chat("sk-all", msg("slowgroup")).await;
    assert_eq!(r.status, 504, "{}", r.body);
    assert!(t.elapsed() < Duration::from_millis(1200));
}

#[tokio::test]
async fn trace_propagation_defaults_per_provider_kind() {
    use aigw_providers::default_propagation;
    assert!(default_propagation(ProviderKind::Vllm));
    assert!(default_propagation(ProviderKind::Ollama));
    assert!(default_propagation(ProviderKind::OpenAiCompatible));
    for k in [
        ProviderKind::OpenAi,
        ProviderKind::AzureOpenAi,
        ProviderKind::Anthropic,
        ProviderKind::Bedrock,
        ProviderKind::Vertex,
        ProviderKind::Mistral,
    ] {
        assert!(!default_propagation(k), "{k:?}");
    }
    let client = UpstreamClient::unguarded().unwrap();
    let reg = Registry::with_builtin();
    let build = |kind, settings| {
        Upstream::from_deployment(
            &dep("g", kind, "http://127.0.0.1:9", "m", settings),
            &reg,
            &client,
        )
        .unwrap()
    };
    assert!(build(ProviderKind::Vllm, json!({})).propagates_trace());
    assert!(!build(ProviderKind::Vllm, json!({"propagateTraceContext": false})).propagates_trace());
    assert!(!build(ProviderKind::Anthropic, json!({})).propagates_trace());
    assert!(
        build(
            ProviderKind::Anthropic,
            json!({"propagateTraceContext": true})
        )
        .propagates_trace()
    );
    // Von der Modellgruppe geerbt.
    let group_settings = obj(json!({"propagateTraceContext": true, "timeouts": {"totalMs": 1234}}));
    let d = dep(
        "g",
        ProviderKind::Anthropic,
        "http://127.0.0.1:9",
        "m",
        json!({}),
    );
    let eff = aigw_providers::with_group_defaults(&group_settings, &d);
    let up = Upstream::from_deployment(&eff, &reg, &client).unwrap();
    assert!(up.propagates_trace());
    assert_eq!(up.timeouts().total, Duration::from_millis(1234));
}

#[tokio::test]
async fn mock_provider_can_be_disabled() {
    let (base, _) = start_fake().await;
    let snap = || {
        snapshot(
            vec![group(
                "demo",
                quick(),
                &[],
                vec![dep("demo", ProviderKind::Mock, &base, "mock", json!({}))],
            )],
            vec![key("all", &[], &[])],
        )
    };
    let gw = gateway_with(snap(), true).await;
    assert_eq!(gw.chat("sk-all", msg("demo")).await.status, 200);
    let gw = gateway_with(snap(), false).await;
    let r = gw.chat("sk-all", msg("demo")).await;
    assert_eq!(r.status, 503, "{}", r.body);
}

#[tokio::test]
async fn client_request_id_is_recorded() {
    let (base, _) = start_fake().await;
    let snap = snapshot(
        vec![group(
            "a",
            quick(),
            &[],
            vec![dep(
                "a",
                ProviderKind::OpenAiCompatible,
                &base,
                "echo",
                json!({}),
            )],
        )],
        vec![key("all", &[], &[])],
    );
    let gw = gateway(snap).await;
    gw.post(
        "/v1/chat/completions",
        "sk-all",
        msg("a"),
        &[
            ("x-request-id", "req-1"),
            ("x-client-request-id", "client-42"),
        ],
    )
    .await;
    assert_eq!(
        gw.capture.last().await.client_request_id.as_deref(),
        Some("client-42")
    );
    gw.capture.0.lock().unwrap().clear();
    gw.post(
        "/v1/chat/completions",
        "sk-all",
        msg("a"),
        &[("x-request-id", "req-7")],
    )
    .await;
    assert_eq!(
        gw.capture.last().await.client_request_id.as_deref(),
        Some("req-7")
    );
}
