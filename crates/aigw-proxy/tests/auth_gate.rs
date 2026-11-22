// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! AIGW-58 (Sicherheitsreview): Ohne gültigen Schlüssel liest die Datenebene den Request-Body
//! nicht ein (kein Speicher-DoS durch anonyme Anfragen bis `max_body_bytes`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aigw_core::ir::{ApiError, ErrorKind};
use aigw_proxy::{ApiKey, AuthContext, Authenticator, ProxyState};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use tower::ServiceExt as _;

/// Lässt nur den Schlüssel `good` zu.
struct OnlyGood;

impl Authenticator for OnlyGood {
    fn authenticate(&self, key: Option<&ApiKey>) -> Result<AuthContext, ApiError> {
        match key {
            Some(k) if k.secret == "good" => Ok(AuthContext::default()),
            _ => Err(ApiError::new(ErrorKind::Authentication, "invalid_api_key")),
        }
    }
}

/// Body, der vermerkt, ob er gelesen wurde.
fn tracked_body(read: Arc<AtomicBool>) -> Body {
    let chunk =
        Bytes::from_static(br#"{"model":"mock","messages":[{"role":"user","content":"hi"}]}"#);
    Body::from_stream(futures_util::stream::once(async move {
        read.store(true, Ordering::SeqCst);
        Ok::<_, std::io::Error>(chunk)
    }))
}

async fn send(path: &str, key: Option<&str>) -> (StatusCode, bool) {
    let mut st = ProxyState::mock();
    st.auth = Arc::new(OnlyGood);
    let app = aigw_proxy::router(st);
    let read = Arc::new(AtomicBool::new(false));
    let mut req = Request::post(path).header("content-type", "application/json");
    if let Some(k) = key {
        req = req.header("authorization", format!("Bearer {k}"));
    }
    let res = app
        .oneshot(req.body(tracked_body(Arc::clone(&read))).unwrap())
        .await
        .unwrap();
    (res.status(), read.load(Ordering::SeqCst))
}

#[tokio::test]
async fn body_is_not_read_without_valid_key() {
    for path in ["/v1/chat/completions", "/v1/messages", "/v1/embeddings"] {
        for key in [None, Some("bad")] {
            let (status, read) = send(path, key).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{path} {key:?}");
            assert!(!read, "{path} {key:?}: Body wurde gelesen");
        }
    }
    let (status, read) = send("/v1/chat/completions", Some("good")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(read);
}
