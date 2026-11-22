// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! OpenTelemetry-Export über OTLP/HTTP mit Protobuf (AIGW-45), ohne OpenTelemetry-SDK.
//!
//! - **Traces:** [`layer`] liefert einen `tracing`-Layer, der Spans mit
//!   `target: "aigw::otel"` ([`aigw_core::observe::TRACE_TARGET`]) aufzeichnet; der Export
//!   startet mit [`TraceActivator::activate`].
//! - **Logs:** [`OtlpLogExporter`] implementiert [`crate::LogExporter`] und sendet das
//!   aufbereitete Request-Log als OTLP-Logs.
//!
//! Begründung (eigene Umsetzung statt `opentelemetry`/`tracing-opentelemetry`/
//! `opentelemetry-otlp`) und Wire-Format: `docs/konzept/07-beobachtung.md` §12.

mod http;
mod logs;
mod proto;
mod trace;

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

use aigw_core::config::OtlpConfig;

pub use http::OtlpConfigError;
pub use logs::OtlpLogExporter;
pub use trace::{OtelFilter, OtelLayer, TraceActivator, TraceExporter, layer, test_subscriber};

use proto::{KeyValue, Origin};

/// Zähler des OTLP-Exports (`/metrics`).
#[derive(Debug, Default)]
pub struct OtlpMetrics {
    pub(crate) spans_exported: AtomicU64,
    pub(crate) spans_dropped: AtomicU64,
    pub(crate) trace_export_errors: AtomicU64,
    pub(crate) logs_exported: AtomicU64,
    pub(crate) log_export_errors: AtomicU64,
    pub(crate) trace_export_retries: AtomicU64,
    pub(crate) log_export_retries: AtomicU64,
}

impl OtlpMetrics {
    /// Exportierte Spans.
    pub fn spans_exported(&self) -> u64 {
        self.spans_exported.load(Ordering::Relaxed)
    }

    /// Verworfene Spans (Puffer voll oder Export fehlgeschlagen).
    pub fn spans_dropped(&self) -> u64 {
        self.spans_dropped.load(Ordering::Relaxed)
    }

    /// Exportierte Log-Einträge.
    pub fn logs_exported(&self) -> u64 {
        self.logs_exported.load(Ordering::Relaxed)
    }

    /// Hängt die Zähler im Prometheus-Textformat an `out` an.
    pub fn render_prometheus(&self, out: &mut String) {
        let r = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let _ = writeln!(
            out,
            "# HELP aigw_otlp_spans_exported_total An den Collector übergebene Spans\n\
             # TYPE aigw_otlp_spans_exported_total counter\n\
             aigw_otlp_spans_exported_total {}\n\
             # HELP aigw_otlp_spans_dropped_total Verworfene Spans (Puffer voll oder Export fehlgeschlagen)\n\
             # TYPE aigw_otlp_spans_dropped_total counter\n\
             aigw_otlp_spans_dropped_total {}\n\
             # HELP aigw_otlp_logs_exported_total An den Collector übergebene Log-Einträge\n\
             # TYPE aigw_otlp_logs_exported_total counter\n\
             aigw_otlp_logs_exported_total {}\n\
             # HELP aigw_otlp_export_errors_total Fehlgeschlagene OTLP-Exporte je Signal\n\
             # TYPE aigw_otlp_export_errors_total counter\n\
             aigw_otlp_export_errors_total{{signal=\"traces\"}} {}\n\
             aigw_otlp_export_errors_total{{signal=\"logs\"}} {}\n\
             # HELP aigw_otlp_export_retries_total Wiederholte OTLP-Exporte je Signal (429/5xx/Verbindung)\n\
             # TYPE aigw_otlp_export_retries_total counter\n\
             aigw_otlp_export_retries_total{{signal=\"traces\"}} {}\n\
             aigw_otlp_export_retries_total{{signal=\"logs\"}} {}",
            r(&self.spans_exported),
            r(&self.spans_dropped),
            r(&self.logs_exported),
            r(&self.trace_export_errors),
            r(&self.log_export_errors),
            r(&self.trace_export_retries),
            r(&self.log_export_retries),
        );
    }
}

/// Ressource (`service.name` …) und Instrumentierungs-Bereich.
pub(crate) fn origin(cfg: &OtlpConfig) -> Origin {
    Origin {
        resource: vec![
            KeyValue::str("service.name", cfg.service_name.clone()),
            KeyValue::str("service.version", aigw_core::VERSION),
            KeyValue::str("telemetry.sdk.name", "aigw"),
            KeyValue::str("telemetry.sdk.language", "rust"),
            KeyValue::str("telemetry.sdk.version", aigw_core::VERSION),
        ],
        scope_name: "aigw".to_owned(),
        scope_version: aigw_core::VERSION.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prometheus_text() {
        let m = OtlpMetrics::default();
        m.spans_dropped.fetch_add(2, Ordering::Relaxed);
        let mut out = String::new();
        m.render_prometheus(&mut out);
        assert!(out.contains("aigw_otlp_spans_dropped_total 2\n"));
        assert!(out.contains("aigw_otlp_export_errors_total{signal=\"logs\"} 0\n"));
    }
}
