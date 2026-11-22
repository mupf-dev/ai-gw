// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Playground der Admin-UI (AIGW-54): Chat im Namen eines bestehenden virtuellen Keys.
//!
//! Die Admin-API (`POST /admin/api/v1/playground/chat`) prüft Rolle und Geltungsbereich und
//! wählt den Key; hier läuft die Anfrage **durch dieselbe Pipeline** wie
//! `POST /v1/chat/completions` – nur die Prüfung des Key-Headers entfällt, weil der Kontext
//! vorgegeben ist ([`AuthContext`] des Keys aus dem aktuellen Snapshot). Modellfreigaben,
//! Limits, Budgets, Plugins/Guardrails, Cache, Routing, Kostenabrechnung und Request-Log
//! gelten unverändert für diesen Key. Die Admin-Anmeldung umgeht damit keine Regel der
//! Datenebene.

use aigw_core::snapshot::Snapshot;
use axum::http::{HeaderMap, HeaderName, HeaderValue, header};
use axum::response::Response;
use bytes::Bytes;
use time::OffsetDateTime;

use crate::ProxyState;
use crate::auth::AuthContext;
use crate::error::ErrorFormat;
use crate::request_id::{REQUEST_ID_HEADER, RequestId, RequestStart};

/// Client-Request-ID der Playground-Anfragen im Request-Log (Suche im Log-Explorer).
pub const CLIENT_REQUEST_ID: &str = "playground";

/// Zustand der Datenebene für den Playground: wie `state`, aber immer mit Routing-Headern
/// (`x-aigw-deployment`, `x-aigw-model` …), die die UI anzeigt.
pub fn state_for(state: &ProxyState) -> ProxyState {
    let mut st = state.clone();
    st.config.routing_headers = true;
    st
}

/// Key-Kontext eines **aktiven**, nicht abgelaufenen Keys (Name) im Snapshot `snap`.
pub fn key_context(
    snap: std::sync::Arc<Snapshot>,
    name: &str,
    now: OffsetDateTime,
) -> Option<AuthContext> {
    let ctx = snap
        .keys
        .values()
        .find(|k| k.key.metadata.name == name)?
        .clone();
    if !ctx.is_valid_at(now) {
        return None;
    }
    Some(AuthContext {
        key_id: Some(ctx.key.id.to_string()),
        key: Some(ctx),
        snapshot: Some(snap),
        workload: None,
    })
}

/// Führt einen Chat-Request (OpenAI-Format, `body` wie bei `/v1/chat/completions`) im
/// Kontext `auth` unter der Request-ID `rid` aus (vom Aufrufer vergeben, damit er sie vorab
/// ins Audit-Log schreiben kann). Antwort wie auf der Datenebene (JSON oder SSE) mit
/// Request-ID-Header.
pub async fn chat_completions(
    state: &ProxyState,
    rid: RequestId,
    auth: AuthContext,
    body: Bytes,
) -> Response {
    let started = RequestStart::now();
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        HeaderName::from_static("x-request-id"),
        HeaderValue::from_static(CLIENT_REQUEST_ID),
    );
    let mut res = crate::handlers::chat(
        state,
        &rid,
        started,
        &headers,
        Ok(body),
        ErrorFormat::OpenAi,
        Some(auth),
    )
    .await;
    if let Ok(v) = HeaderValue::from_str(&rid.0) {
        res.headers_mut().insert(REQUEST_ID_HEADER, v);
    }
    res
}
