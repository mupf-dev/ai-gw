// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Standard-Guardrail-Profile über die Admin-API (AIGW-86): Liste, Dry-Run mit Diff,
//! Anwenden (Namen mit Profil-Präfix, Labels, Audit je Objekt), Idempotenz, Profilwechsel,
//! Payload-Logging-Obergrenze, Rechte wie bei Plugin-Bindungen (F183, AIGW-58).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use aigw_admin::{AdminState, BootstrapToken, UpstreamPolicy, router_with};
use aigw_core::config::{AdminAuthConfig, BootstrapMode};
use aigw_store::Store;
use axum::Router;
use axum::body::Body;
use http_body_util::BodyExt as _;
use hyper::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt as _;

const BOOT: &str = "boot-token-0123456789-0123456789-abcdef";

async fn app() -> Router {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    store.migrate().await.unwrap();
    let cfg = AdminAuthConfig {
        bootstrap: BootstrapMode::On,
        ..AdminAuthConfig::default()
    };
    let state =
        AdminState::from_config(store, &cfg, Some(BootstrapToken::new(BOOT).unwrap()), None)
            .unwrap()
            .with_upstream_policy(UpstreamPolicy::strict());
    router_with(state)
}

async fn call(
    app: &Router,
    token: &str,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method.clone())
        .uri(format!("/admin/api/v1{path}"))
        .header("authorization", format!("Bearer {token}"));
    if matches!(method, Method::PUT | Method::DELETE) {
        req = req.header("if-match", "*");
    }
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
    let v = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|e| panic!("kein JSON ({e}): {}", String::from_utf8_lossy(&bytes)))
    };
    (status, v)
}

async fn ok(app: &Router, token: &str, method: Method, path: &str, body: Option<Value>) -> Value {
    let (s, v) = call(app, token, method, path, body).await;
    assert!(s.is_success(), "{path}: {s} {v}");
    v
}

async fn admin_key(app: &Router, name: &str, roles: Value) -> String {
    let v = ok(
        app,
        BOOT,
        Method::POST,
        "/admin-api-keys",
        Some(json!({"name": name, "roles": roles})),
    )
    .await;
    v["secret"].as_str().unwrap().to_owned()
}

fn team(name: &str) -> Value {
    json!({"level": "team", "target": name})
}

/// (Name, Aktion) der Änderungen.
fn actions(v: &Value) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = v["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["name"].as_str().unwrap().to_owned(),
                c["action"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    out.sort();
    out
}

async fn binding_names(app: &Router) -> Vec<String> {
    let v = ok(app, BOOT, Method::GET, "/plugin-bindings?limit=500", None).await;
    let mut n: Vec<String> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["metadata"]["name"].as_str().unwrap().to_owned())
        .collect();
    n.sort();
    n
}

const P: &str = "/guardrail-profiles";

#[tokio::test]
async fn profiles_list_dry_run_apply_idempotent_and_switch() {
    let app = app().await;
    // Liste mit Inhalt als PluginBinding-Specs.
    let v = ok(&app, BOOT, Method::GET, P, None).await;
    let names: Vec<&str> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["basis", "streng", "offen"]);
    assert_eq!(v["items"][0]["bindings"][1]["spec"]["plugin"], "pii");
    assert_eq!(
        v["items"][0]["bindings"][1]["spec"]["config"]["direction"],
        "both"
    );
    assert_eq!(v["items"][1]["logging"]["payloadModeMax"], "masked");
    assert!(v["items"][0]["description"].as_str().unwrap().len() > 100);

    ok(
        &app,
        BOOT,
        Method::POST,
        "/organizations",
        Some(json!({"metadata": {"name": "acme"}, "spec": {}})),
    )
    .await;
    ok(
        &app,
        BOOT,
        Method::POST,
        "/teams",
        Some(json!({"metadata": {"name": "support"}, "spec": {"organization": "acme", "payloadMode": "full"}})),
    )
    .await;

    // Dry-Run: Diff, nichts geschrieben.
    let v = ok(
        &app,
        BOOT,
        Method::POST,
        &format!("{P}/basis:apply"),
        Some(json!({"scope": team("support"), "dryRun": true})),
    )
    .await;
    assert_eq!(v["dryRun"], true);
    assert_eq!(
        actions(&v),
        [
            ("basis-pii-team-support".into(), "create".into()),
            ("basis-prompt-filter-team-support".into(), "create".into())
        ]
    );
    assert!(binding_names(&app).await.is_empty());

    // Anwenden.
    let v = ok(
        &app,
        BOOT,
        Method::POST,
        &format!("{P}/basis:apply"),
        Some(json!({"scope": team("support")})),
    )
    .await;
    assert_eq!(v["dryRun"], false);
    assert_eq!(
        binding_names(&app).await,
        ["basis-pii-team-support", "basis-prompt-filter-team-support"]
    );
    let b = ok(
        &app,
        BOOT,
        Method::GET,
        "/plugin-bindings/basis-pii-team-support",
        None,
    )
    .await;
    assert_eq!(
        b["metadata"]["labels"]["aigw.io/guardrail-profile"],
        "basis"
    );
    assert_eq!(
        b["metadata"]["labels"]["aigw.io/guardrail-profile-version"],
        "1"
    );
    assert_eq!(b["spec"]["scope"], team("support"));
    // Audit je Objekt.
    let a = ok(
        &app,
        BOOT,
        Method::GET,
        "/audit-log?kind=PluginBinding&action=create",
        None,
    )
    .await;
    assert_eq!(a["items"].as_array().unwrap().len(), 2, "{a}");

    // Idempotent: nochmal → alles unverändert, kein neuer Audit-Eintrag.
    let v = ok(
        &app,
        BOOT,
        Method::POST,
        &format!("{P}/basis:apply"),
        Some(json!({"scope": team("support")})),
    )
    .await;
    assert!(actions(&v).iter().all(|(_, a)| a == "unchanged"), "{v}");
    let a = ok(
        &app,
        BOOT,
        Method::GET,
        "/audit-log?kind=PluginBinding",
        None,
    )
    .await;
    assert_eq!(a["items"].as_array().unwrap().len(), 2);

    // Von Hand angelegte Bindung auf demselben Scope bleibt beim Profilwechsel erhalten.
    ok(
        &app,
        BOOT,
        Method::POST,
        "/plugin-bindings",
        Some(json!({"metadata": {"name": "eigene"}, "spec": {"scope": team("support"), "plugin": "pii", "order": 5, "config": {"action": "log-only"}}})),
    )
    .await;

    // Wechsel auf „Streng“ (ohne content-safety-Angaben): Basis-Bindungen werden ersetzt,
    // Payload-Logging des Teams auf höchstens masked begrenzt.
    let v = ok(
        &app,
        BOOT,
        Method::POST,
        &format!("{P}/streng:apply"),
        Some(json!({"scope": team("support")})),
    )
    .await;
    assert_eq!(
        actions(&v),
        [
            ("basis-pii-team-support".into(), "delete".into()),
            ("basis-prompt-filter-team-support".into(), "delete".into()),
            ("streng-pii-team-support".into(), "create".into()),
            ("streng-prompt-filter-team-support".into(), "create".into()),
            ("support".into(), "update".into()),
        ]
    );
    assert_eq!(v["warnings"].as_array().unwrap().len(), 1, "{v}");
    assert!(
        v["warnings"][0]
            .as_str()
            .unwrap()
            .contains("content-safety")
    );
    assert_eq!(
        binding_names(&app).await,
        [
            "eigene",
            "streng-pii-team-support",
            "streng-prompt-filter-team-support"
        ]
    );
    let t = ok(&app, BOOT, Method::GET, "/teams/support", None).await;
    assert_eq!(t["spec"]["payloadModeMax"], "masked");
    assert_eq!(t["spec"]["payloadMode"], "masked");
    let b = ok(
        &app,
        BOOT,
        Method::GET,
        "/plugin-bindings/streng-pii-team-support",
        None,
    )
    .await;
    assert_eq!(b["spec"]["locked"], true);
    assert_eq!(b["spec"]["config"]["categories"]["creditCard"], "reject");

    // „Streng“ mit Prüfmodell (provider model) – Dry-Run zeigt die dritte Bindung.
    let v = ok(
        &app,
        BOOT,
        Method::POST,
        &format!("{P}/streng:apply"),
        Some(json!({"scope": team("support"), "dryRun": true,
                    "contentSafety": {"provider": "model", "modelGroup": "llama-guard"}})),
    )
    .await;
    assert!(
        actions(&v).contains(&("streng-content-safety-team-support".into(), "create".into())),
        "{v}"
    );

    // Fehler: unbekanntes Profil, Scope ohne Ziel, unbekanntes Team, unbekannter Parameter.
    for (path, body, status) in [
        (
            format!("{P}/gibtsnicht:apply"),
            json!({"scope": {"level": "global"}}),
            StatusCode::NOT_FOUND,
        ),
        (
            format!("{P}/basis"),
            json!({"scope": {"level": "global"}}),
            StatusCode::NOT_FOUND,
        ),
        (
            format!("{P}/basis:apply"),
            json!({"scope": {"level": "team"}}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            format!("{P}/basis:apply"),
            json!({"scope": team("fehlt")}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            format!("{P}/streng:apply"),
            json!({"scope": team("support"), "contentSafety": {"action": "flag"}}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        let (s, v) = call(&app, BOOT, Method::POST, &path, Some(body)).await;
        assert_eq!(s, status, "{path}: {v}");
    }

    // Global: Logging nur als Hinweis.
    let v = ok(
        &app,
        BOOT,
        Method::POST,
        &format!("{P}/offen:apply"),
        Some(json!({"scope": {"level": "global"}})),
    )
    .await;
    assert_eq!(
        actions(&v),
        [("offen-prompt-filter-global".into(), "create".into())]
    );
    assert!(
        v["warnings"][0]
            .as_str()
            .unwrap()
            .contains("observe.payload"),
        "{v}"
    );
}

#[tokio::test]
async fn profiles_follow_binding_rbac() {
    let app = app().await;
    ok(
        &app,
        BOOT,
        Method::POST,
        "/organizations",
        Some(json!({"metadata": {"name": "acme"}, "spec": {}})),
    )
    .await;
    ok(
        &app,
        BOOT,
        Method::POST,
        "/organizations",
        Some(json!({"metadata": {"name": "other"}, "spec": {}})),
    )
    .await;
    for (t, o) in [("platform", "acme"), ("fremd", "other")] {
        ok(
            &app,
            BOOT,
            Method::POST,
            "/teams",
            Some(json!({"metadata": {"name": t}, "spec": {"organization": o}})),
        )
        .await;
    }
    let org_admin = admin_key(
        &app,
        "acme-admin",
        json!([{"role": "org-admin", "organization": "acme"}]),
    )
    .await;
    let team_admin = admin_key(
        &app,
        "plat-admin",
        json!([{"role": "team-admin", "organization": "acme", "team": "platform"}]),
    )
    .await;

    // team-admin: eigenes Team ja (Logging-Obergrenze nur als Hinweis, S-09), global/fremd nein.
    let v = ok(
        &app,
        &team_admin,
        Method::POST,
        &format!("{P}/streng:apply"),
        Some(json!({"scope": team("platform")})),
    )
    .await;
    assert!(
        v["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("payloadModeMax")),
        "{v}"
    );
    assert!(!actions(&v).iter().any(|(n, _)| n == "platform"), "{v}");
    for (scope, dry) in [
        (json!({"level": "global"}), true),
        (team("fremd"), true),
        (json!({"level": "organization", "target": "acme"}), false),
    ] {
        let (s, v) = call(
            &app,
            &team_admin,
            Method::POST,
            &format!("{P}/basis:apply"),
            Some(json!({"scope": scope, "dryRun": dry})),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    }

    // org-admin: Organisation ja; Verweis auf ein globales Secret (content-safety) nein (AIGW-58).
    let v = ok(
        &app,
        &org_admin,
        Method::POST,
        &format!("{P}/streng:apply"),
        Some(json!({"scope": {"level": "organization", "target": "acme"}})),
    )
    .await;
    // Die Organisation selbst ändert nur ein admin (S-09): Logging als Hinweis.
    assert!(!actions(&v).iter().any(|(n, _)| n == "acme"), "{v}");
    assert!(v["warnings"].to_string().contains("Payload-Logging"), "{v}");
    let cs = json!({"provider": "azure", "endpoint": "https://cs.example.com", "secret": "cs-key"});
    let (s, v) = call(
        &app,
        &org_admin,
        Method::POST,
        &format!("{P}/streng:apply"),
        Some(json!({"scope": {"level": "organization", "target": "acme"}, "dryRun": true, "contentSafety": cs})),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert!(v["detail"].as_str().unwrap().contains("global"), "{v}");
    // Globaler admin darf es.
    let v = ok(
        &app,
        BOOT,
        Method::POST,
        &format!("{P}/streng:apply"),
        Some(json!({"scope": {"level": "organization", "target": "acme"}, "dryRun": true, "contentSafety": cs})),
    )
    .await;
    assert!(
        actions(&v).contains(&(
            "streng-content-safety-organization-acme".into(),
            "create".into()
        )),
        "{v}"
    );
}
