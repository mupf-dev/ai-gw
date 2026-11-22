// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Metriken der WASM-Plugins (AIGW-63).
//!
//! - `aigw_wasm_calls_total{module,phase,outcome}` – Aufrufe je Ergebnis (`pass`, `modify`,
//!   `block`, `reject`, `respond`, `drop`, `error`).
//! - `aigw_wasm_errors_total{module,kind}` – Fehler (`fuel`, `timeout`, `memory`, `trap`,
//!   `guest`, `output`, `instantiate`); was daraus wird, entscheidet `failMode`
//!   (`aigw_plugin_failmode_triggered_total{plugin="wasm"}`).
//! - `aigw_wasm_call_duration_seconds{module,phase}` – Dauer je Aufruf (Instanz + Aufruf).
//! - `aigw_wasm_fuel_consumed_total{module}` – verbrauchtes Fuel.
//! - `aigw_wasm_compile_total{outcome}` – Kompilierungen (`ok`, `error`, `cached`).
//! - `aigw_wasm_signature_rejected_total{reason}` – beim Aufbau der Kette abgelehnte Module
//!   (AIGW-101: `missing`, `unknown-key`, `invalid`, `digest`).
//!
//! `module` ist der Modulname (begrenzt: Module legt nur ein globaler `admin` an).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

/// Grenzen der Dauer-Buckets (Sekunden).
const BUCKETS: [f64; 12] = [
    0.000_05, 0.000_1, 0.000_25, 0.000_5, 0.001, 0.002_5, 0.005, 0.01, 0.025, 0.05, 0.1, 1.0,
];

#[derive(Debug, Default, Clone)]
struct Histogram {
    buckets: [u64; BUCKETS.len()],
    count: u64,
    sum: f64,
}

#[derive(Debug, Default)]
struct Inner {
    calls: BTreeMap<(String, &'static str, &'static str), u64>,
    errors: BTreeMap<(String, &'static str), u64>,
    durations: BTreeMap<(String, &'static str), Histogram>,
    fuel: BTreeMap<String, u64>,
    compile: BTreeMap<&'static str, u64>,
    signature: BTreeMap<&'static str, u64>,
}

/// Zähler der WASM-Plugins (eine Instanz je Laufzeit).
#[derive(Debug, Default)]
pub struct WasmMetrics {
    inner: Mutex<Inner>,
}

impl WasmMetrics {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Zählt einen Aufruf.
    pub fn record_call(
        &self,
        module: &str,
        phase: &'static str,
        outcome: &'static str,
        duration: Duration,
        fuel: u64,
    ) {
        let mut m = self.lock();
        *m.calls
            .entry((module.to_owned(), phase, outcome))
            .or_default() += 1;
        let h = m.durations.entry((module.to_owned(), phase)).or_default();
        let secs = duration.as_secs_f64();
        for (i, le) in BUCKETS.iter().enumerate() {
            if secs <= *le {
                h.buckets[i] += 1;
            }
        }
        h.count += 1;
        h.sum += secs;
        *m.fuel.entry(module.to_owned()).or_default() += fuel;
    }

    /// Zählt einen Fehler.
    pub fn record_error(&self, module: &str, kind: &'static str) {
        *self
            .lock()
            .errors
            .entry((module.to_owned(), kind))
            .or_default() += 1;
    }

    /// Zählt eine Kompilierung (`ok`, `error`, `cached`).
    pub fn record_compile(&self, outcome: &'static str) {
        *self.lock().compile.entry(outcome).or_default() += 1;
    }

    /// Zählt ein wegen der Signatur abgelehntes Modul (AIGW-101).
    pub fn signature_rejected(&self, reason: &'static str) {
        *self.lock().signature.entry(reason).or_default() += 1;
    }

    /// Stand des Zählers abgelehnter Signaturen (Tests).
    pub fn signature_rejections(&self, reason: &str) -> u64 {
        self.lock().signature.get(reason).copied().unwrap_or(0)
    }

    /// Stand eines Aufrufzählers (Tests).
    pub fn calls(&self, module: &str, phase: &str, outcome: &str) -> u64 {
        self.lock()
            .calls
            .iter()
            .filter(|((m, p, o), _)| m == module && *p == phase && *o == outcome)
            .map(|(_, n)| *n)
            .sum()
    }

    /// Stand eines Fehlerzählers (Tests).
    pub fn errors(&self, module: &str, kind: &str) -> u64 {
        self.lock()
            .errors
            .iter()
            .filter(|((m, k), _)| m == module && *k == kind)
            .map(|(_, n)| *n)
            .sum()
    }

    /// Verbrauchtes Fuel eines Moduls.
    pub fn fuel(&self, module: &str) -> u64 {
        self.lock().fuel.get(module).copied().unwrap_or(0)
    }

    /// Prometheus-Textformat (0.0.4).
    pub fn render_prometheus(&self, out: &mut String) {
        let m = self.lock();
        out.push_str(
            "# HELP aigw_wasm_calls_total Aufrufe von WASM-Plugins je Ergebnis\n\
             # TYPE aigw_wasm_calls_total counter\n",
        );
        for ((module, phase, outcome), n) in &m.calls {
            let _ = writeln!(
                out,
                "aigw_wasm_calls_total{{module=\"{module}\",phase=\"{phase}\",outcome=\"{outcome}\"}} {n}"
            );
        }
        out.push_str(
            "# HELP aigw_wasm_errors_total Fehler von WASM-Plugins (fuel, timeout, memory, trap, guest, output, instantiate)\n\
             # TYPE aigw_wasm_errors_total counter\n",
        );
        for ((module, kind), n) in &m.errors {
            let _ = writeln!(
                out,
                "aigw_wasm_errors_total{{module=\"{module}\",kind=\"{kind}\"}} {n}"
            );
        }
        out.push_str(
            "# HELP aigw_wasm_call_duration_seconds Dauer eines WASM-Aufrufs (Instanz + Aufruf)\n\
             # TYPE aigw_wasm_call_duration_seconds histogram\n",
        );
        for ((module, phase), h) in &m.durations {
            let labels = format!("module=\"{module}\",phase=\"{phase}\"");
            for (le, n) in BUCKETS.iter().zip(h.buckets) {
                let _ = writeln!(
                    out,
                    "aigw_wasm_call_duration_seconds_bucket{{{labels},le=\"{le}\"}} {n}"
                );
            }
            let _ = writeln!(
                out,
                "aigw_wasm_call_duration_seconds_bucket{{{labels},le=\"+Inf\"}} {}\n\
                 aigw_wasm_call_duration_seconds_sum{{{labels}}} {}\n\
                 aigw_wasm_call_duration_seconds_count{{{labels}}} {}",
                h.count, h.sum, h.count
            );
        }
        out.push_str(
            "# HELP aigw_wasm_fuel_consumed_total Verbrauchtes Fuel (CPU-Maß) je Modul\n\
             # TYPE aigw_wasm_fuel_consumed_total counter\n",
        );
        for (module, n) in &m.fuel {
            let _ = writeln!(
                out,
                "aigw_wasm_fuel_consumed_total{{module=\"{module}\"}} {n}"
            );
        }
        out.push_str(
            "# HELP aigw_wasm_compile_total Kompilierungen von WASM-Modulen (ok, error, cached)\n\
             # TYPE aigw_wasm_compile_total counter\n",
        );
        for (outcome, n) in &m.compile {
            let _ = writeln!(out, "aigw_wasm_compile_total{{outcome=\"{outcome}\"}} {n}");
        }
        out.push_str(
            "# HELP aigw_wasm_signature_rejected_total Wegen der Signatur nicht geladene WASM-Module\n\
             # TYPE aigw_wasm_signature_rejected_total counter\n",
        );
        for (reason, n) in &m.signature {
            let _ = writeln!(
                out,
                "aigw_wasm_signature_rejected_total{{reason=\"{reason}\"}} {n}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders() {
        let m = WasmMetrics::default();
        m.record_call("kw", "request", "pass", Duration::from_micros(30), 1000);
        m.record_error("kw", "fuel");
        m.record_compile("ok");
        let mut out = String::new();
        m.render_prometheus(&mut out);
        assert!(out.contains(
            "aigw_wasm_calls_total{module=\"kw\",phase=\"request\",outcome=\"pass\"} 1\n"
        ));
        assert!(out.contains("aigw_wasm_errors_total{module=\"kw\",kind=\"fuel\"} 1\n"));
        assert!(out.contains(
            "aigw_wasm_call_duration_seconds_bucket{module=\"kw\",phase=\"request\",le=\"0.00005\"} 1\n"
        ));
        assert!(out.contains("aigw_wasm_fuel_consumed_total{module=\"kw\"} 1000\n"));
        assert_eq!(m.calls("kw", "request", "pass"), 1);
        assert_eq!(m.fuel("kw"), 1000);
    }
}
