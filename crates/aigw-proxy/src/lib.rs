// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Hot Path (Stub).
//!
//! Stellt die OpenAI-kompatiblen Endpunkte unter `/v1` bereit (bisher nur Platzhalter mit
//! `501 Not Implemented` im OpenAI-Fehlerformat) sowie den Liveness-Endpunkt `/healthz`.
//! Routing, Lastverteilung, Fallbacks und Streaming folgen mit dem Architekturkonzept.

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{any, get};
use serde_json::json;

/// Router des Hot Paths (`/healthz`, `/v1/*`).
pub fn router() -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/{*rest}", any(not_implemented))
}

/// Liveness: antwortet ohne Abhängigkeiten mit `200 ok`.
async fn healthz() -> &'static str {
    "ok"
}

async fn not_implemented() -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        axum::Json(json!({
            "error": {
                "message": "Endpunkt noch nicht umgesetzt",
                "type": "not_implemented",
                "code": "not_implemented",
            }
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn healthz_ok_and_v1_not_implemented() {
        let res = router()
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = res.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"ok");

        let res = router()
            .oneshot(
                Request::post("/v1/chat/completions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_IMPLEMENTED);
    }
}
