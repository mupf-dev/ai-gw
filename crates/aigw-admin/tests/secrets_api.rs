// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Geheimnisse über die Admin-API (F149): Werte nur schreibbar, nie in Antworten, Fehlern
//! oder im Audit-Log; `PATCH` je Feld; `409` beim Löschen eines verwendeten Geheimnisses;
//! Prüfung der Hash-Kette über `GET /audit-log/verify` (F116).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::sync::Arc;

use aigw_admin::{AdminAuthenticator, AdminState, BootstrapToken, router_with};
use aigw_store::{Keyring, MasterKey, Store};
use axum::Router;
use axum::body::Body;
use http_body_util::BodyExt as _;
use hyper::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt as _;

const TOKEN: &str = "test-token-0123456789-0123456789-abcdef";
const BASE: &str = "/admin/api/v1/secrets";

async fn app() -> (Router, Store, Arc<Keyring>) {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    store.migrate().await.unwrap();
    let (master, _) = MasterKey::generate().unwrap();
    let keyring = Arc::new(Keyring::new(master));
    let auth: Arc<dyn AdminAuthenticator> = Arc::new(BootstrapToken::new(TOKEN).unwrap());
    let router =
        router_with(AdminState::new(store.clone(), auth).with_keyring(Arc::clone(&keyring)));
    (router, store, keyring)
}

async fn call(
    app: &Router,
    method: Method,
    path: &str,
    etag: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value, Option<String>) {
    let mut req = Request::builder()
        .method(method.clone())
        .uri(path)
        .header("authorization", format!("Bearer {TOKEN}"));
    if let Some(e) = etag {
        req = req.header("if-match", e);
    }
    let body = match body {
        Some(b) => {
            let ct = if method == Method::PATCH {
                "application/merge-patch+json"
            } else {
                "application/json"
            };
            req = req.header("content-type", ct);
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let res = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let etag = res
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    assert!(
        !text.contains("hmac-geheim") && !text.contains("neu-geheim"),
        "Geheimwert in Antwort: {text}"
    );
    let v = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::String(text))
    };
    (status, v, etag)
}

#[tokio::test]
async fn secrets_crud_without_values() {
    let (app, store, keyring) = app().await;

    let (s, v, _) = call(
        &app,
        Method::POST,
        BASE,
        None,
        Some(json!({"metadata": {"name": "hooks"},
                    "spec": {"description": "HMAC", "data": {"value": {"value": "hmac-geheim"}}}})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["spec"]["fields"], json!(["value"]));
    assert_eq!(v["spec"]["description"], "HMAC");
    assert!(v["status"]["keyId"].is_string());

    // valueFrom ist über die Admin-API nicht erlaubt; Typfehler ohne Wert im Text.
    let (s, _, _) = call(
        &app,
        Method::POST,
        BASE,
        None,
        Some(json!({"metadata": {"name": "x"},
                    "spec": {"data": {"value": {"valueFrom": {"env": "HOME"}}}}})),
    )
    .await;
    assert!(s.is_client_error());
    let (s, v, _) = call(
        &app,
        Method::POST,
        BASE,
        None,
        Some(json!({"metadata": {"name": "x"}, "spec": {"data": {"1bad": {"value": "a"}}}})),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let (s, _, _) = call(
        &app,
        Method::POST,
        BASE,
        None,
        Some(json!({"metadata": {"name": "x"}, "spec": {"data": {}}})),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);

    // Lesen und Liste ohne Werte.
    let (s, v, etag) = call(&app, Method::GET, &format!("{BASE}/hooks"), None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["metadata"]["name"], "hooks");
    let (s, v, _) = call(&app, Method::GET, BASE, None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["items"].as_array().unwrap().len(), 1);

    // PATCH: Feld hinzufügen, Beschreibung entfernen.
    let (s, v, etag) = call(
        &app,
        Method::PATCH,
        &format!("{BASE}/hooks"),
        etag.as_deref(),
        Some(json!({"spec": {"description": null, "data": {"apiKey": {"value": "neu-geheim"}}}})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["spec"]["fields"], json!(["apiKey", "value"]));
    assert!(v["spec"].get("description").is_none());
    let plain = store.decrypt_secrets(&keyring).await.unwrap();
    assert_eq!(
        plain["hooks"].1.get("value").unwrap().expose(),
        "hmac-geheim"
    );
    assert_eq!(
        plain["hooks"].1.get("apiKey").unwrap().expose(),
        "neu-geheim"
    );
    // Letztes Feld entfernen → 422.
    let (s, _, _) = call(
        &app,
        Method::PATCH,
        &format!("{BASE}/hooks"),
        etag.as_deref(),
        Some(json!({"spec": {"data": {"apiKey": null, "value": null}}})),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);

    // PUT ersetzt alles.
    let (s, v, etag) = call(
        &app,
        Method::PUT,
        &format!("{BASE}/hooks"),
        etag.as_deref(),
        Some(json!({"metadata": {"name": "hooks"},
                    "spec": {"data": {"value": {"value": "hmac-geheim"}}}})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["spec"]["fields"], json!(["value"]));

    // In Verwendung (MCP-Server) → 409.
    let (s, v, _) = call(
        &app,
        Method::POST,
        "/admin/api/v1/mcp-servers",
        None,
        Some(json!({"metadata": {"name": "gh"},
                    "spec": {"url": "https://mcp.example.org/mcp", "secret": "hooks"}})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["spec"]["secret"], "hooks");
    let (s, _, _) = call(
        &app,
        Method::DELETE,
        &format!("{BASE}/hooks"),
        etag.as_deref(),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    // credential und secret zugleich → 422.
    let (s, _, _) = call(
        &app,
        Method::POST,
        "/admin/api/v1/mcp-servers",
        None,
        Some(json!({"metadata": {"name": "gh2"},
                    "spec": {"url": "https://mcp.example.org/mcp", "secret": "hooks",
                             "credential": "x"}})),
    )
    .await;
    assert!(s.is_client_error());
    let (_, _, mcp_etag) = call(
        &app,
        Method::GET,
        "/admin/api/v1/mcp-servers/gh",
        None,
        None,
    )
    .await;
    let (s, _, _) = call(
        &app,
        Method::DELETE,
        "/admin/api/v1/mcp-servers/gh",
        mcp_etag.as_deref(),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _, _) = call(
        &app,
        Method::DELETE,
        &format!("{BASE}/hooks"),
        etag.as_deref(),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    // Audit: Art `secret`, keine Werte; Hash-Kette unversehrt.
    let (s, v, _) = call(
        &app,
        Method::GET,
        "/admin/api/v1/audit-log?kind=secret",
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let items = v["items"].as_array().unwrap();
    assert!(items.len() >= 4, "{v}");
    assert!(items.iter().all(|e| e["kind"] == "secret"));
    let (s, v, _) = call(
        &app,
        Method::GET,
        "/admin/api/v1/audit-log/verify",
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["ok"], true, "{v}");
    assert!(v["checked"].as_u64().unwrap() >= 6);
    assert_eq!(v["headHash"].as_str().unwrap().len(), 64);
}
