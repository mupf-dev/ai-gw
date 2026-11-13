// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Zähler des Request-Logs (für `/metrics`, Prometheus-Textformat).

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

/// Zähler des Request-Logs. Im Hot Path wird nur `dropped` erhöht (und nur im Fehlerfall).
#[derive(Debug, Default)]
pub struct ObserveMetrics {
    pub(crate) dropped: AtomicU64,
    pub(crate) written: AtomicU64,
    pub(crate) write_failed: AtomicU64,
    pub(crate) batches: AtomicU64,
    pub(crate) export_errors: AtomicU64,
    pub(crate) purged: AtomicU64,
}

/// Momentaufnahme der Zähler.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MetricsSnapshot {
    /// Verworfen, weil der Puffer voll (oder geschlossen) war: `aigw_log_dropped_total`.
    pub dropped: u64,
    /// Verarbeitete Einträge (in DB bzw. Exporter geschrieben): `aigw_log_written_total`.
    pub written: u64,
    /// Verloren, weil die DB auch nach Wiederholungen nicht schreibbar war.
    pub write_failed: u64,
    /// Geschriebene Batches.
    pub batches: u64,
    /// Fehlgeschlagene Exporter-Aufrufe.
    pub export_errors: u64,
    /// Durch Aufbewahrungsfristen gelöschte Zeilen.
    pub purged: u64,
}

impl ObserveMetrics {
    /// Liest alle Zähler.
    pub fn snapshot(&self) -> MetricsSnapshot {
        let r = |c: &AtomicU64| c.load(Ordering::Relaxed);
        MetricsSnapshot {
            dropped: r(&self.dropped),
            written: r(&self.written),
            write_failed: r(&self.write_failed),
            batches: r(&self.batches),
            export_errors: r(&self.export_errors),
            purged: r(&self.purged),
        }
    }

    pub(crate) fn add(counter: &AtomicU64, n: impl TryInto<u64>) {
        counter.fetch_add(n.try_into().unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    /// Hängt die Zähler im Prometheus-Textformat an `out` an.
    pub fn render_prometheus(&self, out: &mut String) {
        let s = self.snapshot();
        for (name, help, value) in [
            (
                "aigw_log_dropped_total",
                "Request-Log-Einträge verworfen (Puffer voll oder geschlossen)",
                s.dropped,
            ),
            (
                "aigw_log_written_total",
                "Request-Log-Einträge geschrieben",
                s.written,
            ),
            (
                "aigw_log_write_failed_total",
                "Request-Log-Einträge verloren (Datenbank nicht schreibbar)",
                s.write_failed,
            ),
            (
                "aigw_log_batches_total",
                "Geschriebene Request-Log-Batches",
                s.batches,
            ),
            (
                "aigw_log_export_errors_total",
                "Fehlgeschlagene Exporte des Request-Logs",
                s.export_errors,
            ),
            (
                "aigw_log_purged_total",
                "Durch Aufbewahrungsfristen gelöschte Zeilen",
                s.purged,
            ),
        ] {
            let _ = writeln!(
                out,
                "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prometheus_text() {
        let m = ObserveMetrics::default();
        m.dropped.fetch_add(3, Ordering::Relaxed);
        let mut out = String::new();
        m.render_prometheus(&mut out);
        assert!(out.contains("# TYPE aigw_log_dropped_total counter\naigw_log_dropped_total 3\n"));
        assert!(out.contains("aigw_log_written_total 0\n"));
    }
}
