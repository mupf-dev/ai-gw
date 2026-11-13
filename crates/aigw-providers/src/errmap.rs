// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Gemeinsame Aufbereitung klassifizierter Upstream-Fehler für Bedrock und Vertex
//! (gleiche Regeln wie `openai::classify_openai_error`, siehe
//! docs/konzept/06-streaming-und-anbieter.md §6).

use std::time::Duration;

use aigw_core::ir::{ApiError, ErrorClass, ErrorKind, UpstreamError};
use http::HeaderMap;

/// Längste weitergegebene Fehlermeldung des Anbieters (Zeichen).
const MAX_ERROR_MESSAGE: usize = 1000;

/// Kürzt Meldungen des Anbieters.
pub(crate) fn truncate(mut s: String) -> String {
    if s.chars().count() > MAX_ERROR_MESSAGE {
        s = s.chars().take(MAX_ERROR_MESSAGE).collect();
        s.push('…');
    }
    s
}

/// `Retry-After` (Sekunden) bzw. `retry-after-ms`.
pub(crate) fn retry_after_header(headers: &HeaderMap) -> Option<Duration> {
    let num = |name: &str| {
        headers
            .get(name)?
            .to_str()
            .ok()?
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite() && *v >= 0.0)
    };
    if let Some(ms) = num("retry-after-ms") {
        return Some(Duration::from_secs_f64(ms / 1000.0));
    }
    num("retry-after").map(Duration::from_secs_f64)
}

/// Grobe Klasse aus dem Status (wenn der Anbieter keinen genaueren Typ liefert).
pub(crate) fn class_from_status(status: u16) -> ErrorClass {
    match status {
        429 => ErrorClass::RateLimit,
        401 | 403 => ErrorClass::Auth,
        404 => ErrorClass::NotFound,
        408 | 504 => ErrorClass::Timeout,
        409 => ErrorClass::ServerError,
        400..=499 => ErrorClass::InvalidRequest,
        503 | 529 => ErrorClass::Overloaded,
        500..=599 => ErrorClass::ServerError,
        _ => ErrorClass::Protocol,
    }
}

/// Typische Formulierungen für „Kontext zu lang“ (Bedrock, Gemini, Modelle hinter Bedrock).
pub(crate) fn looks_like_context_length(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    [
        "context length",
        "context window",
        "maximum context",
        "input is too long",
        "prompt is too long",
        "too many input tokens",
        "too many tokens",
        "input token count",
        "exceeds the maximum number of tokens",
        "maximum number of tokens allowed",
        "context_length_exceeded",
    ]
    .iter()
    .any(|p| m.contains(p))
}

/// Baut den [`UpstreamError`] mit den Regeln für den Client: Zugangsfehler des Gateways werden
/// 502 ohne Meldung des Anbieters, unbekanntes Modell 502, Kontextlänge/Inhaltsfilter 400 mit
/// festem Code.
pub(crate) fn upstream_error(
    class: ErrorClass,
    status: u16,
    message: String,
    code: Option<String>,
) -> UpstreamError {
    let message = if message.trim().is_empty() {
        format!("Anbieter meldet HTTP {status}")
    } else {
        truncate(message)
    };
    let mut api = ApiError {
        kind: ErrorKind::from_status(status),
        message,
        param: None,
        code,
    };
    match class {
        ErrorClass::Auth => {
            api = ApiError::new(
                ErrorKind::BadGateway,
                format!("Anbieter lehnt die Zugangsdaten des Gateways ab (HTTP {status})"),
            )
            .with_code("upstream_authentication_error");
        }
        ErrorClass::NotFound => {
            api.kind = ErrorKind::BadGateway;
            api.message = format!("Anbieter kennt Modell/Deployment nicht: {}", api.message);
            api.code = Some("upstream_not_found".to_owned());
        }
        ErrorClass::ContextLength => {
            api.kind = ErrorKind::InvalidRequest;
            api.code = Some("context_length_exceeded".to_owned());
        }
        ErrorClass::ContentFilter => {
            api.kind = ErrorKind::InvalidRequest;
            api.code = Some("content_filter".to_owned());
        }
        ErrorClass::RateLimit => api.kind = ErrorKind::RateLimit,
        ErrorClass::Quota => {
            api.kind = ErrorKind::RateLimit;
            api.code
                .get_or_insert_with(|| "insufficient_quota".to_owned());
        }
        ErrorClass::Overloaded => api.kind = ErrorKind::Overloaded,
        ErrorClass::Timeout => {
            api.kind = ErrorKind::Timeout;
            api.code
                .get_or_insert_with(|| "upstream_timeout".to_owned());
        }
        ErrorClass::Protocol => {
            api.kind = ErrorKind::BadGateway;
            api.code = Some("upstream_protocol_error".to_owned());
        }
        _ => {}
    }
    UpstreamError::new(class, api).with_status(status)
}

/// Fehler „Antwort/Stream nicht lesbar“.
pub(crate) fn protocol_error(what: impl std::fmt::Display) -> ApiError {
    ApiError::new(
        ErrorKind::BadGateway,
        format!("Anbieter-Antwort nicht lesbar: {what}"),
    )
    .with_code("upstream_protocol_error")
}

/// Stream endete ohne Abschluss.
pub(crate) fn truncated_error() -> ApiError {
    ApiError::new(
        ErrorKind::BadGateway,
        "Anbieter-Stream endete unerwartet (ohne Abschluss)",
    )
    .with_code("upstream_stream_truncated")
}

/// Interner Fehler beim Bau der Anfrage.
pub(crate) fn internal(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(ErrorKind::Internal, e.to_string())
}
