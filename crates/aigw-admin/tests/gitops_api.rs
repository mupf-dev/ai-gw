// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! GitOps-Routen der Admin-API (AIGW-33): Export, Apply/Trockenlauf, eingeschränkte
//! Geheimnis-Herkunft und Modus „nur YAML“.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::sync::Arc;

use aigw_admin::gitops::{GitopsState, router as gitops_router};
use aigw_admin::{AdminAuthenticator, AdminState, BootstrapToken, read_only, router_with};
use aigw_store::{Keyring, MasterKey, Store};
use axum::Router;
use axum::body::Body;
use http_body_util::BodyExt as _;
use hyper::{Method, Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt as _;

const TOKEN: &str = "test-token-0123456789-0123456789-abcdef";

const MANIFEST: &str = "apiVersion: aigw/v1alpha1
kind: Organization
metadata: {name: acme}
---
apiVersion: aigw/v1alpha1
kind: Team
metadata: {name: platform}
spec: {organization: acme}
";

async fn app(read_only_mode: bool) -> (Router, Store) {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    store.migrate().await.unwrap();
    let (master, _) = MasterKey::generate().unwrap();
    let auth: Arc<dyn AdminAuthenticator> = Arc::new(BootstrapToken::new(TOKEN).unwrap());
    let router = router_with(
        AdminState::new(store.clone(), Arc::clone(&auth)).with_read_only(read_only_mode),
    )
    .merge(gitops_router(GitopsState {
        store: store.clone(),
        keyring: Arc::new(Keyring::new(master)),
        auth,
        upstreams: aigw_admin::UpstreamPolicy::strict(),
        secret_dir: None,
    }));
    (read_only::apply(router, read_only_mode), store)
}

async fn call(
    app: &Router,
    method: Method,
    path: &str,
    body: Option<(&str, String)>,
    auth: bool,
) -> (StatusCode, String, String) {
    let mut req = Request::builder().method(method).uri(path);
    if auth {
        req = req.header("authorization", format!("Bearer {TOKEN}"));
    }
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
    let ct = res
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, ct, String::from_utf8(bytes.to_vec()).unwrap())
}

fn json(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or_else(|e| panic!("kein JSON ({e}): {s}"))
}

#[tokio::test]
async fn export_and_apply() {
    let (app, store) = app(false).await;
    let yaml = || Some(("application/yaml", MANIFEST.to_owned()));

    // Anmeldung nötig.
    let (s, _, _) = call(
        &app,
        Method::GET,
        "/admin/api/v1/config:export",
        None,
        false,
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // Trockenlauf: Plan, nichts geschrieben.
    let (s, _, b) = call(
        &app,
        Method::POST,
        "/admin/api/v1/config:apply?dryRun=true",
        yaml(),
        true,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let r = json(&b);
    assert_eq!(r["applied"], false);
    assert_eq!(r["plan"]["summary"]["create"], 2);
    assert_eq!(r["plan"]["changes"][0]["action"], "create");
    assert_eq!(store.config_version().await.unwrap(), 0);

    // Anwenden: eine Version, Akteur gitops:<principal>.
    let (s, _, b) = call(
        &app,
        Method::POST,
        "/admin/api/v1/config:apply",
        yaml(),
        true,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let r = json(&b);
    assert_eq!(r["applied"], true);
    assert_eq!(r["configVersion"], 1);
    let audit = store.audit_log(10).await.unwrap();
    assert!(audit.iter().all(|a| a.actor == "gitops:bootstrap-admin"));

    // Export enthält beides, ohne Server-Felder.
    let (s, ct, b) = call(&app, Method::GET, "/admin/api/v1/config:export", None, true).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(ct, "application/yaml");
    assert!(
        b.contains("kind: Organization") && b.contains("kind: Team"),
        "{b}"
    );
    assert!(!b.contains("createdAt"));

    // Unbekannter Parameter → 400.
    let (s, _, _) = call(
        &app,
        Method::GET,
        "/admin/api/v1/config:export?foo=1",
        None,
        true,
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Ungültiges Manifest → 422 mit allen Fehlern, nichts angewendet.
    let bad =
        "apiVersion: aigw/v1alpha1\nkind: Team\nmetadata: {name: x}\nspec: {organization: fehlt}\n";
    let (s, ct, b) = call(
        &app,
        Method::POST,
        "/admin/api/v1/config:apply",
        Some(("application/yaml", bad.to_owned())),
        true,
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(ct, "application/problem+json");
    assert!(json(&b)["detail"].as_str().unwrap().contains("fehlt"));
    assert_eq!(store.config_version().await.unwrap(), 1);

    // Über die API: nur AIGW_SECRET_*-Variablen, keine Dateien.
    for r in ["{env: HOME}", "{file: /etc/passwd}"] {
        let cred = format!(
            "apiVersion: aigw/v1alpha1\nkind: ProviderCredential\nmetadata: {{name: c}}\nspec:\n  provider: openai\n  data:\n    apiKey:\n      valueFrom: {r}\n"
        );
        let (s, _, b) = call(
            &app,
            Method::POST,
            "/admin/api/v1/config:apply?dryRun=true",
            Some(("application/yaml", cred)),
            true,
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
        assert!(b.contains("nicht erlaubt"), "{b}");
    }
}

#[tokio::test]
async fn read_only_mode_blocks_admin_writes_but_not_gitops() {
    let (app, _store) = app(true).await;
    let org = Some((
        "application/json",
        r#"{"metadata":{"name":"acme"},"spec":{}}"#.to_owned(),
    ));
    let (s, ct, b) = call(&app, Method::POST, "/admin/api/v1/organizations", org, true).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(ct, "application/problem+json");
    assert_eq!(json(&b)["code"], "read-only");

    let (s, _, _) = call(&app, Method::GET, "/admin/api/v1/organizations", None, true).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _, _) = call(
        &app,
        Method::DELETE,
        "/admin/api/v1/organizations/acme",
        None,
        true,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    let (s, _, b) = call(
        &app,
        Method::POST,
        "/admin/api/v1/config:apply",
        Some(("application/yaml", MANIFEST.to_owned())),
        true,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, _, b) = call(&app, Method::GET, "/admin/api/v1/organizations", None, true).await;
    assert_eq!(s, StatusCode::OK);
    assert!(b.contains("acme"));
    // F56: die UI erfährt den Modus über auth/info (ohne Anmeldung abrufbar).
    let (s, _, b) = call(&app, Method::GET, "/admin/api/v1/auth/info", None, false).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json(&b)["readOnly"], true);
    assert_eq!(json(&b)["bootstrap"], false);
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
