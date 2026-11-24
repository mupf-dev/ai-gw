// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Budget je Deployment über die Admin-API (AIGW-126): Anlegen, Validierung, Nachladen per
//! höherem Betrag, Entfernen, `GET /usage/budgets?level=deployment` und GitOps-Prüfung.
//! SQLite und – mit `AIGW_TEST_POSTGRES_URL` – Postgres.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]

use std::sync::Arc;

use aigw_admin::AdminState;
use aigw_admin::usage::{UsageState, router as usage_router};
use aigw_billing::{BudgetKey, BudgetLevel};
use aigw_core::config::{AdminAuthConfig, CounterBackendKind};
use aigw_core::resource::{BudgetPeriod, DeploymentSpec, new_id};
use aigw_store::Store;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};
use sqlx::AssertSqlSafe;

struct Env {
    addr: std::net::SocketAddr,
    token: String,
}

impl Env {
    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
        let write = matches!(method, Method::PUT | Method::PATCH | Method::DELETE);
        let mut req = Request::builder()
            .method(method)
            .uri(format!("http://{}/admin/api/v1{path}", self.addr))
            .header("authorization", format!("Bearer {}", self.token));
        if write {
            // F38: If-Match ist Pflicht.
            req = req.header("if-match", "*");
        }
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Bytes::from(b.to_string())
            }
            None => Bytes::new(),
        };
        let res = client
            .request(req.body(Full::new(body)).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let body = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }
}

async fn start(store: Store) -> Env {
    store.migrate().await.unwrap();
    let (token, secret) = aigw_admin::BootstrapToken::generate().unwrap();
    let state = AdminState::from_config(
        store.clone(),
        &AdminAuthConfig::default(),
        Some(token),
        None,
    )
    .unwrap();
    let usage = usage_router(UsageState {
        store: store.clone(),
        calendar: Arc::new(aigw_billing::Calendar::utc()),
        auth: Arc::clone(&state.auth),
        counters: CounterBackendKind::Local,
    });
    let app = aigw_admin::router_with(state).merge(usage);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Env {
        addr,
        token: secret,
    }
}

fn dep(name: &str, priority: u32, budget: Value) -> Value {
    json!({"metadata": {"name": name},
           "spec": {"modelGroup": "claude", "provider": "anthropic",
                    "providerModel": "claude-sonnet-5", "priority": priority,
                    "budget": budget}})
}

async fn scenario(store: Store) {
    let env = start(store.clone()).await;
    let (s, v) = env
        .call(
            Method::POST,
            "/model-groups",
            Some(json!({"metadata": {"name": "claude"}, "spec": {"routing": "budget-balanced"}})),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["spec"]["routing"], "budget-balanced");

    // Vier Konten, priority 0..3, je 200 Budget.
    for i in 0..4u32 {
        let (s, v) = env
            .call(
                Method::POST,
                "/deployments",
                Some(dep(
                    &format!("acct-{i}"),
                    i,
                    json!({"amount": "200", "period": "total", "warnAtPercent": [80, 95],
                           "switchAtPercent": 98}),
                )),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        assert_eq!(v["spec"]["budget"]["amount"], "200");
        assert_eq!(v["spec"]["budget"]["switchAtPercent"], 98);
    }

    // Ungültige Budgets.
    for (budget, hint) in [
        (json!({"amount": "0", "period": "total"}), "budget.amount"),
        (
            json!({"amount": "5", "period": "month", "switchAtPercent": 0}),
            "budget.switchAtPercent",
        ),
        (
            json!({"amount": "5", "period": "month", "warnAtPercent": [150]}),
            "budget.warnAtPercent",
        ),
    ] {
        let (s, v) = env
            .call(Method::POST, "/deployments", Some(dep("bad", 0, budget)))
            .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
        assert!(v.to_string().contains(hint), "{hint}: {v}");
    }
    let (s, _) = env
        .call(
            Method::POST,
            "/deployments",
            Some(dep(
                "bad",
                0,
                json!({"amount": "5", "period": "total", "hardLimit": false}),
            )),
        )
        .await;
    assert!(s.is_client_error(), "unbekanntes Feld abgelehnt: {s}");

    // Verbrauch im Ledger (wie vom Gateway geschrieben) erscheint in der Übersicht.
    let id = store.get::<DeploymentSpec>("acct-0").await.unwrap().id;
    let key = BudgetKey {
        level: BudgetLevel::Deployment,
        target: id,
        period: BudgetPeriod::Total,
        period_start_ms: 0,
    };
    store
        .sync_budget_usage(&[(key, 197_000_000_000)], &[], 1)
        .await
        .unwrap();
    let (s, v) = env
        .call(Method::GET, "/usage/budgets?level=deployment", None)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 4, "{v}");
    let a0 = items.iter().find(|i| i["targetName"] == "acct-0").unwrap();
    assert_eq!(a0["level"], "deployment");
    assert_eq!(a0["modelGroup"], "claude");
    assert_eq!(a0["switchAtPercent"], 98);
    assert_eq!(a0["used"], "197");
    assert_eq!(a0["percent"], 98);
    assert_eq!(a0["exhausted"], false);

    // Nachladen: Betrag erhöhen (Guthaben nachgekauft) – der Verbrauch bleibt stehen.
    let (_, mut cur) = env.call(Method::GET, "/deployments/acct-0", None).await;
    cur["spec"]["budget"]["amount"] = json!("400");
    let (s, v) = env
        .call(
            Method::PUT,
            "/deployments/acct-0",
            Some(json!({"metadata": {"name": cur["metadata"]["name"]}, "spec": cur["spec"]})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["spec"]["budget"]["amount"], "400", "{v}");
    assert_eq!(v["spec"]["budget"]["switchAtPercent"], 98);
    let (_, v) = env
        .call(Method::GET, "/usage/budgets?level=deployment", None)
        .await;
    let a0 = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["targetName"] == "acct-0")
        .unwrap()
        .clone();
    assert_eq!(
        (a0["amount"].clone(), a0["percent"].clone()),
        (json!("400"), json!(49))
    );

    // Budget entfernen.
    let (_, mut cur) = env.call(Method::GET, "/deployments/acct-3", None).await;
    cur["spec"]["budget"] = Value::Null;
    let (s, v) = env
        .call(
            Method::PUT,
            "/deployments/acct-3",
            Some(json!({"metadata": {"name": cur["metadata"]["name"]}, "spec": cur["spec"]})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(v["spec"].get("budget").is_none(), "{v}");
    let (_, v) = env.call(Method::GET, "/usage/budgets", None).await;
    let n = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["level"] == "deployment")
        .count();
    assert_eq!(n, 3);

    // GitOps: dieselben Regeln.
    use aigw_store::gitops::{self, ApplyOptions, GitopsError, ProcessSecrets};
    let yaml = "\
apiVersion: aigw/v1alpha1
kind: Deployment
metadata: {name: acct-9}
spec: {modelGroup: claude, provider: anthropic, providerModel: m, budget: {amount: '10', period: total, switchAtPercent: 101}}
";
    let docs = gitops::parse_documents(yaml).unwrap();
    let keyring = aigw_store::Keyring::new(aigw_store::MasterKey::generate().unwrap().0);
    let err = gitops::plan(
        &store,
        &keyring,
        &docs,
        &ApplyOptions {
            dry_run: true,
            ..ApplyOptions::default()
        },
        &ProcessSecrets,
    )
    .await
    .unwrap_err();
    let GitopsError::Invalid(issues) = err else {
        panic!("Validierungsfehler erwartet: {err}");
    };
    let text: Vec<String> = issues.iter().map(ToString::to_string).collect();
    assert!(text.join("\n").contains("switchAtPercent"), "{text:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite() {
    scenario(Store::connect("sqlite::memory:").await.unwrap()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres() {
    let Ok(base) = std::env::var("AIGW_TEST_POSTGRES_URL") else {
        eprintln!("AIGW_TEST_POSTGRES_URL nicht gesetzt – Postgres-Test übersprungen");
        return;
    };
    let db = format!("aigw_dep_budget_test_{}", new_id().simple());
    let admin = sqlx::PgPool::connect(&base).await.unwrap();
    sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {db}")))
        .execute(&admin)
        .await
        .unwrap();
    let url = match base.rsplit_once('/') {
        Some((head, _)) => format!("{head}/{db}"),
        None => panic!("ungültige AIGW_TEST_POSTGRES_URL"),
    };
    scenario(Store::connect(&url).await.unwrap()).await;
    sqlx::query(AssertSqlSafe(format!("DROP DATABASE {db} WITH (FORCE)")))
        .execute(&admin)
        .await
        .unwrap();
}
