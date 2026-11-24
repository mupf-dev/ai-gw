// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Mehrere Anbieter-Konten rotieren (AIGW-125, AIGW-126) gegen einen Fake-Anthropic-Upstream:
//!
//! - Konto meldet „credit balance is too low“ → `Quota`, die Anfrage landet ohne sichtbaren
//!   Fehler beim nächsten Deployment, der Breaker meidet das leere Konto danach.
//! - Budget je Deployment: 4 Konten (priority 0..3) werden nacheinander bis zum Budget genutzt;
//!   sind alle erschöpft, kommt `429 deployment_budget_exhausted`; Nachladen (Betrag erhöhen,
//!   Hot Reload) macht ein Konto wieder nutzbar.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use aigw_billing::{Budgets, Calendar};
use aigw_core::resource::{
    CredentialData, Deployment, DeploymentBudget, DeploymentSpec, JsonObject, ModelGroup,
    ModelGroupSpec, ObjectMeta, Price, PriceSpec, RoutingStrategy, SecretString, new_id,
};
use aigw_core::snapshot::{DeploymentCtx, ModelGroupCtx, Snapshot};
use aigw_core::types::ProviderKind;
use aigw_providers::{Registry, UpstreamClient};
use aigw_proxy::routing::BreakerState;
use aigw_proxy::{ModelRoutes, ProxyState, SnapshotRoutes};
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------------------------
// Fake-Anthropic: je Modell (= Konto) „ok“ oder „Guthaben leer“.
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Fake {
    empty: Mutex<Vec<String>>,
    hits: Mutex<Vec<String>>,
}

impl Fake {
    fn set_empty(&self, model: &str) {
        self.empty.lock().unwrap().push(model.to_owned());
    }

    fn count(&self, model: &str) -> usize {
        self.hits
            .lock()
            .unwrap()
            .iter()
            .filter(|h| *h == model)
            .count()
    }
}

async fn messages(State(fake): State<Arc<Fake>>, body: Bytes) -> Response {
    let body: Value = serde_json::from_slice(&body).unwrap();
    let model = body["model"].as_str().unwrap_or_default().to_owned();
    fake.hits.lock().unwrap().push(model.clone());
    if fake.empty.lock().unwrap().contains(&model) {
        return (
            StatusCode::BAD_REQUEST,
            [("content-type", "application/json")],
            json!({"type": "error", "error": {"type": "invalid_request_error",
                   "message": "Your credit balance is too low to access the Anthropic API. \
                               Please go to Plans & Billing to upgrade or purchase credits."},
                   "request_id": "req_1"})
            .to_string(),
        )
            .into_response();
    }
    (
        [("content-type", "application/json")],
        json!({"id": "msg_1", "type": "message", "role": "assistant", "model": model,
               "content": [{"type": "text", "text": format!("von {model}")}],
               "stop_reason": "end_turn", "stop_sequence": null,
               "usage": {"input_tokens": 10, "output_tokens": 20}})
        .to_string(),
    )
        .into_response()
}

async fn start_fake() -> (String, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    let app = Router::new()
        .route("/v1/messages", post(messages))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), fake)
}

// ---------------------------------------------------------------------------------------------
// Konfiguration
// ---------------------------------------------------------------------------------------------

/// Ein Konto: Deployment `name` (= Anbieter-Modell im Fake) mit eigenem Schlüssel.
fn account(
    name: &str,
    base: &str,
    priority: u32,
    budget: Option<Value>,
    id: aigw_core::resource::Id,
) -> DeploymentCtx {
    DeploymentCtx {
        deployment: Deployment {
            id,
            metadata: ObjectMeta::named(name),
            spec: DeploymentSpec {
                model_group: "claude".into(),
                provider: ProviderKind::Anthropic,
                endpoint: Some(base.into()),
                provider_model: name.into(),
                credential: Some(format!("{name}-key")),
                weight: 1,
                priority,
                capacity: Default::default(),
                enabled: true,
                settings: JsonObject::new(),
                budget: budget.map(|b| serde_json::from_value::<DeploymentBudget>(b).unwrap()),
            },
        },
        credential: Some(Arc::new(CredentialData(BTreeMap::from([(
            "apiKey".to_owned(),
            SecretString::new(format!("sk-ant-{name}")),
        )])))),
    }
}

fn snapshot(version: i64, routing: RoutingStrategy, deps: Vec<DeploymentCtx>) -> Snapshot {
    let g = Arc::new(ModelGroupCtx {
        group: ModelGroup {
            id: new_id(),
            metadata: ObjectMeta::named("claude"),
            spec: ModelGroupSpec {
                aliases: Vec::new(),
                capabilities: Vec::new(),
                routing,
                fallbacks: Vec::new(),
                limits: Vec::new(),
                settings: JsonObject::new(),
            },
        },
        deployments: deps,
    });
    let mut s = Snapshot {
        version,
        ..Snapshot::default()
    };
    s.model_groups.insert("claude".into(), g.clone());
    // Preis je Konto: 1 000 je 1 Mio. Tokens → 10 + 20 Tokens = 0,03 je Anfrage.
    for d in &g.deployments {
        let model = d.deployment.spec.provider_model.clone();
        let spec: PriceSpec = serde_json::from_value(json!({
            "provider": "anthropic", "model": model, "validFrom": "2026-01-01T00:00:00Z",
            "perMillionTokens": {"input": "1000", "output": "1000"}
        }))
        .unwrap();
        s.prices
            .entry((Some(ProviderKind::Anthropic), model.clone()))
            .or_default()
            .push(Arc::new(Price {
                id: new_id(),
                metadata: ObjectMeta::named(format!("price-{model}")),
                spec,
            }));
    }
    s
}

struct Env {
    gw: SocketAddr,
    routes: Arc<SnapshotRoutes>,
}

async fn gateway(snap: &Snapshot) -> Env {
    let budgets = Arc::new(Budgets::new(Calendar::utc(), None));
    let routes = Arc::new(
        SnapshotRoutes::new(
            Registry::with_builtin(),
            UpstreamClient::unguarded().unwrap(),
        )
        .with_budgets(budgets),
    );
    routes.rebuild(snap);
    let state = ProxyState::with_routes(routes.clone() as Arc<dyn ModelRoutes>);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, aigw_proxy::router(state))
            .await
            .unwrap();
    });
    Env { gw, routes }
}

struct Reply {
    status: StatusCode,
    deployment: String,
    body: Value,
}

impl Env {
    /// Anfrage im Anthropic-Format (wie Claude-Clients), `max_tokens` klein (Reservierung).
    async fn ask(&self) -> Reply {
        let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
        let body = json!({"model": "claude", "max_tokens": 20,
                          "messages": [{"role": "user", "content": "Hallo"}]});
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("http://{}/v1/messages", self.gw))
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(body.to_string())))
            .unwrap();
        let res = client.request(req).await.unwrap();
        let status = res.status();
        let deployment = res
            .headers()
            .get("x-aigw-deployment")
            .map_or_else(String::new, |v| v.to_str().unwrap().to_owned());
        let body = res.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            deployment,
            body: serde_json::from_slice(&body).unwrap_or(Value::Null),
        }
    }

    fn breaker(&self, deployment: &str) -> BreakerState {
        self.routes
            .deployment_status()
            .into_iter()
            .find(|d| d.deployment == deployment)
            .unwrap()
            .breaker
    }

    fn budget_used(&self) -> HashMap<String, i64> {
        self.routes
            .deployment_status()
            .into_iter()
            .filter_map(|d| d.budget.map(|(u, _)| (d.deployment, u)))
            .collect()
    }
}

// ---------------------------------------------------------------------------------------------
// AIGW-125
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn empty_credit_balance_falls_back_without_visible_error() {
    let (base, fake) = start_fake().await;
    fake.set_empty("acct-0");
    let snap = snapshot(
        1,
        RoutingStrategy::Weighted,
        vec![
            account("acct-0", &base, 0, None, new_id()),
            account("acct-1", &base, 1, None, new_id()),
        ],
    );
    let env = gateway(&snap).await;

    let r = env.ask().await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.deployment, "acct-1");
    assert_eq!(r.body["content"][0]["text"], "von acct-1");
    assert_eq!((fake.count("acct-0"), fake.count("acct-1")), (1, 1));
    // Leeres Konto: Breaker sofort offen, weitere Anfragen gehen direkt an acct-1.
    assert_eq!(env.breaker("acct-0"), BreakerState::Open);
    for _ in 0..5 {
        let r = env.ask().await;
        assert_eq!(
            (r.status, r.deployment.as_str()),
            (StatusCode::OK, "acct-1")
        );
    }
    assert_eq!(fake.count("acct-0"), 1, "leeres Konto wird gemieden");
}

#[tokio::test]
async fn all_accounts_empty_reports_quota_in_ingress_format() {
    let (base, fake) = start_fake().await;
    fake.set_empty("acct-0");
    fake.set_empty("acct-1");
    let snap = snapshot(
        1,
        RoutingStrategy::Weighted,
        vec![
            account("acct-0", &base, 0, None, new_id()),
            account("acct-1", &base, 1, None, new_id()),
        ],
    );
    let env = gateway(&snap).await;
    let r = env.ask().await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS, "{}", r.body);
    assert_eq!(r.body["type"], "error");
    assert_eq!(r.body["error"]["type"], "rate_limit_error");
}

// ---------------------------------------------------------------------------------------------
// AIGW-126
// ---------------------------------------------------------------------------------------------

fn budget(amount: &str) -> Value {
    json!({"amount": amount, "period": "total", "warnAtPercent": [80]})
}

#[tokio::test]
async fn four_accounts_rotate_by_budget_and_reload() {
    let (base, fake) = start_fake().await;
    let ids: Vec<_> = (0..4).map(|_| new_id()).collect();
    let deps = |amount0: &str| {
        (0..4)
            .map(|i| {
                account(
                    &format!("acct-{i}"),
                    &base,
                    u32::try_from(i).unwrap(),
                    Some(budget(if i == 0 { amount0 } else { "0.09" })),
                    ids[i],
                )
            })
            .collect::<Vec<_>>()
    };
    let env = gateway(&snapshot(1, RoutingStrategy::Weighted, deps("0.09"))).await;

    // Je Konto 3 Anfragen à 0,03 (= 0,09), dann das nächste – ohne Fehler für den Client.
    let mut order = Vec::new();
    for _ in 0..12 {
        let r = env.ask().await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
        order.push(r.deployment);
    }
    let expect: Vec<String> = (0..4)
        .flat_map(|i| std::iter::repeat_n(format!("acct-{i}"), 3))
        .collect();
    assert_eq!(order, expect);
    for i in 0..4 {
        assert_eq!(fake.count(&format!("acct-{i}")), 3);
    }
    let used = env.budget_used();
    assert!(used.values().all(|u| *u == 90_000_000), "{used:?}");

    // Alle erschöpft: 429 im Eingangsformat, kein Upstream-Aufruf.
    let r = env.ask().await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS, "{}", r.body);
    assert_eq!(r.body["error"]["type"], "rate_limit_error");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Budget aller Deployments erschöpft"),
        "{}",
        r.body
    );
    assert_eq!(fake.hits.lock().unwrap().len(), 12);

    // Nachladen: Betrag von acct-0 erhöhen (Hot Reload) → wieder nutzbar, Stand bleibt.
    env.routes
        .rebuild(&snapshot(2, RoutingStrategy::Weighted, deps("0.15")));
    for _ in 0..2 {
        let r = env.ask().await;
        assert_eq!(
            (r.status, r.deployment.as_str()),
            (StatusCode::OK, "acct-0")
        );
    }
    assert_eq!(env.ask().await.status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn budget_balanced_spreads_requests_over_accounts() {
    let (base, fake) = start_fake().await;
    let deps = (0..4)
        .map(|i| {
            account(
                &format!("acct-{i}"),
                &base,
                0,
                Some(budget("100")),
                new_id(),
            )
        })
        .collect();
    let env = gateway(&snapshot(1, RoutingStrategy::BudgetBalanced, deps)).await;
    for _ in 0..200 {
        assert_eq!(env.ask().await.status, StatusCode::OK);
    }
    for i in 0..4 {
        let n = fake.count(&format!("acct-{i}"));
        assert!((45..=55).contains(&n), "acct-{i}: {n}");
    }
}
