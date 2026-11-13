// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Prometheus-Metriken des Gateways (AIGW-44): schlanke eigene Registry aus Atomics.
//!
//! Befüllt wird ausschließlich über den [`RequestObserver`]-Pfad: [`MetricsObserver`] sitzt
//! vor dem Request-Log ([`crate::RequestLogger`]), zählt den [`RequestRecord`] und reicht ihn
//! weiter. Im Hot Path:
//!
//! 1. Hash der Label-Werte (ohne Allokation, Werte werden aus dem Datensatz geliehen),
//! 2. lock-freies Nachschlagen der Serie in einer per [`ArcSwap`] veröffentlichten Map,
//! 3. eine Handvoll `Relaxed`-Inkremente (Zähler, Histogramm-Buckets, Tokens, Kosten).
//!
//! Nur die **erste** Anfrage einer neuen Label-Kombination nimmt einen Mutex, legt die Serie an
//! und veröffentlicht eine neue Map (Copy-on-Write). Gegen Label-Explosion sind die frei
//! wachsenden Labels (`model_group`, `deployment`) auf eine Höchstzahl unterschiedlicher Werte
//! begrenzt; weitere Werte landen im Sammelwert `other` (siehe [`MetricsConfig`]). Key-, Team-
//! und Nutzer-IDs sind nie Labels.
//!
//! Ausgabe im Prometheus-Textformat 0.0.4 ([`GatewayMetrics::render_prometheus`]).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::{self, Write as _};
use std::hash::{BuildHasher, BuildHasherDefault, Hash, Hasher, RandomState};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use aigw_core::config::MetricsConfig;
use aigw_core::ir::ErrorClass;
use aigw_core::observe::{PayloadMode, RequestObserver, RequestRecord};
use aigw_core::resource::Id;
use aigw_core::types::ProviderKind;
use aigw_core::wire::WireFormat;
use aigw_store::ReloadMetrics;
use arc_swap::ArcSwap;

use crate::process::endpoint_name;

/// Sammelwert für Label-Werte jenseits der Obergrenze.
pub const OTHER: &str = "other";

/// Bucket-Grenzen von `aigw_request_duration_seconds` (Mikrosekunden).
const DURATION_BOUNDS_US: &[u64] = &[
    50_000,
    100_000,
    250_000,
    500_000,
    1_000_000,
    2_500_000,
    5_000_000,
    10_000_000,
    20_000_000,
    30_000_000,
    60_000_000,
    120_000_000,
    300_000_000,
];

/// Bucket-Grenzen von `aigw_ttft_seconds` (Mikrosekunden).
const TTFT_BOUNDS_US: &[u64] = &[
    10_000, 25_000, 50_000, 100_000, 250_000, 500_000, 1_000_000, 2_500_000, 5_000_000, 10_000_000,
    30_000_000,
];

/// Token-Arten (`aigw_tokens_total{type}`).
const TOKEN_TYPES: [&str; 5] = ["input", "output", "cache_read", "cache_write", "reasoning"];

/// Fehlerklassen (`aigw_upstream_errors_total{class}`, wie [`ErrorClass::as_str`]), Index =
/// [`error_index`].
const ERROR_CLASSES: [&str; 14] = [
    "invalid_request",
    "context_length",
    "content_filter",
    "auth",
    "not_found",
    "rate_limit",
    "quota",
    "timeout",
    "connect",
    "server_error",
    "overloaded",
    "protocol",
    "internal",
    OTHER,
];

fn error_index(class: ErrorClass) -> usize {
    match class {
        ErrorClass::InvalidRequest => 0,
        ErrorClass::ContextLength => 1,
        ErrorClass::ContentFilter => 2,
        ErrorClass::Auth => 3,
        ErrorClass::NotFound => 4,
        ErrorClass::RateLimit => 5,
        ErrorClass::Quota => 6,
        ErrorClass::Timeout => 7,
        ErrorClass::Connect => 8,
        ErrorClass::ServerError => 9,
        ErrorClass::Overloaded => 10,
        ErrorClass::Protocol => 11,
        ErrorClass::Internal => 12,
        #[allow(unreachable_patterns)]
        _ => 13,
    }
}

fn status_class(status: u16) -> u8 {
    u8::try_from((status / 100).min(9)).unwrap_or(9)
}

fn status_class_label(class: u8) -> &'static str {
    match class {
        1 => "1xx",
        2 => "2xx",
        3 => "3xx",
        4 => "4xx",
        5 => "5xx",
        _ => OTHER,
    }
}

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

/// Histogramm mit festen Grenzen (Zählungen je Bucket, nicht kumulativ; Summe in µs).
struct Histogram {
    bounds: &'static [u64],
    counts: Box<[AtomicU64]>,
    sum_us: AtomicU64,
}

impl Histogram {
    fn new(bounds: &'static [u64]) -> Self {
        Self {
            bounds,
            counts: (0..=bounds.len()).map(|_| AtomicU64::new(0)).collect(),
            sum_us: AtomicU64::new(0),
        }
    }

    #[inline]
    fn observe(&self, d: Duration) {
        let us = micros(d);
        // Erster Bucket mit Grenze ≥ Wert (`le`); dahinter +Inf.
        let idx = self.bounds.partition_point(|&b| b < us);
        if let Some(c) = self.counts.get(idx) {
            c.fetch_add(1, Ordering::Relaxed);
        }
        self.sum_us.fetch_add(us, Ordering::Relaxed);
    }

    /// Kumulative Zählungen, Summe (µs).
    fn read(&self) -> (Vec<u64>, u64) {
        let mut acc = 0;
        let cumulative = self
            .counts
            .iter()
            .map(|c| {
                acc += c.load(Ordering::Relaxed);
                acc
            })
            .collect();
        (cumulative, self.sum_us.load(Ordering::Relaxed))
    }
}

/// Label-Werte einer Serie (bereits begrenzt bzw. auf `other` abgebildet).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Labels {
    model_group: Box<str>,
    deployment: Box<str>,
    provider: &'static str,
    ingress: &'static str,
}

/// Zähler einer Label-Kombination (inkl. Statusklasse).
struct Series {
    labels: Labels,
    status_class: u8,
    requests: AtomicU64,
    duration: Histogram,
    ttft: Histogram,
    tokens: [AtomicU64; 5],
    cost_nanos: AtomicI64,
    stream_aborts: AtomicU64,
}

impl Series {
    fn new(labels: Labels, status_class: u8) -> Self {
        Self {
            labels,
            status_class,
            requests: AtomicU64::new(0),
            duration: Histogram::new(DURATION_BOUNDS_US),
            ttft: Histogram::new(TTFT_BOUNDS_US),
            tokens: Default::default(),
            cost_nanos: AtomicI64::new(0),
            stream_aborts: AtomicU64::new(0),
        }
    }

    #[inline]
    fn observe(&self, r: &RequestRecord) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.duration.observe(r.duration);
        if let Some(t) = r.ttft {
            self.ttft.observe(t);
        }
        let u = &r.usage;
        for (slot, n) in self.tokens.iter().zip([
            u.input_tokens,
            u.output_tokens,
            u.cache_read_tokens.unwrap_or(0),
            u.cache_write_tokens.unwrap_or(0),
            u.reasoning_tokens.unwrap_or(0),
        ]) {
            if n > 0 {
                slot.fetch_add(n, Ordering::Relaxed);
            }
        }
        if let Some(c) = r.cost {
            self.cost_nanos.fetch_add(c.nanos(), Ordering::Relaxed);
        }
        if r.stream && r.client_aborted {
            self.stream_aborts.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Unveränderte Label-Werte aus dem Datensatz (Schlüssel im Hot Path).
#[derive(Debug, Clone, PartialEq, Eq)]
struct RawKey {
    model_group: Option<Box<str>>,
    deployment: Option<Id>,
    provider: Option<ProviderKind>,
    ingress: WireFormat,
    status_class: u8,
}

impl RawKey {
    fn of(r: &RequestRecord, class: u8) -> Self {
        Self {
            model_group: r.model_group.as_deref().map(Box::from),
            deployment: r.deployment_id,
            provider: r.provider,
            ingress: r.ingress,
            status_class: class,
        }
    }

    #[inline]
    fn matches(&self, r: &RequestRecord, class: u8) -> bool {
        self.status_class == class
            && self.ingress == r.ingress
            && self.provider == r.provider
            && self.deployment == r.deployment_id
            && self.model_group.as_deref() == r.model_group.as_deref()
    }
}

/// Hasher für bereits gehashte `u64`-Schlüssel.
#[derive(Default)]
struct IdentityHasher(u64);

impl Hasher for IdentityHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = self.0.rotate_left(8) ^ u64::from(*b);
        }
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = n;
    }
}

struct Entry {
    key: RawKey,
    series: Arc<Series>,
}

type SeriesMap = HashMap<u64, Vec<Entry>, BuildHasherDefault<IdentityHasher>>;

/// Zulassung neuer Label-Werte (nur im langsamen Pfad, unter Mutex).
#[derive(Default)]
struct Admission {
    model_groups: HashSet<Box<str>>,
    deployments: HashSet<Id>,
    /// Serien nach begrenzten Labels (Aliasse aus dem Hot-Path-Schlüssel zeigen hierauf).
    by_labels: HashMap<(Labels, u8), Arc<Series>>,
    /// Einträge in der Map (inkl. Aliasse).
    entries: usize,
}

/// Prometheus-Metriken des Gateways.
pub struct GatewayMetrics {
    max_model_groups: usize,
    max_deployments: usize,
    /// Obergrenze der Map-Einträge (Aliasse auf `other` eingeschlossen); darüber wird für
    /// unbekannte Kombinationen jedes Mal der langsame Pfad genommen.
    max_entries: usize,
    hasher: RandomState,
    series: ArcSwap<SeriesMap>,
    admission: Mutex<Admission>,
    inflight: AtomicI64,
    upstream_errors: [AtomicU64; ERROR_CLASSES.len()],
    counter_degraded: OnceLock<Box<dyn Fn() -> bool + Send + Sync>>,
}

impl fmt::Debug for GatewayMetrics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GatewayMetrics")
            .field("max_model_groups", &self.max_model_groups)
            .field("max_deployments", &self.max_deployments)
            .field("series", &self.series.load().len())
            .finish_non_exhaustive()
    }
}

impl Default for GatewayMetrics {
    fn default() -> Self {
        Self::new(&MetricsConfig::default())
    }
}

impl GatewayMetrics {
    /// Leere Registry mit den Obergrenzen aus `cfg`.
    pub fn new(cfg: &MetricsConfig) -> Self {
        let max_model_groups = cfg.max_model_groups.max(1);
        let max_deployments = cfg.max_deployments.max(1);
        Self {
            max_model_groups,
            max_deployments,
            // Großzügig: jede zugelassene Kombination × Statusklassen × Formate, plus Aliasse.
            max_entries: (max_model_groups + max_deployments).saturating_mul(64),
            hasher: RandomState::new(),
            series: ArcSwap::from_pointee(SeriesMap::default()),
            admission: Mutex::new(Admission::default()),
            inflight: AtomicI64::new(0),
            upstream_errors: Default::default(),
            counter_degraded: OnceLock::new(),
        }
    }

    /// Quelle für `aigw_counter_backend_degraded` (z. B. `CounterBackend::health()` von
    /// `aigw-counters`); einmal setzbar. Ohne Quelle entfällt die Metrik.
    pub fn set_counter_health(&self, degraded: impl Fn() -> bool + Send + Sync + 'static) {
        let _ = self.counter_degraded.set(Box::new(degraded));
    }

    /// Eine Anfrage beginnt.
    #[inline]
    pub fn request_started(&self) {
        self.inflight.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    fn key_hash(&self, r: &RequestRecord, class: u8) -> u64 {
        let mut h = self.hasher.build_hasher();
        r.model_group.as_deref().hash(&mut h);
        r.deployment_id.hash(&mut h);
        r.provider.hash(&mut h);
        r.ingress.hash(&mut h);
        class.hash(&mut h);
        h.finish()
    }

    #[inline]
    fn count_upstream_error(&self, class: ErrorClass) {
        if let Some(c) = self.upstream_errors.get(error_index(class)) {
            c.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Zählt eine abgeschlossene Anfrage (Hot Path).
    #[inline]
    pub fn observe(&self, r: &RequestRecord) {
        self.inflight.fetch_sub(1, Ordering::Relaxed);
        // Upstream-Fehler: jeder fehlgeschlagene Versuch (Retries/Fallbacks) und ein Fehler
        // nach erfolgreichem letztem Versuch (im Stream).
        for class in r.attempts.iter().filter_map(|a| a.error) {
            self.count_upstream_error(class);
        }
        if let Some(kind) = r.error_kind
            && r.attempts.last().is_some_and(|a| a.error.is_none())
        {
            self.count_upstream_error(ErrorClass::from_kind(kind));
        }
        let class = status_class(r.status);
        let hash = self.key_hash(r, class);
        {
            let map = self.series.load();
            if let Some(e) = map
                .get(&hash)
                .and_then(|v| v.iter().find(|e| e.key.matches(r, class)))
            {
                e.series.observe(r);
                return;
            }
        }
        self.insert(hash, r, class).observe(r);
    }

    /// Langsamer Pfad: Serie anlegen (bzw. auf `other` abbilden) und veröffentlichen.
    #[cold]
    fn insert(&self, hash: u64, r: &RequestRecord, class: u8) -> Arc<Series> {
        let mut adm = self
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Inzwischen von einem anderen Thread angelegt?
        if let Some(e) = self
            .series
            .load()
            .get(&hash)
            .and_then(|v| v.iter().find(|e| e.key.matches(r, class)))
        {
            return Arc::clone(&e.series);
        }
        let model_group: Box<str> = match r.model_group.as_deref() {
            None => "".into(),
            Some(g) if adm.model_groups.contains(g) => g.into(),
            Some(g) if adm.model_groups.len() < self.max_model_groups => {
                adm.model_groups.insert(g.into());
                g.into()
            }
            Some(_) => OTHER.into(),
        };
        let deployment: Box<str> = match r.deployment_id {
            None => "".into(),
            Some(d) if adm.deployments.contains(&d) => d.to_string().into(),
            Some(d) if adm.deployments.len() < self.max_deployments => {
                adm.deployments.insert(d);
                d.to_string().into()
            }
            Some(_) => OTHER.into(),
        };
        let labels = Labels {
            model_group,
            deployment,
            provider: r.provider.map_or("", ProviderKind::as_str),
            ingress: endpoint_name(r.ingress),
        };
        let series = Arc::clone(
            adm.by_labels
                .entry((labels.clone(), class))
                .or_insert_with(|| Arc::new(Series::new(labels, class))),
        );
        if adm.entries < self.max_entries {
            adm.entries += 1;
            let old = self.series.load();
            let mut map = SeriesMap::with_capacity_and_hasher(old.len() + 1, Default::default());
            for (h, entries) in old.iter() {
                map.insert(
                    *h,
                    entries
                        .iter()
                        .map(|e| Entry {
                            key: e.key.clone(),
                            series: Arc::clone(&e.series),
                        })
                        .collect(),
                );
            }
            map.entry(hash).or_default().push(Entry {
                key: RawKey::of(r, class),
                series: Arc::clone(&series),
            });
            self.series.store(Arc::new(map));
        }
        series
    }

    /// Anzahl der Serien (unterschiedliche begrenzte Label-Kombinationen).
    pub fn series_count(&self) -> usize {
        self.admission
            .lock()
            .map(|a| a.by_labels.len())
            .unwrap_or_default()
    }

    /// Hängt alle Gateway-Metriken im Prometheus-Textformat an `out` an.
    pub fn render_prometheus(&self, out: &mut String) {
        let mut series: Vec<Arc<Series>> = self
            .admission
            .lock()
            .map(|a| a.by_labels.values().cloned().collect())
            .unwrap_or_default();
        series.sort_by(|a, b| (&a.labels, a.status_class).cmp(&(&b.labels, b.status_class)));

        header(
            out,
            "aigw_requests_total",
            "counter",
            "Abgeschlossene Anfragen der Datenebene",
        );
        for s in &series {
            let _ = writeln!(
                out,
                "aigw_requests_total{{{},status_class=\"{}\"}} {}",
                base_labels(&s.labels),
                status_class_label(s.status_class),
                s.requests.load(Ordering::Relaxed)
            );
        }
        render_histograms(
            out,
            &series,
            "aigw_request_duration_seconds",
            "Gesamtdauer je Anfrage bis zum letzten Byte",
            |s| &s.duration,
        );
        render_histograms(
            out,
            &series,
            "aigw_ttft_seconds",
            "Zeit bis zum ersten Token (Streams)",
            |s| &s.ttft,
        );

        // Ohne Statusklasse: über die Klassen summiert.
        let mut per_labels: BTreeMap<&Labels, ([u64; 5], i64, u64)> = BTreeMap::new();
        for s in &series {
            let e = per_labels.entry(&s.labels).or_default();
            for (acc, t) in e.0.iter_mut().zip(&s.tokens) {
                *acc += t.load(Ordering::Relaxed);
            }
            e.1 += s.cost_nanos.load(Ordering::Relaxed);
            e.2 += s.stream_aborts.load(Ordering::Relaxed);
        }
        header(
            out,
            "aigw_tokens_total",
            "counter",
            "Tokens je Art (vom Anbieter gemeldet oder geschätzt)",
        );
        for (labels, (tokens, _, _)) in &per_labels {
            for (ty, n) in TOKEN_TYPES.iter().zip(tokens) {
                let _ = writeln!(
                    out,
                    "aigw_tokens_total{{{},type=\"{ty}\"}} {n}",
                    base_labels(labels)
                );
            }
        }
        header(
            out,
            "aigw_cost_total",
            "counter",
            "Kosten in Währungseinheiten (Platzhalter bis zur Kostenberechnung, AIGW-31)",
        );
        for (labels, (_, cost, _)) in &per_labels {
            let _ = writeln!(
                out,
                "aigw_cost_total{{{}}} {}",
                base_labels(labels),
                *cost as f64 / 1e9
            );
        }
        header(
            out,
            "aigw_stream_client_aborts_total",
            "counter",
            "Streams, die der Client vorzeitig getrennt hat",
        );
        for (labels, (_, _, aborts)) in &per_labels {
            let _ = writeln!(
                out,
                "aigw_stream_client_aborts_total{{{}}} {aborts}",
                base_labels(labels)
            );
        }

        header(
            out,
            "aigw_upstream_errors_total",
            "counter",
            "Fehlgeschlagene Upstream-Versuche (inkl. Retries/Fallbacks) und Stream-Fehler je Fehlerklasse",
        );
        for (class, c) in ERROR_CLASSES.iter().zip(&self.upstream_errors) {
            let _ = writeln!(
                out,
                "aigw_upstream_errors_total{{class=\"{class}\"}} {}",
                c.load(Ordering::Relaxed)
            );
        }
        header(
            out,
            "aigw_inflight_requests",
            "gauge",
            "Laufende Anfragen der Datenebene",
        );
        let _ = writeln!(
            out,
            "aigw_inflight_requests {}",
            self.inflight.load(Ordering::Relaxed).max(0)
        );
        if let Some(f) = self.counter_degraded.get() {
            header(
                out,
                "aigw_counter_backend_degraded",
                "gauge",
                "1, wenn das Zähler-Backend eingeschränkt ist (lokaler Rückfall)",
            );
            let _ = writeln!(out, "aigw_counter_backend_degraded {}", u8::from(f()));
        }
    }
}

fn header(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
}

/// Escaping nach dem Textformat: `\`, `"` und Zeilenumbruch.
fn escape(v: &str) -> std::borrow::Cow<'_, str> {
    if v.contains(['\\', '"', '\n']) {
        v.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .into()
    } else {
        v.into()
    }
}

fn base_labels(l: &Labels) -> String {
    format!(
        "model_group=\"{}\",deployment=\"{}\",provider=\"{}\",ingress=\"{}\"",
        escape(&l.model_group),
        escape(&l.deployment),
        l.provider,
        l.ingress
    )
}

fn seconds(us: u64) -> f64 {
    us as f64 / 1e6
}

fn render_histograms(
    out: &mut String,
    series: &[Arc<Series>],
    name: &str,
    help: &str,
    pick: impl Fn(&Series) -> &Histogram,
) {
    header(out, name, "histogram", help);
    for s in series {
        let h = pick(s);
        let (cumulative, sum_us) = h.read();
        let total = cumulative.last().copied().unwrap_or(0);
        if total == 0 {
            continue;
        }
        let labels = format!(
            "{},status_class=\"{}\"",
            base_labels(&s.labels),
            status_class_label(s.status_class)
        );
        for (bound, n) in h.bounds.iter().zip(&cumulative) {
            let _ = writeln!(
                out,
                "{name}_bucket{{{labels},le=\"{}\"}} {n}",
                seconds(*bound)
            );
        }
        let _ = writeln!(out, "{name}_bucket{{{labels},le=\"+Inf\"}} {total}");
        let _ = writeln!(out, "{name}_sum{{{labels}}} {}", seconds(sum_us));
        let _ = writeln!(out, "{name}_count{{{labels}}} {total}");
    }
}

/// Metriken des Konfigurations-Snapshots (aktive Version, Reloads, Veraltung).
pub fn render_snapshot(out: &mut String, version: i64, reload: &ReloadMetrics) {
    header(
        out,
        "aigw_snapshot_version",
        "gauge",
        "config_version des aktiven Konfigurations-Snapshots",
    );
    let _ = writeln!(out, "aigw_snapshot_version {version}");
    header(
        out,
        "aigw_snapshot_reload_total",
        "counter",
        "Snapshot-Neubauten bzw. Versionsprüfungen nach Ergebnis",
    );
    for (result, c) in [
        ("ok", &reload.reloads_ok),
        ("failed", &reload.reloads_failed),
        ("check_failed", &reload.checks_failed),
    ] {
        let _ = writeln!(
            out,
            "aigw_snapshot_reload_total{{result=\"{result}\"}} {}",
            c.load(Ordering::Relaxed)
        );
    }
    header(
        out,
        "aigw_snapshot_stale_seconds",
        "gauge",
        "Sekunden seit der Snapshot nicht mehr mit der Datenbank abgeglichen werden kann (0 = aktuell)",
    );
    let _ = writeln!(
        out,
        "aigw_snapshot_stale_seconds {}",
        reload.stale_for().map_or(0.0, |d| d.as_secs_f64())
    );
    header(
        out,
        "aigw_snapshot_build_seconds",
        "gauge",
        "Dauer des letzten erfolgreichen Snapshot-Neubaus",
    );
    let _ = writeln!(
        out,
        "aigw_snapshot_build_seconds {}",
        seconds(reload.last_build_micros.load(Ordering::Relaxed))
    );
}

/// Fan-out-Beobachter: zählt jeden Datensatz in [`GatewayMetrics`] und reicht ihn an den
/// nächsten Beobachter (in der Regel das Request-Log) weiter.
#[derive(Debug, Clone)]
pub struct MetricsObserver {
    metrics: Arc<GatewayMetrics>,
    next: Arc<dyn RequestObserver>,
}

impl MetricsObserver {
    /// Zählt in `metrics`, dann `next`.
    pub fn new(metrics: Arc<GatewayMetrics>, next: Arc<dyn RequestObserver>) -> Self {
        Self { metrics, next }
    }
}

impl RequestObserver for MetricsObserver {
    #[inline]
    fn request_started(&self) {
        self.metrics.request_started();
        self.next.request_started();
    }

    #[inline]
    fn record(&self, record: RequestRecord) {
        self.metrics.observe(&record);
        self.next.record(record);
    }

    fn payload_mode(&self) -> PayloadMode {
        self.next.payload_mode()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigw_core::ir::{ErrorKind, Usage};
    use aigw_core::observe::{Attempt, NoopObserver};
    use aigw_core::resource::{Amount, new_id};

    fn rec(group: &str, dep: Id, status: u16) -> RequestRecord {
        let mut r = RequestRecord::new("x", WireFormat::OpenAiChat, status);
        r.model_group = Some(group.into());
        r.deployment_id = Some(dep);
        r.provider = Some(ProviderKind::OpenAi);
        r.provider_model = Some("gpt".into());
        r.duration = Duration::from_millis(300);
        r.ttft = Some(Duration::from_millis(40));
        r.usage = Usage {
            input_tokens: 10,
            output_tokens: 5,
            reasoning_tokens: Some(2),
            ..Usage::default()
        };
        r.cost = Some(Amount::from_nanos(1_500_000_000));
        r
    }

    #[test]
    fn counts_histograms_tokens_and_errors() {
        let m = Arc::new(GatewayMetrics::default());
        let obs = MetricsObserver::new(Arc::clone(&m), Arc::new(NoopObserver));
        let dep = new_id();
        for _ in 0..3 {
            obs.request_started();
            obs.record(rec("gpt-4o", dep, 200));
        }
        let attempt = |error: Option<ErrorClass>| Attempt {
            model_group: Some("gpt-4o".into()),
            deployment: "d".into(),
            deployment_id: Some(dep),
            error,
            upstream_status: None,
            duration: Duration::from_millis(5),
        };
        // Stream: erster Versuch Verbindungsfehler, zweiter ok, dann Fehler im Stream.
        obs.request_started();
        let mut err = rec("gpt-4o", dep, 504);
        err.error_kind = Some(ErrorKind::Timeout);
        err.stream = true;
        err.client_aborted = true;
        err.attempts = vec![attempt(Some(ErrorClass::Connect)), attempt(None)];
        obs.record(err);
        // Zwei fehlgeschlagene Versuche (429) ohne Erfolg.
        obs.request_started();
        let mut limited = rec("gpt-4o", dep, 429);
        limited.error_kind = Some(ErrorKind::RateLimit);
        limited.attempts = vec![
            attempt(Some(ErrorClass::RateLimit)),
            attempt(Some(ErrorClass::RateLimit)),
        ];
        obs.record(limited);
        obs.request_started();
        // Fehler vor der Deployment-Wahl zählt nicht als Upstream-Fehler.
        let mut auth = RequestRecord::new("y", WireFormat::AnthropicMessages, 401);
        auth.error_kind = Some(ErrorKind::Authentication);
        obs.record(auth);
        obs.request_started();

        let mut out = String::new();
        m.render_prometheus(&mut out);
        let l = format!(
            "model_group=\"gpt-4o\",deployment=\"{dep}\",provider=\"openai\",ingress=\"openai-chat\""
        );
        assert!(out.contains(&format!(
            "aigw_requests_total{{{l},status_class=\"2xx\"}} 3\n"
        )));
        assert!(out.contains(&format!(
            "aigw_requests_total{{{l},status_class=\"5xx\"}} 1\n"
        )));
        assert!(out.contains(
            "aigw_requests_total{model_group=\"\",deployment=\"\",provider=\"\",ingress=\"anthropic-messages\",status_class=\"4xx\"} 1\n"
        ));
        assert!(out.contains(&format!(
            "aigw_request_duration_seconds_bucket{{{l},status_class=\"2xx\",le=\"0.25\"}} 0\n"
        )));
        assert!(out.contains(&format!(
            "aigw_request_duration_seconds_bucket{{{l},status_class=\"2xx\",le=\"0.5\"}} 3\n"
        )));
        assert!(out.contains(&format!(
            "aigw_request_duration_seconds_bucket{{{l},status_class=\"2xx\",le=\"+Inf\"}} 3\n"
        )));
        assert!(out.contains(&format!(
            "aigw_request_duration_seconds_sum{{{l},status_class=\"2xx\"}} 0.9\n"
        )));
        assert!(out.contains(&format!(
            "aigw_ttft_seconds_bucket{{{l},status_class=\"2xx\",le=\"0.05\"}} 3\n"
        )));
        assert!(out.contains(&format!("aigw_tokens_total{{{l},type=\"input\"}} 50\n")));
        assert!(out.contains(&format!("aigw_tokens_total{{{l},type=\"reasoning\"}} 10\n")));
        assert!(out.contains(&format!("aigw_cost_total{{{l}}} 7.5\n")));
        assert!(out.contains(&format!("aigw_stream_client_aborts_total{{{l}}} 1\n")));
        assert!(out.contains("aigw_upstream_errors_total{class=\"connect\"} 1\n"));
        assert!(out.contains("aigw_upstream_errors_total{class=\"timeout\"} 1\n"));
        assert!(out.contains("aigw_upstream_errors_total{class=\"rate_limit\"} 2\n"));
        assert!(out.contains("aigw_upstream_errors_total{class=\"auth\"} 0\n"));
        assert!(out.contains("aigw_inflight_requests 1\n"));
        assert!(!out.contains("aigw_counter_backend_degraded"));
        m.set_counter_health(|| true);
        let mut out = String::new();
        m.render_prometheus(&mut out);
        assert!(out.contains("aigw_counter_backend_degraded 1\n"));
    }

    #[test]
    fn label_values_are_capped_with_other() {
        let m = GatewayMetrics::new(&MetricsConfig {
            enabled: true,
            max_model_groups: 2,
            max_deployments: 3,
        });
        let deps: Vec<Id> = (0..5).map(|_| new_id()).collect();
        for i in 0..50 {
            m.request_started();
            m.observe(&rec(&format!("group-{i}"), deps[i % 5], 200));
        }
        // 2 Gruppen + other; Deployments: 3 + other.
        let mut out = String::new();
        m.render_prometheus(&mut out);
        assert!(out.contains("model_group=\"other\""));
        assert!(out.contains("deployment=\"other\""));
        let groups: HashSet<&str> = out
            .lines()
            .filter(|l| l.starts_with("aigw_requests_total{"))
            .filter_map(|l| l.split("model_group=\"").nth(1)?.split('"').next())
            .collect();
        assert_eq!(groups.len(), 3, "{groups:?}");
        assert!(m.series_count() <= 3 * 4);
        let total: u64 = out
            .lines()
            .filter(|l| l.starts_with("aigw_requests_total{"))
            .filter_map(|l| l.rsplit(' ').next()?.parse::<u64>().ok())
            .sum();
        assert_eq!(total, 50);
    }

    #[test]
    fn escapes_label_values() {
        let m = GatewayMetrics::default();
        m.observe(&rec("a\"b\\c\nd", new_id(), 200));
        let mut out = String::new();
        m.render_prometheus(&mut out);
        assert!(out.contains("model_group=\"a\\\"b\\\\c\\nd\""));
    }

    #[test]
    fn snapshot_metrics() {
        let r = ReloadMetrics::default();
        r.reloads_ok.fetch_add(2, Ordering::Relaxed);
        let mut out = String::new();
        render_snapshot(&mut out, 42, &r);
        assert!(out.contains("aigw_snapshot_version 42\n"));
        assert!(out.contains("aigw_snapshot_reload_total{result=\"ok\"} 2\n"));
        assert!(out.contains("aigw_snapshot_stale_seconds 0\n"));
    }
}
