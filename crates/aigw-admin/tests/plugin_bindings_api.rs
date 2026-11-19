// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Plugin-Bindungen über die Admin-API (AIGW-69): Prüfung mit der Plugin-Registry beim
//! Schreiben (F118/F165, 422 mit verständlicher Meldung, SSRF-Regeln aus `admin.*`),
//! wirksamer Fehlermodus je Bindung (F148) und dieselbe Prüfung im GitOps-Plan.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::sync::Arc;

use aigw_admin::gitops::{GitopsState, router as gitops_router};
use aigw_admin::{AdminAuthenticator, AdminState, BootstrapToken, UpstreamPolicy, router_with};
use aigw_core::config::AdminConfig;
use aigw_store::{Keyring, MasterKey, Store};
use axum::Router;
use axum::body::Body;
use http_body_util::BodyExt as _;
use hyper::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt as _;

const TOKEN: &str = "test-token-0123456789-0123456789-abcdef";

/// `http` und private Ziele nur für `hooks.lan` (wie `admin.allow_*_upstreams`).
fn policy() -> UpstreamPolicy {
    UpstreamPolicy::from_config(&AdminConfig {
        allow_insecure_upstreams: vec!["hooks.lan".into()],
        allow_private_upstreams: vec!["hooks.lan".into()],
        ..AdminConfig::default()
    })
    .unwrap()
}

async fn app() -> (Router, Store) {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    store.migrate().await.unwrap();
    let (master, _) = MasterKey::generate().unwrap();
    let auth: Arc<dyn AdminAuthenticator> = Arc::new(BootstrapToken::new(TOKEN).unwrap());
    let router = router_with(
        AdminState::new(store.clone(), Arc::clone(&auth)).with_upstream_policy(policy()),
    )
    .merge(gitops_router(GitopsState {
        store: store.clone(),
        keyring: Arc::new(Keyring::new(master)),
        auth,
        upstreams: policy(),
        secret_dir: None,
        manage_users: false,
    }));
    (router, store)
}

async fn call(
    app: &Router,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    call_raw(
        app,
        method,
        path,
        body.map(|b| ("application/json", b.to_string())),
    )
    .await
}

async fn call_raw(
    app: &Router,
    method: Method,
    path: &str,
    body: Option<(&str, String)>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {TOKEN}"));
    let body = match body {
        Some((ct, b)) => {
            req = req.header("content-type", ct);
            Body::from(b)
        }
        None => Body::empty(),
    };
    let res = app
        .clone()
        .oneshot(with_if_match(req).body(body).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let v = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|e| panic!("kein JSON ({e}): {}", String::from_utf8_lossy(&bytes)))
    };
    (status, v)
}

const BASE: &str = "/admin/api/v1/plugin-bindings";

fn binding(name: &str, plugin: &str, config: Value) -> Value {
    json!({"metadata": {"name": name},
           "spec": {"scope": {"level": "global"}, "plugin": plugin, "order": 10,
                    "config": config}})
}

#[tokio::test]
async fn bindings_are_validated_and_show_effective_fail_mode() {
    let (app, _) = app().await;

    // Gültig: Standardwerte der Plugin-Art (pii closed, webhook open).
    let (s, v) = call(
        &app,
        Method::POST,
        BASE,
        Some(binding(
            "pii-default",
            "pii",
            json!({"action": "pseudonymize", "entropy": {"enabled": false}}),
        )),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(
        v["effective"],
        json!({"failMode": "closed", "failModeSource": "plugin-default", "timeoutMs": 1000,
               "outputRejectMode": "finish"})
    );
    let (s, v) = call(
        &app,
        Method::POST,
        BASE,
        Some(binding(
            "audit",
            "webhook",
            json!({"url": "http://hooks.lan:8080/aigw", "phases": ["response"]}),
        )),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["effective"]["failMode"], "open");
    assert_eq!(v["effective"]["failModeSource"], "plugin-default");
    assert_eq!(v["effective"]["timeoutMs"], 200);

    // Ungültig → 422 mit der Meldung des Plugins.
    let cases = [
        ("pii", json!({"acton": "mask"}), "acton"),
        (
            "pii",
            json!({"entropy": {"minLength": 4}}),
            "entropy.minLength",
        ),
        ("gibt-es-nicht", json!({}), "unbekanntes Plugin"),
        (
            "webhook",
            json!({"url": "https://169.254.169.254/latest"}),
            "url",
        ),
        (
            "webhook",
            json!({"url": "http://hooks.example.org/x"}),
            "url",
        ),
        (
            "content-safety",
            json!({"provider": "azure", "credential": "c", "endpoint": "https://10.0.0.1"}),
            "endpoint",
        ),
    ];
    for (plugin, cfg, want) in cases {
        let (s, v) = call(&app, Method::POST, BASE, Some(binding("x", plugin, cfg))).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{plugin}: {v}");
        let detail = v["detail"].as_str().unwrap_or_default();
        assert!(
            detail.starts_with(&format!("Plugin-Bindung ({plugin}): ")) && detail.contains(want),
            "{plugin}: {detail}"
        );
    }
    // F117: Steuerfelder als eigene Felder – ungültige Werte (auch in der alten Form in
    // `config`) werden schon beim Lesen der Eingabe abgelehnt.
    for (spec_extra, want) in [
        (json!({"config": {"failMode": "sometimes"}}), "failMode"),
        (
            json!({"config": {"outputRejectMode": "drop"}}),
            "outputRejectMode",
        ),
        (json!({"failMode": "sometimes"}), "failMode"),
        (
            json!({"failMode": "open", "config": {"failMode": "closed"}}),
            "failMode",
        ),
        (json!({"timeoutMs": 0}), "timeoutMs"),
    ] {
        let mut body = binding("x", "pii", json!({}));
        for (k, v) in spec_extra.as_object().unwrap() {
            body["spec"][k] = v.clone();
        }
        let (s, v) = call(&app, Method::POST, BASE, Some(body)).await;
        assert!(s.is_client_error(), "{spec_extra}: {s} {v}");
        let detail = v["detail"].as_str().unwrap_or_default();
        assert!(detail.contains(want), "{spec_extra}: {detail}");
    }
    // Neue Form: Steuerfelder neben `config`, gelesen als eigene Felder.
    let mut body = binding("pii-ctl", "pii", json!({"action": "mask"}));
    body["spec"]["failMode"] = json!("open");
    body["spec"]["timeoutMs"] = json!(750);
    let (s, v) = call(&app, Method::POST, BASE, Some(body)).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["spec"]["failMode"], "open");
    assert!(v["spec"]["config"].get("failMode").is_none());
    assert_eq!(v["effective"]["failMode"], "open");
    assert_eq!(v["effective"]["failModeSource"], "binding");
    assert_eq!(v["effective"]["timeoutMs"], 750);
    let (s, _) = call(&app, Method::DELETE, &format!("{BASE}/pii-ctl"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    // Ersetzen: Fehlermodus der Bindung überschreibt den Standard; ungültig → 422.
    let (s, v) = call(
        &app,
        Method::PUT,
        &format!("{BASE}/audit"),
        Some(binding(
            "audit",
            "webhook",
            json!({"url": "http://hooks.lan:8080/aigw", "failMode": "closed", "timeoutMs": 500}),
        )),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["effective"]["failMode"], "closed");
    assert_eq!(v["effective"]["failModeSource"], "binding");
    assert_eq!(v["effective"]["timeoutMs"], 500);
    // Alte Form (Steuerschlüssel in `config`) wird in die Felder übernommen.
    assert_eq!(v["spec"]["failMode"], "closed");
    assert_eq!(v["spec"]["timeoutMs"], 500);
    assert!(v["spec"]["config"].get("failMode").is_none());
    let (s, _) = call(
        &app,
        Method::PUT,
        &format!("{BASE}/audit"),
        Some(binding("audit", "webhook", json!({"url": "ftp://x"}))),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);

    // Liste mit Filter.
    let (s, v) = call(&app, Method::GET, &format!("{BASE}?plugin=webhook"), None).await;
    assert_eq!(s, StatusCode::OK);
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["metadata"]["name"], "audit");
    assert!(items[0]["effective"].is_object());
    let (_, v) = call(&app, Method::GET, &format!("{BASE}/pii-default"), None).await;
    assert_eq!(v["spec"]["plugin"], "pii");

    let (s, _) = call(&app, Method::DELETE, &format!("{BASE}/audit"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn gitops_plan_validates_plugin_bindings() {
    let (app, store) = app().await;
    let doc = |config: &str| {
        format!(
            "apiVersion: aigw/v1alpha1\nkind: PluginBinding\nmetadata: {{name: pf}}\nspec:\n  scope: {{level: global}}\n  plugin: prompt-filter\n  config: {config}\n"
        )
    };
    let (s, v) = call_raw(
        &app,
        Method::POST,
        "/admin/api/v1/config:apply?dryRun=true",
        Some(("application/yaml", doc("{words: {terms: [x]}, bogus: 1}"))),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let detail = v["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("Plugin-Bindung (prompt-filter)"), "{v}");
    assert!(detail.contains("bogus"), "{v}");
    assert_eq!(store.config_version().await.unwrap(), 0);

    let (s, v) = call_raw(
        &app,
        Method::POST,
        "/admin/api/v1/config:apply",
        Some(("application/yaml", doc("{words: {terms: [x]}}"))),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (_, v) = call(&app, Method::GET, &format!("{BASE}/pf"), None).await;
    assert_eq!(v["effective"]["failMode"], "closed");

    // AIGW-109/111/112: neue Felder bzw. Plugin-Art, Wertebereiche geprüft.
    for (plugin, config, ok) in [
        (
            "prompt-filter",
            "{invisible: {action: strip}, direction: both, words: {terms: [x]}}",
            true,
        ),
        ("prompt-filter", "{invisible: {action: drop}}", false),
        ("prompt-leak", "{canary: {rotation: stable}}", true),
        ("prompt-leak", "{verbatim: {ngram: 2}}", false),
    ] {
        let yaml = format!(
            "apiVersion: aigw/v1alpha1\nkind: PluginBinding\nmetadata: {{name: neu}}\nspec:\n  scope: {{level: global}}\n  plugin: {plugin}\n  config: {config}\n"
        );
        let (s, v) = call_raw(
            &app,
            Method::POST,
            "/admin/api/v1/config:apply?dryRun=true",
            Some(("application/yaml", yaml)),
        )
        .await;
        let want = if ok {
            StatusCode::OK
        } else {
            StatusCode::UNPROCESSABLE_ENTITY
        };
        assert_eq!(s, want, "{plugin} {config}: {v}");
    }
}

/// S-22: Private Ziele aus `admin.allow_private_upstreams` nur für Bindungen der Ebenen
/// `global`/`model-group` (nur globale Admins); Organisation/Team/Key → nur öffentliche Ziele –
/// über die Admin-API wie über GitOps.
#[tokio::test]
async fn private_webhook_targets_only_on_global_levels() {
    let (app, _) = app().await;
    let hook = json!({"url": "http://hooks.lan:8080/aigw"});
    let org = "apiVersion: aigw/v1alpha1\nkind: Organization\nmetadata: {name: acme}\n---\n\
               apiVersion: aigw/v1alpha1\nkind: Team\nmetadata: {name: web}\n\
               spec: {organization: acme}\n";
    let (s, v) = call_raw(
        &app,
        Method::POST,
        "/admin/api/v1/config:apply",
        Some(("application/yaml", org.to_owned())),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    for (level, target) in [("organization", "acme"), ("team", "web")] {
        let body = json!({"metadata": {"name": format!("hook-{level}")},
            "spec": {"scope": {"level": level, "target": target}, "plugin": "webhook",
                     "order": 10, "config": hook}});
        let (s, v) = call(&app, Method::POST, BASE, Some(body)).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{level}: {v}");
        let detail = v["detail"].as_str().unwrap_or_default();
        assert!(
            detail.contains("allow_private_upstreams"),
            "{level}: {detail}"
        );
        // Öffentliches Ziel bleibt erlaubt.
        let body = json!({"metadata": {"name": format!("pub-{level}")},
            "spec": {"scope": {"level": level, "target": target}, "plugin": "webhook",
                     "order": 10, "config": {"url": "https://hooks.example.org/aigw"}}});
        let (s, v) = call(&app, Method::POST, BASE, Some(body)).await;
        assert_eq!(s, StatusCode::CREATED, "{level}: {v}");
    }
    // GitOps: gleiche Regel.
    let doc = format!(
        "{org}---\napiVersion: aigw/v1alpha1\nkind: PluginBinding\nmetadata: {{name: gh}}\n\
         spec:\n  scope: {{level: team, target: web}}\n  plugin: webhook\n\
         \x20 config: {{url: \"http://hooks.lan:8080/aigw\"}}\n"
    );
    let (s, v) = call_raw(
        &app,
        Method::POST,
        "/admin/api/v1/config:apply?dryRun=true",
        Some(("application/yaml", doc)),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(v.to_string().contains("allow_private_upstreams"), "{v}");
    // Global (nur globale Admins) darf das freigegebene private Ziel.
    let (s, v) = call(
        &app,
        Method::POST,
        BASE,
        Some(binding("hook-global", "webhook", hook)),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
}

/// F38: `If-Match` ist Pflicht – Anfragen ohne eigenen ETag schreiben hier mit `If-Match: *`
/// (eigene Tests prüfen 428/412 ausdrücklich).
fn with_if_match(req: axum::http::request::Builder) -> axum::http::request::Builder {
    use axum::http::Method as M;
    let action = req.uri_ref().is_some_and(|u| {
        let p = u.path();
        ["/revoke", "/suspend", "/resume", "/rotate"]
            .iter()
            .any(|s| p.ends_with(s))
    });
    let write = req
        .method_ref()
        .is_some_and(|m| matches!(*m, M::PUT | M::PATCH | M::DELETE) || (*m == M::POST && action));
    if write
        && !req
            .headers_ref()
            .is_some_and(|h| h.contains_key("if-match"))
    {
        req.header("if-match", "*")
    } else {
        req
    }
}

/// AIGW-52: Plugin-Katalog und Testlauf (nur lokale Plugins, schreibt nichts).
#[tokio::test]
async fn catalog_and_test_run() {
    let (app, store) = app().await;
    let (s, v) = call(&app, Method::GET, "/admin/api/v1/plugins", None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let items = v["items"].as_array().unwrap();
    let get = |n: &str| items.iter().find(|i| i["name"] == n).unwrap().clone();
    assert_eq!(get("webhook")["defaultFailMode"], "open");
    assert_eq!(get("content-safety")["defaultFailMode"], "closed");
    assert_eq!(get("pii")["testable"], true);
    assert_eq!(get("webhook")["testable"], false);

    let version = store.config_version().await.unwrap();
    let test = "/admin/api/v1/plugin-bindings:test";
    let (s, v) = call(
        &app,
        Method::POST,
        test,
        Some(json!({"plugin": "pii", "config": {"action": "mask"}, "text": "an max@example.org"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["outcome"], "modified", "{v}");
    assert!(
        !v["text"].as_str().unwrap().contains("max@example.org"),
        "{v}"
    );
    assert_eq!(v["hits"][0]["category"], "email", "{v}");

    let (s, v) = call(
        &app,
        Method::POST,
        test,
        Some(json!({"plugin": "prompt-filter", "config": {"words": {"terms": ["geheim"]}}, "text": "das ist geheim"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["outcome"], "rejected", "{v}");
    assert_eq!(v["status"], 400, "{v}");

    // Ungültige Konfiguration → 422 mit Feldpräfix `config:`.
    let (s, v) = call(
        &app,
        Method::POST,
        test,
        Some(json!({"plugin": "pii", "config": {"acton": "mask"}, "text": "x"})),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(v["detail"].as_str().unwrap().starts_with("config: "), "{v}");
    // Externe Plugins werden nicht ausgeführt.
    let (s, v) = call(&app, Method::POST, test, Some(json!({"plugin": "webhook", "config": {"url": "https://hooks.example.org"}, "text": "x"}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(v["detail"].as_str().unwrap().starts_with("plugin: "), "{v}");
    // Nichts geschrieben.
    assert_eq!(store.config_version().await.unwrap(), version);
}
