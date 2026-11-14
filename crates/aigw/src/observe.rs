// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Beobachtung starten: Request-Log (AIGW-43), Prometheus-Metriken (AIGW-44), OTLP-Traces und
//! -Logs (AIGW-45); `/metrics` der Steuerebene.

use std::sync::Arc;
use std::time::Duration;

use aigw_core::Config;
use aigw_core::snapshot::SnapshotHandle;
use aigw_observe::otel::{OtlpLogExporter, OtlpMetrics, TraceActivator, TraceExporter};
use aigw_observe::{GatewayMetrics, ObserveBuilder, ObserveHandle, ObserveMetrics};
use aigw_store::{ReloadMetrics, Store};
use axum::Router;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;

/// Laufende Beobachtung.
#[derive(Debug)]
pub(crate) struct Observe {
    pub(crate) log: ObserveHandle,
    pub(crate) otlp_metrics: Arc<OtlpMetrics>,
    pub(crate) traces: Option<TraceExporter>,
}

impl Observe {
    /// Exportiert gepufferte Spans und leert das Request-Log.
    pub(crate) async fn shutdown(self) -> bool {
        let traces_ok = match self.traces {
            Some(t) => t.shutdown(Duration::from_secs(5)).await,
            None => true,
        };
        let log_ok = self.log.shutdown().await;
        traces_ok && log_ok
    }
}

/// Startet Request-Log, Metriken und – falls konfiguriert – den OTLP-Export.
pub(crate) fn start(
    config: &Config,
    store: &Store,
    traces: &TraceActivator,
) -> Result<Observe, Box<dyn std::error::Error>> {
    let otlp = &config.observe.otlp;
    let otlp_metrics = Arc::new(OtlpMetrics::default());
    let mut builder = ObserveBuilder::new(config.observe.clone()).store(store.clone());
    if otlp.logs_active() {
        builder = builder.exporter(Arc::new(OtlpLogExporter::new(
            otlp,
            Arc::clone(&otlp_metrics),
        )?));
    }
    let traces = if otlp.traces_active() {
        Some(traces.activate(otlp, Arc::clone(&otlp_metrics))?)
    } else {
        None
    };
    let log = builder.start();
    tracing::info!(
        payload = config.observe.payload.as_str(),
        request_log = config.observe.request_log,
        jsonl = config.observe.jsonl.enabled,
        metrics = config.observe.metrics.enabled,
        otlp_traces = otlp.traces_active(),
        otlp_logs = otlp.logs_active(),
        otlp_endpoint = %otlp.endpoint,
        sampling_ratio = otlp.traces.sampling_ratio.get(),
        "Beobachtung gestartet"
    );
    Ok(Observe {
        log,
        otlp_metrics,
        traces,
    })
}

/// Quellen für `/metrics`.
#[derive(Debug, Clone)]
pub(crate) struct MetricsSources {
    pub(crate) log: Arc<ObserveMetrics>,
    pub(crate) gateway: Option<Arc<GatewayMetrics>>,
    pub(crate) otlp: Arc<OtlpMetrics>,
    pub(crate) snapshot: SnapshotHandle,
    pub(crate) reload: Arc<ReloadMetrics>,
    /// Ereignis-Webhooks (AIGW-61), falls konfiguriert.
    pub(crate) events: Option<Arc<aigw_webhook::EventMetrics>>,
    /// Antwort-Cache (AIGW-39/40).
    pub(crate) cache: Option<Arc<aigw_cache::ResponseCache>>,
    /// Plugin-Kette: `aigw_plugin_failmode_triggered_total` (F148).
    pub(crate) plugins: Option<Arc<aigw_plugins::PluginMetrics>>,
    /// Verbindungen der Listener: `aigw_server_*` (S-32, AIGW-80).
    pub(crate) server: Option<ServerMetricsSource>,
    /// Tool-Richtlinien und Tool-Inventar (AIGW-87).
    pub(crate) tools: Option<Arc<aigw_proxy::tools::ToolMetrics>>,
    /// WASM-Plugins: `aigw_wasm_*` (AIGW-63).
    #[cfg(feature = "wasm")]
    pub(crate) wasm: Option<Arc<aigw_wasm::WasmMetrics>>,
}

/// Verbindungsmetriken der Listener für `/metrics`.
#[derive(Debug, Clone)]
pub(crate) struct ServerMetricsSource {
    pub(crate) metrics: Arc<crate::serve::ServerMetrics>,
    /// Getrennter Steuer-Listener (sonst nur `listener="data"`).
    pub(crate) with_control: bool,
}

impl MetricsSources {
    fn render(&self) -> String {
        let mut out = String::with_capacity(16 * 1024);
        if let Some(g) = &self.gateway {
            g.render_prometheus(&mut out);
        }
        aigw_observe::prom::render_snapshot(&mut out, self.snapshot.version(), &self.reload);
        self.log.render_prometheus(&mut out);
        self.otlp.render_prometheus(&mut out);
        if let Some(e) = &self.events {
            e.render_prometheus(&mut out);
        }
        if let Some(c) = &self.cache {
            c.render_prometheus(&mut out);
        }
        if let Some(p) = &self.plugins {
            p.render_prometheus(&mut out);
        }
        if let Some(s) = &self.server {
            s.metrics.render_prometheus(&mut out, s.with_control);
        }
        if let Some(t) = &self.tools {
            t.render_prometheus(&mut out);
        }
        #[cfg(feature = "wasm")]
        if let Some(w) = &self.wasm {
            w.render_prometheus(&mut out);
        }
        out
    }
}

/// Router mit `GET /metrics` (Prometheus-Textformat 0.0.4) für die Steuerebene.
pub(crate) fn metrics_router(sources: MetricsSources) -> Router {
    let sources = Arc::new(sources);
    Router::new().route(
        "/metrics",
        get(move || {
            let sources = Arc::clone(&sources);
            async move {
                (
                    [(
                        header::CONTENT_TYPE,
                        "text/plain; version=0.0.4; charset=utf-8",
                    )],
                    sources.render(),
                )
                    .into_response()
            }
        }),
    )
}
