// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

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
use aigw_core::snapshot::KeyCtx;
use aigw_core::types::ProviderKind;
use aigw_core::wire::WireFormat;
use aigw_store::ReloadMetrics;
use arc_swap::ArcSwap;

use crate::process::endpoint_name;

/// Sammelwert für Label-Werte jenseits der Obergrenze.
pub const OTHER: &str = "other";

/// Höchstzahl der Serien von `aigw_guardrail_hits_total` (darüber Kategorie `other`).
pub const MAX_GUARDRAIL_SERIES: usize = 500;

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

/// Name des zuletzt verwendeten Deployments (Label `deployment`, F99): aus dem letzten Versuch
/// mit dieser ID; ohne Versuch die ID selbst.
#[inline]
fn deployment_name(r: &RequestRecord) -> Option<&str> {
    let id = r.deployment_id?;
    r.attempts
        .iter()
        .rev()
        .find(|a| a.deployment_id == Some(id))
        .map(|a| a.deployment.as_str())
}

/// Unveränderte Label-Werte aus dem Datensatz (Schlüssel im Hot Path).
#[derive(Debug, Clone, PartialEq, Eq)]
struct RawKey {
    model_group: Option<Box<str>>,
    deployment: Option<Id>,
    /// Name des Deployments (eine Umbenennung ergibt eine neue Serie).
    deployment_name: Option<Box<str>>,
    provider: Option<ProviderKind>,
    ingress: WireFormat,
    status_class: u8,
}

impl RawKey {
    fn of(r: &RequestRecord, class: u8) -> Self {
        Self {
            model_group: r.model_group.as_deref().map(Box::from),
            deployment: r.deployment_id,
            deployment_name: deployment_name(r).map(Box::from),
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
            && self.deployment_name.as_deref() == deployment_name(r)
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

/// Serie von `aigw_guardrail_hits_total`: (Plugin, Kategorie, Aktion).
type GuardrailKey = (&'static str, Box<str>, &'static str);

/// Zustand eines Deployments für `aigw_deployment_breaker_state` und
/// `aigw_deployment_inflight_requests` (F86); geliefert vom Routing beim Abruf von `/metrics`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentGauge {
    /// Modellgruppe.
    pub model_group: String,
    /// Name des Deployments.
    pub deployment: String,
    /// Circuit Breaker: 0 = geschlossen, 1 = offen, 2 = halboffen.
    pub breaker: u8,
    /// Offene Anfragen dieser Instanz.
    pub in_flight: u32,
    /// Budget des Deployments (AIGW-126): (Verbrauch, Betrag) in Nano-Einheiten der
    /// Abrechnungswährung; `None` ohne Budget.
    pub budget: Option<(i64, i64)>,
}

/// Nano-Einheiten als Dezimalzahl (ohne Gleitkomma-Rundung).
fn nanos_str(n: i64) -> String {
    aigw_core::resource::Amount::from_nanos(n).to_string()
}

/// Quelle der Deployment-Zustände.
type DeploymentSource = Box<dyn Fn() -> Vec<DeploymentGauge> + Send + Sync>;

/// Quelle der Zähler-Statistik: (nachgetragen, verworfen, ausstehend).
type CounterStatsSource = Box<dyn Fn() -> (u64, u64, u64) + Send + Sync>;

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
    /// Upstream-Versuche insgesamt, davon Retries (gleiche Gruppe) und Fallbacks (F86).
    attempts: AtomicU64,
    retries: AtomicU64,
    fallbacks: AtomicU64,
    deployments: OnceLock<DeploymentSource>,
    counter_degraded: OnceLock<Box<dyn Fn() -> bool + Send + Sync>>,
    /// Nachtragen der Zähler nach Ausfällen (F28): (nachgetragen, verworfen, ausstehend).
    counter_stats: OnceLock<CounterStatsSource>,
    /// `aigw_guardrail_hits_total{plugin,category,action}` (nur angefasst bei Erkennungen).
    guardrails: Mutex<BTreeMap<GuardrailKey, u64>>,
    /// MCP-Tool-Aufrufe (AIGW-41) je (Server, Tool, Ergebnis): (Anzahl, Dauer in µs).
    mcp: Mutex<BTreeMap<McpKey, (u64, u64)>>,
}

/// Serie von `aigw_mcp_calls_total`: (Server, Tool, Ergebnis).
type McpKey = (String, String, &'static str);

/// Höchstzahl der Serien von `aigw_mcp_calls_total`; darüber wird `tool` zu `other`.
const MAX_MCP_SERIES: usize = 2000;

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
            attempts: AtomicU64::new(0),
            retries: AtomicU64::new(0),
            fallbacks: AtomicU64::new(0),
            deployments: OnceLock::new(),
            counter_degraded: OnceLock::new(),
            counter_stats: OnceLock::new(),
            guardrails: Mutex::new(BTreeMap::new()),
            mcp: Mutex::new(BTreeMap::new()),
        }
    }

    /// Quelle für `aigw_counter_backend_degraded` (z. B. `CounterBackend::health()` von
    /// `aigw-counters`); einmal setzbar. Ohne Quelle entfällt die Metrik.
    pub fn set_counter_health(&self, degraded: impl Fn() -> bool + Send + Sync + 'static) {
        let _ = self.counter_degraded.set(Box::new(degraded));
    }

    /// Quelle für `aigw_counter_replayed_total`, `aigw_counter_replay_dropped_total` und
    /// `aigw_counter_replay_pending` (F28; z. B. `CounterBackend::stats()`), als
    /// (nachgetragen, verworfen, ausstehend); einmal setzbar.
    pub fn set_counter_stats(&self, stats: impl Fn() -> (u64, u64, u64) + Send + Sync + 'static) {
        let _ = self.counter_stats.set(Box::new(stats));
    }

    /// Quelle für `aigw_deployment_breaker_state` und `aigw_deployment_inflight_requests`
    /// (z. B. `SnapshotRoutes::deployment_status`); einmal setzbar (F86).
    pub fn set_deployment_source(
        &self,
        source: impl Fn() -> Vec<DeploymentGauge> + Send + Sync + 'static,
    ) {
        let _ = self.deployments.set(Box::new(source));
    }

    /// Versuche, Retries und Fallbacks einer Anfrage zählen (F86).
    #[inline]
    fn count_attempts(&self, attempts: &[aigw_core::observe::Attempt]) {
        if attempts.is_empty() {
            return;
        }
        self.attempts.fetch_add(
            u64::try_from(attempts.len()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        let (mut retries, mut fallbacks) = (0u64, 0u64);
        for w in attempts.windows(2) {
            if w[0].model_group == w[1].model_group {
                retries += 1;
            } else {
                fallbacks += 1;
            }
        }
        if retries > 0 {
            self.retries.fetch_add(retries, Ordering::Relaxed);
        }
        if fallbacks > 0 {
            self.fallbacks.fetch_add(fallbacks, Ordering::Relaxed);
        }
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

    /// Zählt einen MCP-Tool-Aufruf (`aigw_mcp_calls_total`, Dauer).
    fn observe_mcp(&self, m: &aigw_core::observe::McpCall, d: Duration) {
        let Ok(mut map) = self.mcp.lock() else {
            return;
        };
        let status = m.outcome.as_str();
        let mut key = (m.server.clone(), m.tool.clone(), status);
        if !map.contains_key(&key) && map.len() >= MAX_MCP_SERIES {
            key.1 = OTHER.to_owned();
        }
        let e = map.entry(key).or_insert((0, 0));
        e.0 += 1;
        e.1 = e.1.saturating_add(micros(d));
    }

    /// Zählt eine abgeschlossene Anfrage (Hot Path).
    #[inline]
    pub fn observe(&self, r: &RequestRecord) {
        self.inflight.fetch_sub(1, Ordering::Relaxed);
        if !r.guardrails.is_empty() {
            self.count_guardrails(r);
        }
        if let Some(m) = &r.mcp {
            self.observe_mcp(m, r.duration);
        }
        // Upstream-Fehler: jeder fehlgeschlagene Versuch (Retries/Fallbacks) und ein Fehler
        // nach erfolgreichem letztem Versuch (im Stream).
        for class in r.attempts.iter().filter_map(|a| a.error) {
            self.count_upstream_error(class);
        }
        self.count_attempts(&r.attempts);
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

    /// Guardrail-Erkennungen zählen; Kategorien jenseits von [`MAX_GUARDRAIL_SERIES`]
    /// Serien landen unter `other` (Kategorien externer Dienste sind nicht fest).
    #[cold]
    fn count_guardrails(&self, r: &RequestRecord) {
        let mut m = self
            .guardrails
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for h in &r.guardrails {
            let key = (h.plugin, Box::<str>::from(h.category.as_str()), h.action);
            let key = if m.contains_key(&key) || m.len() < MAX_GUARDRAIL_SERIES {
                key
            } else {
                (h.plugin, OTHER.into(), h.action)
            };
            *m.entry(key).or_insert(0) += u64::from(h.count);
        }
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
        // F99: Label = Name des Deployments (ohne Versuch mit Namen: die ID).
        let label = |d: Id| -> Box<str> {
            deployment_name(r).map_or_else(|| d.to_string().into(), Box::from)
        };
        let deployment: Box<str> = match r.deployment_id {
            None => "".into(),
            Some(d) if adm.deployments.contains(&d) => label(d),
            Some(d) if adm.deployments.len() < self.max_deployments => {
                adm.deployments.insert(d);
                label(d)
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
        {
            let m = self
                .guardrails
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !m.is_empty() {
                header(
                    out,
                    "aigw_guardrail_hits_total",
                    "counter",
                    "Erkennungen der Guardrail-Plugins je Plugin, Kategorie und Aktion",
                );
                for ((plugin, category, action), n) in m.iter() {
                    let _ = writeln!(
                        out,
                        "aigw_guardrail_hits_total{{plugin=\"{plugin}\",category=\"{}\",action=\"{action}\"}} {n}",
                        escape(category)
                    );
                }
            }
        }
        for (name, help, c) in [
            (
                "aigw_upstream_attempts_total",
                "Upstream-Versuche (erster Versuch, Retries und Fallbacks)",
                &self.attempts,
            ),
            (
                "aigw_upstream_retries_total",
                "Erneute Versuche innerhalb derselben Modellgruppe",
                &self.retries,
            ),
            (
                "aigw_fallbacks_total",
                "Wechsel auf eine Fallback-Modellgruppe",
                &self.fallbacks,
            ),
        ] {
            header(out, name, "counter", help);
            let _ = writeln!(out, "{name} {}", c.load(Ordering::Relaxed));
        }
        if let Some(f) = self.deployments.get() {
            let mut deps = f();
            deps.sort_by(|a, b| {
                (&a.model_group, &a.deployment).cmp(&(&b.model_group, &b.deployment))
            });
            deps.truncate(self.max_deployments);
            header(
                out,
                "aigw_deployment_breaker_state",
                "gauge",
                "Circuit Breaker je Deployment (0 geschlossen, 1 offen, 2 halboffen)",
            );
            for d in &deps {
                let _ = writeln!(
                    out,
                    "aigw_deployment_breaker_state{{model_group=\"{}\",deployment=\"{}\"}} {}",
                    escape(&d.model_group),
                    escape(&d.deployment),
                    d.breaker
                );
            }
            header(
                out,
                "aigw_deployment_inflight_requests",
                "gauge",
                "Offene Upstream-Anfragen je Deployment (diese Instanz)",
            );
            for d in &deps {
                let _ = writeln!(
                    out,
                    "aigw_deployment_inflight_requests{{model_group=\"{}\",deployment=\"{}\"}} {}",
                    escape(&d.model_group),
                    escape(&d.deployment),
                    d.in_flight
                );
            }
            // AIGW-126: Budget je Deployment (nur Deployments mit Budget).
            if deps.iter().any(|d| d.budget.is_some()) {
                for (name, help, f) in [
                    (
                        "aigw_deployment_budget_used",
                        "Verbrauch des Deployment-Budgets im laufenden Zeitraum (Abrechnungswährung)",
                        (|u, _| nanos_str(u)) as fn(i64, i64) -> String,
                    ),
                    (
                        "aigw_deployment_budget_amount",
                        "Betrag des Deployment-Budgets je Zeitraum (Abrechnungswährung)",
                        |_, a| nanos_str(a),
                    ),
                    (
                        "aigw_deployment_budget_used_ratio",
                        "Verbrauch des Deployment-Budgets als Anteil des Betrags (0–1, kann 1 übersteigen)",
                        |u, a| {
                            #[allow(clippy::cast_precision_loss)]
                            let r = if a > 0 { u as f64 / a as f64 } else { 0.0 };
                            format!("{r}")
                        },
                    ),
                ] {
                    header(out, name, "gauge", help);
                    for d in &deps {
                        if let Some((u, a)) = d.budget {
                            let _ = writeln!(
                                out,
                                "{name}{{model_group=\"{}\",deployment=\"{}\"}} {}",
                                escape(&d.model_group),
                                escape(&d.deployment),
                                f(u, a)
                            );
                        }
                    }
                }
            }
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
        let mcp: Vec<(McpKey, (u64, u64))> = self
            .mcp
            .lock()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), *v)).collect())
            .unwrap_or_default();
        if !mcp.is_empty() {
            header(
                out,
                "aigw_mcp_calls_total",
                "counter",
                "MCP-Tool-Aufrufe je Server, Tool und Ergebnis",
            );
            for ((server, tool, status), (n, _)) in &mcp {
                let _ = writeln!(
                    out,
                    "aigw_mcp_calls_total{{server=\"{}\",tool=\"{}\",status=\"{status}\"}} {n}",
                    escape(server),
                    escape(tool)
                );
            }
            header(
                out,
                "aigw_mcp_call_duration_seconds",
                "summary",
                "Dauer der MCP-Tool-Aufrufe",
            );
            for ((server, tool, status), (n, us)) in &mcp {
                let l = format!(
                    "server=\"{}\",tool=\"{}\",status=\"{status}\"",
                    escape(server),
                    escape(tool)
                );
                let _ = writeln!(
                    out,
                    "aigw_mcp_call_duration_seconds_sum{{{l}}} {}\naigw_mcp_call_duration_seconds_count{{{l}}} {n}",
                    seconds(*us)
                );
            }
        }
        if let Some(f) = self.counter_degraded.get() {
            header(
                out,
                "aigw_counter_backend_degraded",
                "gauge",
                "1, wenn das Zähler-Backend eingeschränkt ist (lokaler Rückfall)",
            );
            let _ = writeln!(out, "aigw_counter_backend_degraded {}", u8::from(f()));
        }
        if let Some(f) = self.counter_stats.get() {
            let (replayed, dropped, pending) = f();
            for (name, kind, help, v) in [
                (
                    "aigw_counter_replayed_total",
                    "counter",
                    "Nach einem Ausfall des Zähler-Backends nachgetragene Deltas",
                    replayed,
                ),
                (
                    "aigw_counter_replay_dropped_total",
                    "counter",
                    "Verworfene Deltas (Fenster abgelaufen oder Rückstau voll)",
                    dropped,
                ),
                (
                    "aigw_counter_replay_pending",
                    "gauge",
                    "Noch nicht nachgetragene Deltas",
                    pending,
                ),
            ] {
                header(out, name, kind, help);
                let _ = writeln!(out, "{name} {v}");
            }
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
        // AIGW-75: Teilmenge von ok bzw. Rückfälle/Konsistenzprüfung/Platten-Cache
        ("incremental", &reload.reloads_incremental),
        ("incremental_fallback", &reload.incremental_fallbacks),
        ("verify", &reload.verify_runs),
        ("verify_mismatch", &reload.verify_mismatches),
        ("cache_write", &reload.cache_writes),
        ("cache_write_failed", &reload.cache_write_errors),
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

    fn payload_mode(&self, key: Option<&KeyCtx>) -> PayloadMode {
        self.next.payload_mode(key)
    }
}

#[cfg(test)]
mod f99_tests {
    use super::*;
    use aigw_core::observe::Attempt;
    use aigw_core::resource::new_id;

    #[test]
    fn deployment_label_is_name() {
        let m = GatewayMetrics::default();
        let dep = new_id();
        let mut r = RequestRecord::new("x", WireFormat::OpenAiChat, 200);
        r.model_group = Some("g".into());
        r.deployment_id = Some(dep);
        r.attempts = vec![Attempt {
            model_group: Some("g".into()),
            deployment: "azure-west".into(),
            deployment_id: Some(dep),
            error: None,
            upstream_status: None,
            duration: Duration::from_millis(5),
        }];
        m.request_started();
        m.observe(&r);
        m.request_started();
        m.observe(&r);
        // Umbenennung: neue Serie mit neuem Namen
        r.attempts[0].deployment = "azure-west-2".into();
        m.request_started();
        m.observe(&r);
        let mut out = String::new();
        m.render_prometheus(&mut out);
        assert!(out.contains("deployment=\"azure-west\""), "{out}");
        assert!(out.contains("deployment=\"azure-west-2\""));
        assert!(!out.contains(&dep.to_string()), "keine ID als Label");
        assert!(out.contains(
            "aigw_requests_total{model_group=\"g\",deployment=\"azure-west\",provider=\"\",ingress=\"openai-chat\",status_class=\"2xx\"} 2"
        ));
        // F28: Zähler-Statistik
        m.set_counter_stats(|| (5, 2, 1));
        let mut out = String::new();
        m.render_prometheus(&mut out);
        assert!(out.contains("aigw_counter_replayed_total 5"));
        assert!(out.contains("aigw_counter_replay_dropped_total 2"));
        assert!(out.contains("aigw_counter_replay_pending 1"));
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
            // Name = ID-Text: gleiche Serie wie die Datensätze ohne Versuch (F99 eigener Test).
            deployment: dep.to_string(),
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
        // Versuche: 2 + 2, davon 2 Retries in derselben Gruppe, kein Fallback (F86).
        assert!(out.contains("aigw_upstream_attempts_total 4\n"));
        assert!(out.contains("aigw_upstream_retries_total 2\n"));
        assert!(out.contains("aigw_fallbacks_total 0\n"));
        assert!(!out.contains("aigw_deployment_breaker_state"));
        m.set_deployment_source(|| {
            vec![
                DeploymentGauge {
                    model_group: "gpt-4o".into(),
                    deployment: "d\"1".into(),
                    breaker: 1,
                    in_flight: 2,
                    budget: None,
                },
                DeploymentGauge {
                    model_group: "claude".into(),
                    deployment: "acct-1".into(),
                    breaker: 0,
                    in_flight: 0,
                    budget: Some((50_000_000_000, 200_000_000_000)),
                },
            ]
        });
        let mut fb = rec("gpt-4o", dep, 200);
        let mut other = attempt(None);
        other.model_group = Some("claude".into());
        fb.attempts = vec![attempt(Some(ErrorClass::ServerError)), other];
        obs.request_started();
        obs.record(fb);
        let mut out = String::new();
        m.render_prometheus(&mut out);
        assert!(out.contains("aigw_fallbacks_total 1\n"));
        assert!(out.contains(
            "aigw_deployment_breaker_state{model_group=\"gpt-4o\",deployment=\"d\\\"1\"} 1\n"
        ));
        assert!(out.contains(
            "aigw_deployment_inflight_requests{model_group=\"gpt-4o\",deployment=\"d\\\"1\"} 2\n"
        ));
        // AIGW-126: Budget je Deployment.
        assert!(out.contains(
            "aigw_deployment_budget_used{model_group=\"claude\",deployment=\"acct-1\"} 50\n"
        ));
        assert!(out.contains(
            "aigw_deployment_budget_amount{model_group=\"claude\",deployment=\"acct-1\"} 200\n"
        ));
        assert!(out.contains(
            "aigw_deployment_budget_used_ratio{model_group=\"claude\",deployment=\"acct-1\"} 0.25\n"
        ));
        assert!(!out.contains("aigw_deployment_budget_used{model_group=\"gpt-4o\""));
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
    fn guardrail_hits() {
        let m = GatewayMetrics::default();
        let mut r = RequestRecord::new("r", WireFormat::OpenAiChat, 400);
        m.request_started();
        m.observe(&r);
        let mut out = String::new();
        m.render_prometheus(&mut out);
        assert!(!out.contains("aigw_guardrail_hits_total"));
        let hits = aigw_core::observe::GuardrailHits::default();
        hits.add("pii", "email", "mask", 2);
        hits.add("pii", "email", "mask", 1);
        hits.add("prompt-filter", "injection", "reject", 1);
        r.guardrails = hits.take();
        m.request_started();
        m.observe(&r);
        let mut out = String::new();
        m.render_prometheus(&mut out);
        assert!(out.contains(
            "aigw_guardrail_hits_total{plugin=\"pii\",category=\"email\",action=\"mask\"} 3\n"
        ));
        assert!(out.contains(
            "aigw_guardrail_hits_total{plugin=\"prompt-filter\",category=\"injection\",action=\"reject\"} 1\n"
        ));
    }

    #[test]
    fn mcp_calls() {
        let m = GatewayMetrics::default();
        let mut r = RequestRecord::new("x", WireFormat::Mcp, 200);
        r.duration = Duration::from_millis(250);
        r.mcp = Some(Box::new(aigw_core::observe::McpCall {
            server: "gh".into(),
            tool: "search".into(),
            outcome: aigw_core::observe::McpOutcome::Ok,
        }));
        m.request_started();
        m.observe(&r);
        let mut out = String::new();
        m.render_prometheus(&mut out);
        assert!(
            out.contains("aigw_mcp_calls_total{server=\"gh\",tool=\"search\",status=\"ok\"} 1"),
            "{out}"
        );
        assert!(out.contains(
            "aigw_mcp_call_duration_seconds_count{server=\"gh\",tool=\"search\",status=\"ok\"} 1"
        ));
        assert!(out.contains("ingress=\"mcp\""), "{out}");
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
