// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Fehlerantworten im Format des Eingangs.

use aigw_core::ir::ApiError;
use aigw_core::wire::{anthropic, openai};
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;

/// Fehlerformat (= Eingangsformat).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorFormat {
    /// `{"error":{"message","type","param","code"}}`.
    OpenAi,
    /// `{"type":"error","error":{"type","message"},"request_id"}`.
    Anthropic,
}

impl ErrorFormat {
    /// Format für Pfade ohne eindeutigen Endpunkt: Anthropic bei `/v1/messages…` oder
    /// gesetztem `anthropic-version`, sonst OpenAI.
    pub fn detect(path: &str, headers: &HeaderMap) -> Self {
        if path.starts_with("/v1/messages") || headers.contains_key("anthropic-version") {
            Self::Anthropic
        } else {
            Self::OpenAi
        }
    }

    /// Baut die Fehlerantwort.
    pub fn response(self, err: &ApiError, request_id: Option<&str>) -> Response {
        let (status, body) = match self {
            Self::OpenAi => (err.status(), serde_json::to_vec(&openai::encode_error(err))),
            Self::Anthropic => (
                // F152: exakter Status vor dem der Klasse.
                err.exact_status
                    .unwrap_or_else(|| anthropic::error_status(err.kind)),
                serde_json::to_vec(&anthropic::encode_error(err, request_id)),
            ),
        };
        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = body.unwrap_or_else(|_| b"{}".to_vec());
        let mut res = Response::new(Body::from(body));
        *res.status_mut() = status;
        res.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        res
    }
}
