// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Preisvorschläge (AIGW-85) über die Admin-API gegen eine Fake-Retail-API (lokaler
//! axum-Server, Client ohne SSRF-Filter): Zuordnung Deployment → Meter je Region und
//! Bereitstellungsart, Folgeseiten, Zwischenspeicher, Katalog-Ersatz, Übernahme mit Labels und
//! Audit, Vorrang manueller Preise, Fehlertoleranz.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use aigw_admin::{AdminAuthenticator, AdminState, BootstrapToken, router_with};
use aigw_billing::suggest::Suggester;
use aigw_core::config::PriceSuggestionsConfig;
use aigw_core::resource::{DeploymentSpec, ModelGroupSpec, ObjectMeta, PriceSpec};
use aigw_outbound::{AddressGuard, OutboundClient};
use aigw_store::Store;
use axum::Router;
use axum::body::Body;
use axum::extract::{Query, State};
use http_body_util::BodyExt as _;
use hyper::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt as _;

const TOKEN: &str = "test-token-0123456789-0123456789-abcdef";

#[derive(Clone)]
struct Fake {
    calls: Arc<AtomicUsize>,
    fail: bool,
    base: String,
    last_query: Arc<std::sync::Mutex<String>>,
}

fn item(sku: &str, unit: &str, price: f64) -> Value {
    json!({
        "currencyCode": "USD", "retailPrice": price, "unitPrice": price,
        "armRegionName": "swedencentral", "location": "SE Central",
        "effectiveStartDate": "2026-03-01T00:00:00Z", "productName": "Azure OpenAI GPT5",
        "skuName": sku, "serviceName": "Foundry Models",
        "meterName": format!("{sku} {} Tokens", if unit == "1M" { "1M" } else { "" }).replace("  ", " "),
        "unitOfMeasure": unit, "type": "Consumption", "isPrimaryMeterRegion": true
    })
}

async fn retail(
    State(f): State<Fake>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    f.calls.fetch_add(1, Ordering::SeqCst);
    if f.fail {
        return (StatusCode::INTERNAL_SERVER_ERROR, "kaputt").into_response();
    }
    if q.contains_key("$skip") {
        return axum::Json(json!({
            "Items": [
                item("gpt-4o-mini-0718-Inp-regnl", "1K", 0.000165),
                item("gpt-4o-mini-0718-Outp-regnl", "1K", 0.00066),
            ],
            "NextPageLink": null, "Count": 2
        }))
        .into_response();
    }
    *f.last_query.lock().unwrap() = format!(
        "{}|{}",
        q.get("currencyCode").cloned().unwrap_or_default(),
        q.get("$filter").cloned().unwrap_or_default()
    );
    axum::Json(json!({
        "Items": [
            item("5.4 mini Inp Gl", "1M", 0.75),
            item("5.4 mini cd Inp Gl", "1M", 0.075),
            item("5.4 mini Opt Gl", "1M", 4.5),
            item("5.4 mini Inp Dz", "1M", 0.825),
            item("5.4 mini cd Inp Dz", "1M", 0.0825),
            item("5.4 mini Opt Dz", "1M", 4.95),
            item("5.4 mini Batch Inp Gl", "1M", 0.375),
            item("5.4 mini pp Inp Gl", "1M", 1.5),
        ],
        "NextPageLink": format!("{}/api/retail/prices?$skip=1000", f.base),
        "Count": 8
    }))
    .into_response()
}

async fn fake_retail(fail: bool) -> Fake {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let f = Fake {
        calls: Arc::new(AtomicUsize::new(0)),
        fail,
        base,
        last_query: Arc::default(),
    };
    let app = Router::new()
        .route("/api/retail/prices", axum::routing::get(retail))
        .with_state(f.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    f
}

async fn app(fake: &Fake) -> (Router, Store) {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    store.migrate().await.unwrap();
    let mut cfg = PriceSuggestionsConfig::default();
    cfg.azure.api_url = format!("{}/api/retail/prices", fake.base);
    // Tests gegen lokale Fake-Server: Client ohne SSRF-Filter.
    let client = OutboundClient::new(AddressGuard::Trusted).unwrap();
    let suggester = Arc::new(Suggester::from_config(&cfg, &client).unwrap());
    let auth: Arc<dyn AdminAuthenticator> = Arc::new(BootstrapToken::new(TOKEN).unwrap());
    let router =
        router_with(AdminState::new(store.clone(), auth).with_price_suggester(Some(suggester)));
    (router, store)
}

async fn call(app: &Router, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(format!("/admin/api/v1{path}"))
        .header("authorization", format!("Bearer {TOKEN}"));
    let body = match body {
        Some(b) => {
            req = req.header("content-type", "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let res = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let v = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, v)
}

async fn seed(store: &Store) {
    store
        .create(
            "test",
            ObjectMeta::named("g"),
            serde_json::from_value::<ModelGroupSpec>(json!({})).unwrap(),
        )
        .await
        .unwrap();
    let deps = [
        ("az-se", json!({"modelGroup": "g", "provider": "azure-openai",
            "endpoint": "https://res.openai.azure.com", "providerModel": "gpt54mini",
            "settings": {"pricing": {"region": "swedencentral", "model": "gpt-5.4-mini"}}})),
        ("az-dz", json!({"modelGroup": "g", "provider": "azure-openai",
            "endpoint": "https://res2.openai.azure.com", "providerModel": "gpt54mini-dz",
            "settings": {"pricing": {"region": "swedencentral", "model": "gpt-5.4-mini",
                                     "deploymentType": "data-zone"}}})),
        ("az-4o", json!({"modelGroup": "g", "provider": "azure-openai",
            "endpoint": "https://res.openai.azure.com", "providerModel": "gpt-4o-mini",
            "settings": {"pricing": {"region": "swedencentral", "deploymentType": "regional"}}})),
        ("oa", json!({"modelGroup": "g", "provider": "openai", "providerModel": "gpt-5.4-mini"})),
        ("claude", json!({"modelGroup": "g", "provider": "anthropic",
            "endpoint": "https://res.services.ai.azure.com/anthropic",
            "providerModel": "claude-sonnet-5-5",
            "settings": {"pricing": {"region": "swedencentral"}}})),
    ];
    for (name, spec) in deps {
        store
            .create(
                "test",
                ObjectMeta::named(name),
                serde_json::from_value::<DeploymentSpec>(spec).unwrap(),
            )
            .await
            .unwrap();
    }
    // Manuell gepflegter Preis (ohne Label) für das OpenAI-Deployment.
    store
        .create(
            "test",
            ObjectMeta::named("manual-oa"),
            serde_json::from_value::<PriceSpec>(json!({
                "provider": "openai", "model": "gpt-5.4-mini", "validFrom": "2026-01-01T00:00:00Z",
                "perMillionTokens": {"input": "1", "output": "4.5"}
            }))
            .unwrap(),
        )
        .await
        .unwrap();
}

fn by_dep<'a>(v: &'a Value, d: &str) -> &'a Value {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["deployment"] == d)
        .unwrap_or_else(|| panic!("{d} fehlt: {v}"))
}

#[tokio::test]
async fn suggestions_from_retail_api_and_catalog() {
    let fake = fake_retail(false).await;
    let (app, store) = app(&fake).await;
    seed(&store).await;

    let (s, v) = call(&app, Method::GET, "/price-suggestions", None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // Zwei Seiten (NextPageLink auf demselben Host), USD, Filter je Region.
    assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
    let q = fake.last_query.lock().unwrap().clone();
    assert!(q.starts_with("USD|"), "{q}");
    assert!(q.contains("serviceName eq 'Foundry Models'") && q.contains("armRegionName eq 'swedencentral'"), "{q}");

    let se = by_dep(&v, "az-se");
    assert_eq!(se["status"], "missing", "{se}");
    assert_eq!(se["source"], "azure-retail");
    assert_eq!(se["region"], "swedencentral");
    assert_eq!(se["deploymentType"], "global");
    assert_eq!(se["suggested"]["provider"], "azure-openai");
    assert_eq!(se["suggested"]["model"], "gpt54mini");
    assert_eq!(se["suggested"]["perMillionTokens"], json!({"input": "0.75", "cachedInput": "0.075", "output": "4.5"}));
    assert_eq!(se["suggested"]["validFrom"], "2026-03-01T00:00:00Z");
    assert!(se["notes"].to_string().contains("Kosten werden mit 0"));
    let dz = by_dep(&v, "az-dz");
    assert_eq!(dz["suggested"]["perMillionTokens"]["input"], "0.825");
    // 1K-Meter der Folgeseite, Version 0718 aus dem Meter.
    let o4 = by_dep(&v, "az-4o");
    assert_eq!(o4["suggested"]["perMillionTokens"], json!({"input": "0.165", "output": "0.66"}), "{o4}");
    assert_eq!(o4["matched"], "gpt-4o-mini (0718)");
    // Manueller Preis weicht ab.
    let oa = by_dep(&v, "oa");
    assert_eq!(oa["status"], "differs");
    assert_eq!(oa["source"], "catalog");
    assert_eq!(oa["current"]["manual"], true);
    assert!(oa["differences"].to_string().contains("perMillionTokens.input"));
    // Claude in Foundry: nicht in Retail Prices → Anthropic-Katalog.
    let cl = by_dep(&v, "claude");
    assert_eq!(cl["source"], "catalog", "{cl}");
    assert_eq!(cl["suggested"]["perMillionTokens"]["input"], "2");
    assert!(cl["notes"].to_string().contains("Kein Azure-Meter"));
    assert!(v["sources"].to_string().contains("swedencentral"));

    // Filter; Zwischenspeicher (kein neuer Abruf).
    let (_, f) = call(&app, Method::GET, "/price-suggestions?deployment=az-se", None).await;
    assert_eq!(f["items"].as_array().unwrap().len(), 1);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
    let (s, _) = call(&app, Method::GET, "/price-suggestions?bogus=1", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Übernahme: zwei neue Preisversionen mit Labels, Audit je Preis.
    let (s, r) = call(
        &app,
        Method::POST,
        "/price-suggestions:accept",
        Some(json!({"items": [{"deployment": "az-se"}, {"deployment": "claude"}]})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let created = r["created"].as_array().unwrap();
    assert_eq!(created.len(), 2, "{r}");
    let az = created.iter().find(|c| c["spec"]["model"] == "gpt54mini").unwrap();
    assert_eq!(az["metadata"]["labels"]["aigw.io/price-source"], "azure-retail");
    assert_eq!(az["metadata"]["labels"]["aigw.io/price-region"], "swedencentral");
    assert_eq!(az["metadata"]["labels"]["aigw.io/price-deployment"], "az-se");
    let audit = store.audit_log(20).await.unwrap();
    assert!(
        audit.iter().filter(|e| format!("{e:?}").contains("gpt54mini")).count() >= 1,
        "Audit-Eintrag fehlt"
    );
    let (_, v) = call(&app, Method::GET, "/price-suggestions", None).await;
    assert_eq!(by_dep(&v, "az-se")["status"], "unchanged");
    assert_eq!(by_dep(&v, "az-se")["current"]["manual"], false);
    assert_eq!(by_dep(&v, "claude")["status"], "unchanged");

    // Unverändert → übersprungen; manueller Preis nur mit overrideManual.
    let (s, r) = call(&app, Method::POST, "/price-suggestions:accept",
        Some(json!({"items": [{"deployment": "az-se"}]}))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["skipped"][0]["deployment"], "az-se");
    let (s, r) = call(&app, Method::POST, "/price-suggestions:accept",
        Some(json!({"items": [{"deployment": "oa"}]}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{r}");
    assert!(r["detail"].as_str().unwrap().contains("overrideManual"));
    let (s, r) = call(&app, Method::POST, "/price-suggestions:accept",
        Some(json!({"items": [{"deployment": "oa", "overrideManual": true}]}))).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["created"][0]["spec"]["perMillionTokens"]["input"], "0.75");

    // Ungültige Auswahl.
    let (s, _) = call(&app, Method::POST, "/price-suggestions:accept",
        Some(json!({"items": [{"deployment": "gibt-es-nicht"}]}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, _) = call(&app, Method::POST, "/price-suggestions:accept",
        Some(json!({"items": [{"deployment": "az-dz"}], "validFrom": "2020-01-01T00:00:00Z"}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, _) = call(&app, Method::POST, "/price-suggestions:accept", Some(json!({"items": []}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn retail_errors_are_tolerated() {
    let fake = fake_retail(true).await;
    let (app, store) = app(&fake).await;
    seed(&store).await;
    let (s, v) = call(&app, Method::GET, "/price-suggestions", None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let src = v["sources"].as_array().unwrap();
    let az = src.iter().find(|s| s["source"] == "azure-retail").unwrap();
    assert!(az["error"].as_str().unwrap().contains("500"), "{az}");
    // Ersatz: OpenAI-Listenpreis für Azure OpenAI, markiert.
    let se = by_dep(&v, "az-se");
    assert_eq!(se["source"], "catalog");
    assert!(se["notes"].to_string().contains("Näherung"));
    // Fehler-Backoff: kein erneuter Abruf beim nächsten Aufruf.
    let n = fake.calls.load(Ordering::SeqCst);
    let _ = call(&app, Method::GET, "/price-suggestions", None).await;
    assert_eq!(fake.calls.load(Ordering::SeqCst), n);
}

#[tokio::test]
async fn disabled_without_suggester() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    store.migrate().await.unwrap();
    let auth: Arc<dyn AdminAuthenticator> = Arc::new(BootstrapToken::new(TOKEN).unwrap());
    let app = router_with(AdminState::new(store, auth));
    let (s, _) = call(&app, Method::GET, "/price-suggestions", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
