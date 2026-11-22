// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Request-ID (UUIDv7) und Annahmezeitpunkt für jede Anfrage der Datenebene.

use axum::extract::Request;
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::Response;

/// Name des Antwort-Headers.
pub const REQUEST_ID_HEADER: &str = "x-aigw-request-id";

/// Request-ID als Request-Extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestId(pub String);

impl RequestId {
    /// Neue, zeitlich sortierbare ID.
    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7().to_string())
    }
}

impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

/// Annahmezeitpunkt einer Anfrage (vor dem Lesen des Bodys) als Request-Extension.
#[derive(Debug, Clone, Copy)]
pub struct RequestStart {
    /// Monotone Uhr (für Dauer und TTFT).
    pub instant: std::time::Instant,
    /// Wanduhr (für das Request-Log).
    pub at: std::time::SystemTime,
}

impl RequestStart {
    /// Jetzt.
    pub fn now() -> Self {
        Self {
            instant: std::time::Instant::now(),
            at: std::time::SystemTime::now(),
        }
    }
}

/// Middleware: erzeugt die ID und den Annahmezeitpunkt, legt beide als Extension ab und setzt
/// den Antwort-Header.
///
/// Eine vom Client mitgeschickte ID wird bewusst nicht übernommen (nicht vertrauenswürdig).
pub(crate) async fn middleware(mut req: Request, next: Next) -> Response {
    req.extensions_mut().insert(RequestStart::now());
    let id = RequestId::new();
    let header = HeaderValue::from_str(&id.0).ok();
    req.extensions_mut().insert(id);
    let mut res = next.run(req).await;
    if let Some(h) = header {
        res.headers_mut().insert(REQUEST_ID_HEADER, h);
    }
    res
}
