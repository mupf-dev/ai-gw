// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! `GET /admin/api/v1/usage/limit-warnings` (F29): Sekundenlimits mit `postgres-sync`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use aigw_admin::AdminState;
use aigw_admin::usage::{UsageState, router};
use aigw_core::config::{AdminAuthConfig, CounterBackendKind};
use aigw_core::resource::{
    LimitMetric, LimitSpec, LimitWindow, ModelGroupSpec, ObjectMeta, OrganizationSpec, TeamSpec,
};
use aigw_store::Store;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::Value;

fn lim(window: LimitWindow) -> LimitSpec {
    LimitSpec {
        metric: LimitMetric::Requests,
        limit: 10,
        window,
        model_group: None,
    }
}

async fn get(app: axum::Router, token: &str) -> (StatusCode, Value) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let res = client
        .request(
            Request::get(format!("http://{addr}/admin/api/v1/usage/limit-warnings"))
                .header("authorization", format!("Bearer {token}"))
                .body(Full::new(Bytes::new()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

#[tokio::test]
async fn second_limits_with_postgres_sync_are_reported() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    store.migrate().await.unwrap();
    store
        .create(
            "test",
            ObjectMeta::named("acme"),
            OrganizationSpec {
                limits: vec![lim(LimitWindow::Second), lim(LimitWindow::Minute)],
                ..OrganizationSpec::default()
            },
        )
        .await
        .unwrap();
    store
        .create(
            "test",
            ObjectMeta::named("plat"),
            TeamSpec {
                organization: "acme".into(),
                limits: vec![lim(LimitWindow::CalendarDay), lim(LimitWindow::Second)],
                ..TeamSpec::default()
            },
        )
        .await
        .unwrap();
    store
        .create(
            "test",
            ObjectMeta::named("gpt"),
            ModelGroupSpec {
                limits: vec![lim(LimitWindow::Second)],
                ..ModelGroupSpec::default()
            },
        )
        .await
        .unwrap();
    let (token, secret) = aigw_admin::BootstrapToken::generate().unwrap();
    let state = AdminState::from_config(
        store.clone(),
        &AdminAuthConfig::default(),
        Some(token),
        None,
    )
    .unwrap();
    let mk = |counters| {
        router(UsageState {
            store: store.clone(),
            calendar: Arc::new(aigw_billing::Calendar::utc()),
            auth: Arc::clone(&state.auth),
            counters,
        })
    };

    let (s, v) = get(mk(CounterBackendKind::PostgresSync), &secret).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["counterBackend"], "postgres-sync");
    let w: Vec<(String, String, String)> = v["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| {
            (
                w["kind"].as_str().unwrap().to_owned(),
                w["name"].as_str().unwrap().to_owned(),
                w["field"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        w,
        [
            ("organization".into(), "acme".into(), "limits[0]".into()),
            ("team".into(), "plat".into(), "limits[1]".into()),
            ("model-group".into(), "gpt".into(), "limits[0]".into()),
        ]
    );
    assert_eq!(v["warnings"][0]["code"], "second-limit-postgres-sync");

    // Mit redis bzw. local wirkt das Limit exakt (je Instanz bzw. im Cluster): keine Hinweise.
    for k in [CounterBackendKind::Redis, CounterBackendKind::Local] {
        let (s, v) = get(mk(k), &secret).await;
        assert_eq!(s, StatusCode::OK);
        assert!(v["warnings"].as_array().unwrap().is_empty());
    }
    // ohne Anmeldung
    let (s, _) = get(mk(CounterBackendKind::PostgresSync), "falsch").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}
