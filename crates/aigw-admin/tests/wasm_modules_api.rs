// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! WASM-Module über die Admin-API (AIGW-63): Hochladen nur durch globale Admins und nur mit
//! passendem SHA-256, Medientyp, Größe, `If-Match` beim Ersetzen/Löschen, Download, Löschschutz
//! bei verwendeten Modulen und Prüfung des Modulverweises in Plugin-Bindungen.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use aigw_admin::auth::{AuthMethod, BoxFuture};
use aigw_admin::{AdminAuthenticator, AdminState, Principal, router_with};
use aigw_core::resource::{Role, RoleBinding};
use aigw_store::Store;
use aigw_store::wasm::sha256_hex;
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;

const BASE: &str = "/admin/api/v1/wasm-modules";

struct Roles;

impl AdminAuthenticator for Roles {
    fn authenticate<'a>(&'a self, headers: &'a HeaderMap) -> BoxFuture<'a, Option<Principal>> {
        let token = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or_default()
            .to_owned();
        Box::pin(async move {
            let b = |role, org: Option<&str>| RoleBinding {
                role,
                organization: org.map(str::to_owned),
                team: None,
            };
            let roles = match token.as_str() {
                "admin" => vec![b(Role::Admin, None)],
                "org" => vec![b(Role::OrgAdmin, Some("acme"))],
                "viewer" => vec![b(Role::Viewer, None)],
                _ => return None,
            };
            Some(Principal {
                subject: format!("test:{token}"),
                roles,
                method: AuthMethod::AdminApiKey,
                display_name: None,
                email: None,
                expires_at: None,
                csrf: None,
            })
        })
    }
}

async fn app() -> (Router, Store) {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    store.migrate().await.unwrap();
    let state = AdminState::new(store.clone(), Arc::new(Roles)).with_wasm_max_module_bytes(1024);
    (router_with(state), store)
}

struct Resp {
    status: StatusCode,
    body: Value,
    etag: Option<String>,
    raw: Vec<u8>,
}

async fn call(
    app: &Router,
    token: &str,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    body: Body,
) -> Resp {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"));
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let res = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let etag = res
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let raw = res.into_body().collect().await.unwrap().to_bytes().to_vec();
    let body = serde_json::from_slice(&raw).unwrap_or(Value::Null);
    Resp {
        status,
        body,
        etag,
        raw,
    }
}

fn component(tail: &[u8]) -> Vec<u8> {
    let mut v = b"\0asm\x0d\0\x01\0".to_vec();
    v.extend_from_slice(tail);
    v
}

async fn upload(
    app: &Router,
    token: &str,
    name: &str,
    content: &[u8],
    sha: &str,
    if_match: Option<&str>,
) -> Resp {
    let mut h = vec![("content-type", "application/wasm")];
    if let Some(m) = if_match {
        h.push(("if-match", m));
    }
    call(
        app,
        token,
        Method::PUT,
        &format!("{BASE}/{name}?sha256={sha}&description=Test"),
        &h,
        Body::from(content.to_vec()),
    )
    .await
}

#[tokio::test]
async fn upload_rights_integrity_and_lifecycle() {
    let (app, store) = app().await;
    let v1 = component(b"eins");
    let sha1 = sha256_hex(&v1);

    // Nur globale Admins.
    for token in ["viewer", "org"] {
        let r = upload(&app, token, "guard", &v1, &sha1, None).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{token}");
    }
    // Lesen darf jede Rolle.
    let r = call(&app, "viewer", Method::GET, BASE, &[], Body::empty()).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body["items"], json!([]));

    // Falscher SHA, falscher Medientyp, zu groß, kein Komponenten-Format, SHA fehlt.
    let r = upload(&app, "admin", "guard", &v1, &"0".repeat(64), None).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        r.body["detail"]
            .as_str()
            .unwrap()
            .contains("sha256 stimmt nicht")
    );
    let r = call(
        &app,
        "admin",
        Method::PUT,
        &format!("{BASE}/guard?sha256={sha1}"),
        &[("content-type", "application/json")],
        Body::from(v1.clone()),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let big = component(&[0u8; 2048]);
    let r = upload(&app, "admin", "guard", &big, &sha256_hex(&big), None).await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
    let core = b"\0asm\x01\0\0\0".to_vec();
    let r = upload(&app, "admin", "guard", &core, &sha256_hex(&core), None).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let r = call(
        &app,
        "admin",
        Method::PUT,
        &format!("{BASE}/guard"),
        &[("content-type", "application/wasm")],
        Body::from(v1.clone()),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(store.config_version().await.unwrap(), 0);

    // Anlegen.
    let r = upload(&app, "admin", "guard", &v1, &sha1, None).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.body["sha256"], sha1);
    assert_eq!(r.body["size"], v1.len());
    assert_eq!(r.body["createdBy"], "test:admin");
    let etag = r.etag.unwrap();
    assert_eq!(etag, format!("\"{sha1}\""));

    // Ersetzen braucht If-Match.
    let v2 = component(b"zwei");
    let sha2 = sha256_hex(&v2);
    assert_eq!(
        upload(&app, "admin", "guard", &v2, &sha2, None)
            .await
            .status,
        StatusCode::PRECONDITION_REQUIRED
    );
    assert_eq!(
        upload(&app, "admin", "guard", &v2, &sha2, Some("\"falsch\""))
            .await
            .status,
        StatusCode::PRECONDITION_FAILED
    );
    let r = upload(&app, "admin", "guard", &v2, &sha2, Some(&etag)).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body["sha256"], sha2);

    // Download.
    let r = call(
        &app,
        "viewer",
        Method::GET,
        &format!("{BASE}/guard/content"),
        &[],
        Body::empty(),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.raw, v2);

    // Bindung: unbekanntes Modul bzw. falscher SHA → 422; passend → angelegt.
    let binding = |module: &str, sha: Option<&str>| {
        let mut config = json!({"module": module});
        if let Some(s) = sha {
            config["sha256"] = json!(s);
        }
        Body::from(
            json!({"metadata": {"name": "b"}, "spec": {"plugin": "wasm", "scope": {"level": "global"}, "config": config}})
                .to_string(),
        )
    };
    let post = |b| {
        call(
            &app,
            "admin",
            Method::POST,
            "/admin/api/v1/plugin-bindings",
            &[("content-type", "application/json")],
            b,
        )
    };
    assert_eq!(
        post(binding("fehlt", None)).await.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        post(binding("guard", Some(&sha1))).await.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let r = post(binding("guard", Some(&sha2))).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.body["effective"]["failMode"], "closed");

    // Liste zeigt die Verwendung; Löschen verwendeter Module → 409.
    let r = call(&app, "admin", Method::GET, BASE, &[], Body::empty()).await;
    assert_eq!(r.body["items"][0]["usedBy"], json!(["b"]));
    let r = call(
        &app,
        "admin",
        Method::DELETE,
        &format!("{BASE}/guard"),
        &[("if-match", "*")],
        Body::empty(),
    )
    .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    let r = call(
        &app,
        "admin",
        Method::DELETE,
        "/admin/api/v1/plugin-bindings/b",
        &[("if-match", "*")],
        Body::empty(),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(
        call(
            &app,
            "admin",
            Method::DELETE,
            &format!("{BASE}/guard"),
            &[],
            Body::empty()
        )
        .await
        .status,
        StatusCode::PRECONDITION_REQUIRED
    );
    assert_eq!(
        call(
            &app,
            "viewer",
            Method::DELETE,
            &format!("{BASE}/guard"),
            &[("if-match", "*")],
            Body::empty()
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    let r = call(
        &app,
        "admin",
        Method::DELETE,
        &format!("{BASE}/guard"),
        &[("if-match", "*")],
        Body::empty(),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(
        call(
            &app,
            "admin",
            Method::GET,
            &format!("{BASE}/guard"),
            &[],
            Body::empty()
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}
