// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Request-Log starten (AIGW-43) und `/metrics` der Steuerebene.

use std::sync::Arc;

use aigw_core::Config;
use aigw_observe::{ObserveBuilder, ObserveHandle, ObserveMetrics};
use aigw_store::Store;
use axum::Router;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;

/// Startet Writer und Aufbewahrung des Request-Logs (Abschnitt `observe`).
pub(crate) fn start(config: &Config, store: &Store) -> ObserveHandle {
    let handle = ObserveBuilder::new(config.observe.clone())
        .store(store.clone())
        .start();
    tracing::info!(
        payload = config.observe.payload.as_str(),
        request_log = config.observe.request_log,
        jsonl = config.observe.jsonl.enabled,
        "Request-Log gestartet"
    );
    handle
}

/// Router mit `GET /metrics` (Prometheus-Textformat) für die Steuerebene.
pub(crate) fn metrics_router(metrics: Arc<ObserveMetrics>) -> Router {
    Router::new().route(
        "/metrics",
        get(move || {
            let metrics = Arc::clone(&metrics);
            async move {
                let mut out = String::new();
                metrics.render_prometheus(&mut out);
                (
                    [(
                        header::CONTENT_TYPE,
                        "text/plain; version=0.0.4; charset=utf-8",
                    )],
                    out,
                )
                    .into_response()
            }
        }),
    )
}
