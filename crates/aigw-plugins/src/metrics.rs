// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Metriken der Plugin-Kette (F148): wie oft der Fehlermodus einer Bindung gegriffen hat.
//!
//! `aigw_plugin_failmode_triggered_total{plugin,mode}` zählt jeden Plugin-Fehler
//! (Laufzeitfehler, Zeitüberschreitung, verbotene Änderung), bei dem der Fehlermodus
//! angewendet wurde: `mode="open"` = übersprungen, `mode="closed"` = Anfrage abgelehnt bzw.
//! Stream beendet. Serien entstehen erst beim ersten Fehler; `plugin` ist der Plugin-Name
//! (begrenzt durch die Registry). Ungültige Bindungen zählen hier nicht (sie fallen schon beim
//! Schreiben bzw. im Log des Snapshot-Aufbaus auf).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Mutex, PoisonError};

use crate::binding::FailMode;

/// Zähler der Plugin-Kette; von allen Ketten einer Registry geteilt.
#[derive(Debug, Default)]
pub struct PluginMetrics {
    failmode: Mutex<BTreeMap<(&'static str, FailMode), u64>>,
}

impl PluginMetrics {
    /// Zählt einen Plugin-Fehler, für den `mode` angewendet wurde.
    pub fn failmode_triggered(&self, plugin: &'static str, mode: FailMode) {
        let mut m = self.failmode.lock().unwrap_or_else(PoisonError::into_inner);
        *m.entry((plugin, mode)).or_default() += 1;
    }

    /// Stand eines Zählers.
    pub fn failmode_count(&self, plugin: &str, mode: FailMode) -> u64 {
        let m = self.failmode.lock().unwrap_or_else(PoisonError::into_inner);
        m.iter()
            .find(|((p, md), _)| *p == plugin && *md == mode)
            .map_or(0, |(_, n)| *n)
    }

    /// Prometheus-Textformat (0.0.4).
    pub fn render_prometheus(&self, out: &mut String) {
        let m = self.failmode.lock().unwrap_or_else(PoisonError::into_inner);
        out.push_str(
            "# HELP aigw_plugin_failmode_triggered_total Plugin-Fehler, bei denen der Fehlermodus der Bindung gegriffen hat (open = übersprungen, closed = abgelehnt)\n\
             # TYPE aigw_plugin_failmode_triggered_total counter\n",
        );
        for ((plugin, mode), n) in m.iter() {
            let _ = writeln!(
                out,
                "aigw_plugin_failmode_triggered_total{{plugin=\"{plugin}\",mode=\"{}\"}} {n}",
                mode.as_str()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_renders() {
        let m = PluginMetrics::default();
        let mut out = String::new();
        m.render_prometheus(&mut out);
        assert!(out.contains("# TYPE aigw_plugin_failmode_triggered_total counter"));
        assert!(!out.contains("{plugin="));
        m.failmode_triggered("webhook", FailMode::Open);
        m.failmode_triggered("webhook", FailMode::Open);
        m.failmode_triggered("content-safety", FailMode::Closed);
        assert_eq!(m.failmode_count("webhook", FailMode::Open), 2);
        assert_eq!(m.failmode_count("webhook", FailMode::Closed), 0);
        let mut out = String::new();
        m.render_prometheus(&mut out);
        assert!(out.contains(
            "aigw_plugin_failmode_triggered_total{plugin=\"webhook\",mode=\"open\"} 2\n"
        ));
        assert!(out.contains(
            "aigw_plugin_failmode_triggered_total{plugin=\"content-safety\",mode=\"closed\"} 1\n"
        ));
    }
}
