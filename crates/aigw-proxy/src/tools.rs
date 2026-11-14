// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Tool-Richtlinien und Tool-Inventar im Hot Path (AIGW-87, `docs/konzept/16-tools.md`).
//!
//! Schritt in der Pipeline (Chat/Messages, Responses): **nach** Schlüsselprüfung und
//! Modellfreigabe, **vor** Limits/Budgets und der Plugin-Kette:
//!
//! 1. Entscheidung je Tool aus dem Index des Snapshots ([`ToolPolicyIndex`]): `block-reject`
//!    → `403` (`tool_blocked`), `block-strip` bzw. nicht freigegeben → Tool entfernen und
//!    `tool_choice` anpassen, `observe` → Warnung im Log.
//! 2. Inventar ([`ToolInventory`]): je Tool zwei BLAKE3-Hashes (Beschreibung, Schema – das
//!    Schema wird als JSON-Baum ohne Serialisierung gehasht) und ein Eintrag in einer
//!    geshardeten Map; geschrieben wird gepuffert und aggregiert im Hintergrund.
//! 3. Antwort: Tool-Aufrufe des Modells werden gezählt; Aufrufe nicht erlaubter Tools werden
//!    unterbunden (`200` mit `finish_reason: content_filter` bzw. `stop_reason: refusal`,
//!    Header `x-aigw-guardrail: tool-policy` – wie die Ausgabe-Ablehnung der Guardrails, F160).
//!    Im Stream ersetzt der Filter ab dem ersten solchen Aufruf alle Inhaltsereignisse und
//!    beendet die Variante mit `content_filter`; Usage läuft weiter.
//!
//! Ohne Tools in der Anfrage kostet der Schritt nichts.

use std::collections::HashSet;
use std::fmt::{self, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use aigw_core::ir::{
    ApiError, ChatRequest, ChatResponse, Content, ErrorKind, PartKind, StopReason, StreamEvent,
    ToolChoice, ToolKind,
};
use aigw_core::observe::PayloadMode;
use aigw_core::snapshot::Snapshot;
use aigw_core::tools::{
    CLIENT_ORIGIN, Effect, Reason, ToolBatch, ToolCtx, ToolDefinition, ToolEvent,
    ToolInventorySink, ToolKey, ToolKindName, ToolObservation, origin_of,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use dashmap::DashMap;
use futures_util::Stream;
use serde_json::Value;

use crate::auth::AuthContext;
use crate::provider::EventStream;

/// Wert des Headers `x-aigw-guardrail`, wenn ein Tool-Aufruf unterbunden wurde.
pub const TOOL_GUARDRAIL: &str = "tool-policy";
/// Höchstlänge des gespeicherten Clients.
const MAX_CLIENT: usize = 96;

type Hash = [u8; 16];

fn hex(h: &Hash) -> String {
    let mut s = String::with_capacity(32);
    for b in h {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn hash_str(s: &str) -> Hash {
    hash_bytes(s.as_bytes())
}

/// Kanonische Bytefolge eines JSON-Baums (Typ-Präfixe, Längen, Schlüssel in der Reihenfolge
/// der Map – ohne `preserve_order` sortiert), ohne JSON-Serialisierung.
fn encode_json(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => out.push(b'n'),
        Value::Bool(b) => out.push(if *b { b't' } else { b'f' }),
        Value::Number(n) => {
            out.push(b'#');
            if let Some(i) = n.as_i64() {
                out.push(b'i');
                out.extend_from_slice(&i.to_le_bytes());
            } else if let Some(u) = n.as_u64() {
                out.push(b'u');
                out.extend_from_slice(&u.to_le_bytes());
            } else {
                out.push(b'f');
                out.extend_from_slice(&n.as_f64().unwrap_or_default().to_bits().to_le_bytes());
            }
        }
        Value::String(s) => {
            out.push(b's');
            out.extend_from_slice(&(s.len() as u64).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        Value::Array(a) => {
            out.push(b'[');
            out.extend_from_slice(&(a.len() as u64).to_le_bytes());
            for x in a {
                encode_json(out, x);
            }
        }
        Value::Object(m) => {
            out.push(b'{');
            out.extend_from_slice(&(m.len() as u64).to_le_bytes());
            for (k, x) in m {
                out.extend_from_slice(&(k.len() as u64).to_le_bytes());
                out.extend_from_slice(k.as_bytes());
                encode_json(out, x);
            }
        }
    }
}

thread_local! {
    /// Wiederverwendeter Puffer für [`encode_json`].
    static BUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// BLAKE3 (128 Bit) des Schemas über seine kanonische Bytefolge.
fn hash_json(v: Option<&Value>) -> Hash {
    BUF.with_borrow_mut(|buf| {
        buf.clear();
        if let Some(v) = v {
            encode_json(buf, v);
        }
        hash_bytes(buf)
    })
}

fn hash_bytes(b: &[u8]) -> Hash {
    let mut out = [0u8; 16];
    out.copy_from_slice(&blake3::hash(b).as_bytes()[..16]);
    out
}

/// Schneller Fingerabdruck (Beschreibung, Schema) mit prozesszufälligem Schlüssel
/// (foldhash): nur im RAM, um bekannte Definitionen ohne BLAKE3 wiederzuerkennen. Ein Angreifer
/// kennt den Schlüssel nicht und kann keine gezielten Kollisionen bauen.
type Fp = (u64, u64);

static FP_SEED: std::sync::LazyLock<foldhash::quality::RandomState> =
    std::sync::LazyLock::new(foldhash::quality::RandomState::default);

fn fingerprint(tv: &ToolView<'_>) -> Fp {
    use std::hash::BuildHasher as _;
    let d = FP_SEED.hash_one(tv.description.unwrap_or(""));
    let s = BUF.with_borrow_mut(|buf| {
        buf.clear();
        if let Some(v) = tv.schema {
            encode_json(buf, v);
        }
        FP_SEED.hash_one(buf.as_slice())
    });
    (d, s)
}

/// Bekannter Stand eines Tools (gespeicherte Hashes, ggf. mit Fingerabdruck).
#[derive(Debug, Clone, Copy)]
struct Known {
    fp: Option<Fp>,
    dh: Hash,
    sh: Hash,
}

/// Sicht auf ein Tool der Anfrage.
struct ToolView<'a> {
    name: std::borrow::Cow<'a, str>,
    kind: ToolKindName,
    description: Option<&'a str>,
    schema: Option<&'a Value>,
}

fn view(t: &aigw_core::ir::Tool) -> ToolView<'_> {
    match &t.kind {
        ToolKind::Function(f) => ToolView {
            name: std::borrow::Cow::Borrowed(f.name.as_str()),
            kind: ToolKindName::Function,
            description: f.description.as_deref(),
            schema: f.parameters.as_ref(),
        },
        ToolKind::Opaque { value, .. } => {
            let ty = value.get("type").and_then(Value::as_str).unwrap_or("");
            let name = match (value.get("name").and_then(Value::as_str), ty) {
                (Some(n), _) => std::borrow::Cow::Borrowed(n),
                (None, "mcp") => std::borrow::Cow::Owned(format!(
                    "mcp:{}",
                    value
                        .get("server_label")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                )),
                (None, t) => std::borrow::Cow::Borrowed(t),
            };
            // Versionierte Server-Tools (`web_search_20250305`) vs. eingebaute ohne Version.
            let versioned = ty
                .rsplit_once('_')
                .is_some_and(|(_, v)| v.len() == 8 && v.bytes().all(|b| b.is_ascii_digit()));
            ToolView {
                name,
                kind: if versioned {
                    ToolKindName::ServerTool
                } else {
                    ToolKindName::Builtin
                },
                description: value.get("description").and_then(Value::as_str),
                schema: Some(value),
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Inventar
// ---------------------------------------------------------------------------------------------

#[derive(Debug)]
struct Agg {
    kind: ToolKindName,
    origin: Box<str>,
    client: Option<Box<str>>,
    first: i64,
    last: i64,
    requests: u64,
    calls: u64,
    blocked: u64,
    fp: Fp,
    dh: Hash,
    sh: Hash,
    /// Definition für den Abgleich (nur neue/geänderte Hashes).
    def: Option<PendingDef>,
}

#[derive(Debug)]
struct PendingDef {
    description: Option<String>,
    schema: Option<String>,
    /// Inhalte speichern (`payloadMode` `full`/`masked`); sonst nur prüfen.
    store: Option<PayloadMode>,
}

/// Zähler des Inventars (`/metrics`).
#[derive(Debug, Default)]
pub struct ToolMetrics {
    /// Entfernte Tools.
    pub stripped: AtomicU64,
    /// Abgelehnte Anfragen.
    pub rejected: AtomicU64,
    /// `observe`-Treffer.
    pub observed: AtomicU64,
    /// Unterbundene Tool-Aufrufe in Antworten.
    pub calls_blocked: AtomicU64,
    /// Verworfene Einträge (`tools.max_entries`).
    pub dropped: AtomicU64,
    /// Fehlgeschlagene Schreibvorgänge.
    pub flush_errors: AtomicU64,
    /// Erkannte Änderungen.
    pub changed: AtomicU64,
    /// Verdächtige Beschreibungen.
    pub suspicious: AtomicU64,
}

impl ToolMetrics {
    /// Prometheus-Textformat.
    pub fn render_prometheus(&self, out: &mut String) {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let _ = writeln!(
            out,
            "# HELP aigw_tool_policy_actions_total Wirkungen der Tool-Richtlinien (AIGW-87).\n\
             # TYPE aigw_tool_policy_actions_total counter\n\
             aigw_tool_policy_actions_total{{action=\"strip\"}} {}\n\
             aigw_tool_policy_actions_total{{action=\"reject\"}} {}\n\
             aigw_tool_policy_actions_total{{action=\"observe\"}} {}\n\
             aigw_tool_policy_actions_total{{action=\"call_blocked\"}} {}",
            g(&self.stripped),
            g(&self.rejected),
            g(&self.observed),
            g(&self.calls_blocked)
        );
        let _ = writeln!(
            out,
            "# HELP aigw_tool_inventory_events_total Ereignisse des Tool-Inventars.\n\
             # TYPE aigw_tool_inventory_events_total counter\n\
             aigw_tool_inventory_events_total{{event=\"changed\"}} {}\n\
             aigw_tool_inventory_events_total{{event=\"suspicious\"}} {}\n\
             aigw_tool_inventory_events_total{{event=\"dropped\"}} {}\n\
             aigw_tool_inventory_events_total{{event=\"flush_error\"}} {}",
            g(&self.changed),
            g(&self.suspicious),
            g(&self.dropped),
            g(&self.flush_errors)
        );
    }
}

/// Rückruf für Ereignisse (z. B. Ereignis-Webhooks `tool.changed`).
pub type ToolEventHook = Arc<dyn Fn(&ToolEvent) + Send + Sync>;

/// Tool-Inventar: RAM-Abgleich bekannter Hashes, Aggregation, gepuffertes Schreiben.
pub struct ToolInventory {
    sink: Option<Arc<dyn ToolInventorySink>>,
    client_headers: Vec<HeaderName>,
    max_entries: usize,
    injection_check: bool,
    agg: DashMap<ToolKey, Agg>,
    known: DashMap<ToolKey, Known>,
    metrics: Arc<ToolMetrics>,
    hook: Option<ToolEventHook>,
    flush_lock: tokio::sync::Mutex<()>,
}

impl fmt::Debug for ToolInventory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolInventory")
            .field("enabled", &self.sink.is_some())
            .field("entries", &self.agg.len())
            .finish_non_exhaustive()
    }
}

impl Default for ToolInventory {
    /// Ohne Persistenz: Richtlinien wirken, nichts wird inventarisiert.
    fn default() -> Self {
        Self {
            sink: None,
            client_headers: Vec::new(),
            max_entries: 0,
            injection_check: false,
            agg: DashMap::new(),
            known: DashMap::new(),
            metrics: Arc::new(ToolMetrics::default()),
            hook: None,
            flush_lock: tokio::sync::Mutex::new(()),
        }
    }
}

fn parse_hex(s: &str) -> Option<Hash> {
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(0)
}

impl ToolInventory {
    /// Inventar mit Persistenz `sink` und den Einstellungen `tools.*`.
    pub fn new(sink: Arc<dyn ToolInventorySink>, cfg: &aigw_core::config::ToolsConfig) -> Self {
        Self {
            sink: cfg.inventory.then_some(sink),
            client_headers: cfg
                .client_headers
                .iter()
                .filter_map(|h| HeaderName::try_from(h.to_ascii_lowercase()).ok())
                .collect(),
            max_entries: cfg.max_entries,
            injection_check: cfg.injection_check,
            ..Self::default()
        }
    }

    /// Rückruf für Ereignisse setzen.
    #[must_use]
    pub fn with_hook(mut self, hook: ToolEventHook) -> Self {
        self.hook = Some(hook);
        self
    }

    /// Zähler.
    pub fn metrics(&self) -> Arc<ToolMetrics> {
        Arc::clone(&self.metrics)
    }

    /// Inventar aktiv?
    pub fn enabled(&self) -> bool {
        self.sink.is_some()
    }

    /// Bekannte Hashes (Warmstart aus der Datenbank).
    pub fn preload(&self, known: impl IntoIterator<Item = (ToolKey, (String, String))>) {
        for (k, (d, s)) in known {
            if let (Some(dh), Some(sh)) = (parse_hex(&d), parse_hex(&s)) {
                self.known.insert(k, Known { fp: None, dh, sh });
            }
        }
    }

    fn client(&self, headers: &HeaderMap) -> Option<Box<str>> {
        let v = self
            .client_headers
            .iter()
            .chain(std::iter::once(&axum::http::header::USER_AGENT))
            .find_map(|h| headers.get(h))
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())?;
        let mut cut = v.len().min(MAX_CLIENT);
        while !v.is_char_boundary(cut) {
            cut -= 1;
        }
        Some(v[..cut].into())
    }

    /// Hashes und ggf. Definition zu einem Fingerabdruck: bekannt → ohne BLAKE3.
    fn resolve(
        &self,
        key: &ToolKey,
        fp: Fp,
        tv: &ToolView<'_>,
        mode: PayloadMode,
    ) -> (Hash, Hash, Option<PendingDef>) {
        let known = self.known.get(key).map(|k| *k);
        if let Some(k) = known
            && k.fp == Some(fp)
        {
            return (k.dh, k.sh, None);
        }
        let dh = tv.description.map_or([0; 16], hash_str);
        let sh = hash_json(tv.schema);
        if known.is_some_and(|k| (k.dh, k.sh) == (dh, sh)) {
            return (dh, sh, None);
        }
        let store = matches!(mode, PayloadMode::Full | PayloadMode::Masked).then_some(mode);
        let def = PendingDef {
            description: tv.description.map(str::to_owned),
            schema: if store.is_some() {
                tv.schema.and_then(|s| serde_json::to_string(s).ok())
            } else {
                None
            },
            store,
        };
        (dh, sh, Some(def))
    }

    fn observe(
        &self,
        key: ToolKey,
        tv: &ToolView<'_>,
        client: Option<&str>,
        now: i64,
        blocked: bool,
        mode: PayloadMode,
    ) {
        let fp = fingerprint(tv);
        if let Some(mut a) = self.agg.get_mut(&key) {
            a.requests += 1;
            a.blocked += u64::from(blocked);
            a.last = now;
            if let Some(c) = client
                && a.client.as_deref() != Some(c)
            {
                a.client = Some(c.into());
            }
            if a.fp != fp {
                let (dh, sh, def) = self.resolve(&key, fp, tv, mode);
                a.fp = fp;
                a.dh = dh;
                a.sh = sh;
                a.def = def;
            }
            return;
        }
        if self.agg.len() >= self.max_entries {
            self.metrics.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let origin: Box<str> = match origin_of(&tv.name) {
            Some((s, _)) => s.into(),
            None => CLIENT_ORIGIN.into(),
        };
        let (dh, sh, def) = self.resolve(&key, fp, tv, mode);
        self.agg.insert(
            key,
            Agg {
                kind: tv.kind,
                origin,
                client: client.map(Into::into),
                first: now,
                last: now,
                requests: 1,
                calls: 0,
                blocked: u64::from(blocked),
                fp,
                dh,
                sh,
                def,
            },
        );
    }

    fn record_calls(&self, base: &ToolKey, name: &str, calls: u64, blocked: u64) {
        let key = ToolKey {
            name: name.into(),
            ..base.clone()
        };
        if let Some(mut a) = self.agg.get_mut(&key) {
            a.calls += calls;
            a.blocked += blocked;
            return;
        }
        // Eintrag inzwischen geschrieben: nur Zähler mit dem bekannten Stand nachreichen.
        let Some(k) = self.known.get(&key).map(|k| *k) else {
            return;
        };
        if self.agg.len() >= self.max_entries {
            self.metrics.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let now = now_ms();
        let origin: Box<str> = origin_of(name).map_or(CLIENT_ORIGIN, |(s, _)| s).into();
        self.agg.entry(key).or_insert(Agg {
            kind: ToolKindName::Function,
            origin,
            client: None,
            first: now,
            last: now,
            requests: 0,
            calls: 0,
            blocked: 0,
            fp: k.fp.unwrap_or_default(),
            dh: k.dh,
            sh: k.sh,
            def: None,
        });
        self.record_calls(base, name, calls, blocked);
    }

    /// Schreibt alle gepufferten Einträge (Hintergrund-Task, Shutdown, Tests).
    pub async fn flush(&self) {
        let Some(sink) = &self.sink else { return };
        let _guard = self.flush_lock.lock().await;
        let keys: Vec<ToolKey> = self.agg.iter().map(|e| e.key().clone()).collect();
        if keys.is_empty() {
            return;
        }
        let mut batch = ToolBatch {
            items: Vec::with_capacity(keys.len()),
        };
        let mut fps: Vec<(ToolKey, Known)> = Vec::with_capacity(keys.len());
        for k in keys {
            let Some((key, a)) = self.agg.remove(&k) else {
                continue;
            };
            fps.push((
                key.clone(),
                Known {
                    fp: (a.requests > 0).then_some(a.fp),
                    dh: a.dh,
                    sh: a.sh,
                },
            ));
            let definition = a.def.map(|d| {
                let injection = match (&d.description, self.injection_check) {
                    (Some(text), true) => {
                        aigw_plugins::builtin::prompt_filter::injection_signals(text)
                            .into_iter()
                            .map(str::to_owned)
                            .collect()
                    }
                    _ => Vec::new(),
                };
                let mask = |s: String| match d.store {
                    Some(PayloadMode::Masked) => {
                        aigw_pii::Engine::default().mask_labels(&s).into_owned()
                    }
                    _ => s,
                };
                ToolDefinition {
                    description: d.store.and(d.description).map(mask),
                    schema: d.schema.map(mask),
                    injection,
                }
            });
            batch.items.push(ToolObservation {
                key,
                kind: a.kind,
                origin: a.origin.into(),
                client: a.client.map(Into::into),
                first_seen_ms: a.first,
                last_seen_ms: a.last,
                requests: a.requests,
                calls: a.calls,
                blocked: a.blocked,
                description_hash: hex(&a.dh),
                schema_hash: hex(&a.sh),
                definition,
            });
        }
        match sink.flush(batch).await {
            Ok(res) => {
                // Gespeicherter Stand = gesendeter Stand (mit Fingerabdruck).
                drop(res.known);
                for (k, v) in fps {
                    self.known.insert(k, v);
                }
                for ev in &res.events {
                    match ev.kind {
                        aigw_core::tools::ToolEventKind::Changed => {
                            self.metrics.changed.fetch_add(1, Ordering::Relaxed);
                            tracing::warn!(
                                tool = %ev.key.name,
                                origin = %ev.origin,
                                blocked_by = ?ev.blocked_by,
                                "Tool-Inventar: Beschreibung oder Schema eines bekannten Tools geändert"
                            );
                        }
                        aigw_core::tools::ToolEventKind::Suspicious => {
                            self.metrics.suspicious.fetch_add(1, Ordering::Relaxed);
                            tracing::warn!(
                                tool = %ev.key.name,
                                signals = ?ev.signals,
                                "Tool-Inventar: Beschreibung mit Prompt-Injection-Signalen"
                            );
                        }
                        aigw_core::tools::ToolEventKind::Discovered => {
                            tracing::debug!(tool = %ev.key.name, "Tool-Inventar: neues Tool");
                        }
                    }
                    if let Some(h) = &self.hook {
                        h(ev);
                    }
                }
            }
            Err(e) => {
                self.metrics.flush_errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(error = %e, "Tool-Inventar: Schreiben fehlgeschlagen (Batch verworfen)");
            }
        }
    }

    /// Startet das periodische Schreiben (`interval`).
    pub fn spawn(self: &Arc<Self>, interval: Duration) -> ToolInventoryTask {
        let me = Arc::clone(self);
        ToolInventoryTask(tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval.max(Duration::from_millis(50)));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                me.flush().await;
            }
        }))
    }
}

/// Hintergrund-Task von [`ToolInventory::spawn`]; wird beim Fallenlassen beendet.
#[derive(Debug)]
pub struct ToolInventoryTask(tokio::task::JoinHandle<()>);

impl Drop for ToolInventoryTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// ---------------------------------------------------------------------------------------------
// Schritt in der Pipeline
// ---------------------------------------------------------------------------------------------

/// Zustand einer Anfrage mit Tools: Entscheidungen für die Antwort, Zählen der Aufrufe.
pub(crate) struct ToolGuard {
    snapshot: Option<Arc<Snapshot>>,
    organization: Option<String>,
    team: Option<String>,
    api_key: Option<String>,
    model_group: Option<String>,
    inventory: Option<Arc<ToolInventory>>,
    metrics: Arc<ToolMetrics>,
    base: ToolKey,
    /// Nicht erlaubte Tool-Namen (Entscheidungen aus der Anfrage).
    denied: HashSet<Box<str>>,
    /// Erlaubte Tool-Namen (aus der Anfrage, ohne Richtlinie im Hot Path neu zu prüfen).
    allowed: HashSet<Box<str>>,
}

impl fmt::Debug for ToolGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolGuard")
            .field("denied", &self.denied)
            .finish_non_exhaustive()
    }
}

/// Wendet Richtlinien und Inventar auf die Tools der Anfrage an. `None`: keine Tools.
pub(crate) fn begin(
    inventory: &Arc<ToolInventory>,
    auth: &AuthContext,
    model_group: &str,
    headers: &HeaderMap,
    req: &mut ChatRequest,
    mode: PayloadMode,
    request_id: &str,
) -> Result<Option<ToolGuard>, ApiError> {
    if req.tools.is_empty() {
        return Ok(None);
    }
    let key = auth.key.as_deref();
    let mut g = ToolGuard {
        snapshot: auth.snapshot.clone(),
        organization: key.and_then(|k| k.organization.as_ref().map(|o| o.metadata.name.clone())),
        team: key.and_then(|k| k.team.as_ref().map(|t| t.metadata.name.clone())),
        api_key: key.map(|k| k.key.metadata.name.clone()),
        model_group: Some(model_group.to_owned()),
        inventory: inventory.enabled().then(|| Arc::clone(inventory)),
        metrics: inventory.metrics(),
        base: ToolKey {
            organization: auth.organization_id(),
            team: auth.team_id(),
            api_key: auth.api_key_id(),
            name: "".into(),
        },
        denied: HashSet::new(),
        allowed: HashSet::new(),
    };
    let index = g.snapshot.as_ref().map(|s| Arc::clone(&s.tool_index));
    let ctx = g.ctx();
    let active = index.as_ref().filter(|i| i.any_applies(&ctx));
    let mut rejected: Vec<(String, Option<Arc<str>>)> = Vec::new();
    let mut strip: Vec<bool> = Vec::with_capacity(req.tools.len());
    let now = now_ms();
    let mut allowed: HashSet<Box<str>> = HashSet::new();
    let mut denied: HashSet<Box<str>> = HashSet::new();
    let client = g.inventory.as_ref().and_then(|i| i.client(headers));
    for t in &req.tools {
        let tv = view(t);
        let effect = match active {
            Some(idx) => {
                let v = idx.decide(&ctx, &tv.name);
                if let Some(p) = &v.observed {
                    g.metrics.observed.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(request_id, tool = %tv.name, policy = %p, "Tool-Richtlinie (observe): Tool in Anfrage");
                }
                if v.effect != Effect::Allow {
                    tracing::info!(
                        request_id,
                        tool = %tv.name,
                        policy = ?v.policy,
                        unapproved = v.reason == Reason::Unapproved,
                        effect = ?v.effect,
                        "Tool-Richtlinie greift"
                    );
                }
                if v.effect == Effect::Reject {
                    rejected.push((tv.name.to_string(), v.policy.clone()));
                }
                v.effect
            }
            None => Effect::Allow,
        };
        if effect == Effect::Allow {
            allowed.insert(tv.name.as_ref().into());
        } else {
            denied.insert(tv.name.as_ref().into());
        }
        strip.push(effect == Effect::Strip);
        if let Some(inv) = &g.inventory {
            let key = ToolKey {
                name: tv.name.as_ref().into(),
                ..g.base.clone()
            };
            inv.observe(
                key,
                &tv,
                client.as_deref(),
                now,
                effect != Effect::Allow,
                mode,
            );
        }
    }
    g.allowed = allowed;
    g.denied = denied;
    if !rejected.is_empty() {
        g.metrics.rejected.fetch_add(1, Ordering::Relaxed);
        let names: Vec<&str> = rejected.iter().map(|(n, _)| n.as_str()).collect();
        let policies: Vec<&str> = rejected
            .iter()
            .filter_map(|(_, p)| p.as_deref())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        return Err(ApiError::new(
            ErrorKind::PermissionDenied,
            format!(
                "Tool(s) {} durch Tool-Richtlinie gesperrt ({})",
                names.join(", "),
                policies.join(", ")
            ),
        )
        .with_param("tools")
        .with_code("tool_blocked"));
    }
    if strip.iter().any(|s| *s) {
        let n = strip.iter().filter(|s| **s).count();
        g.metrics.stripped.fetch_add(n as u64, Ordering::Relaxed);
        let mut it = strip.iter();
        req.tools.retain(|_| !it.next().copied().unwrap_or(false));
        adjust_tool_choice(req, &g.denied);
    }
    Ok(Some(g))
}

/// Wie der Pipeline-Schritt (Richtlinien und Inventar auf die Tools der Anfrage), ohne
/// Antwortprüfung – für Benchmarks und Tests. `true`, wenn die Anfrage Tools hatte.
pub fn apply_request(
    inventory: &Arc<ToolInventory>,
    auth: &AuthContext,
    model_group: &str,
    headers: &HeaderMap,
    req: &mut ChatRequest,
    mode: PayloadMode,
) -> Result<bool, ApiError> {
    begin(inventory, auth, model_group, headers, req, mode, "").map(|g| g.is_some())
}

/// `tool_choice` nach dem Entfernen von Tools: ohne Tools entfallen `tool_choice` und
/// `parallel_tool_calls`; ein erzwungenes, entferntes Tool wird zu `auto`.
fn adjust_tool_choice(req: &mut ChatRequest, denied: &HashSet<Box<str>>) {
    if req.tools.is_empty() {
        req.tool_choice = None;
        req.parallel_tool_calls = None;
        return;
    }
    if let Some(ToolChoice::Tool { name }) = &req.tool_choice
        && denied.contains(name.as_str())
    {
        req.tool_choice = Some(ToolChoice::Auto);
    }
}

impl ToolGuard {
    fn ctx(&self) -> ToolCtx<'_> {
        ToolCtx {
            organization: self.organization.as_deref(),
            team: self.team.as_deref(),
            api_key: self.api_key.as_deref(),
            model_group: self.model_group.as_deref(),
        }
    }

    /// Darf das Modell `name` aufrufen?
    fn call_allowed(&self, name: &str) -> bool {
        if self.allowed.contains(name) {
            return true;
        }
        if self.denied.contains(name) {
            return false;
        }
        // Aufruf eines Tools, das nicht in der Anfrage stand (z. B. eingebautes Tool).
        self.snapshot
            .as_ref()
            .is_none_or(|s| s.tool_index.decide(&self.ctx(), name).effect == Effect::Allow)
    }

    fn count(&self, name: &str, blocked: bool) {
        if blocked {
            self.metrics.calls_blocked.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(inv) = &self.inventory {
            inv.record_calls(&self.base, name, 1, u64::from(blocked));
        }
    }

    /// Antwort (nicht gestreamt): Aufrufe zählen; enthält sie Aufrufe nicht erlaubter Tools,
    /// wird sie wie eine Ausgabe-Ablehnung gefiltert (`true`).
    pub(crate) fn check_response(&self, resp: &mut ChatResponse) -> bool {
        let mut blocked = false;
        for c in &resp.choices {
            if let Content::Parts(parts) = &c.message.content {
                for p in parts {
                    if let PartKind::ToolCall { name, .. } = &p.kind {
                        let ok = self.call_allowed(name);
                        self.count(name, !ok);
                        blocked |= !ok;
                    }
                }
            }
        }
        if blocked {
            tracing::info!("Tool-Richtlinie: Tool-Aufruf in der Antwort unterbunden");
            aigw_plugins::content_filtered(resp);
        }
        blocked
    }

    /// Stream: Aufrufe zählen und Aufrufe nicht erlaubter Tools unterbinden.
    pub(crate) fn wrap_stream(self, events: EventStream) -> EventStream {
        Box::pin(GuardStream {
            inner: events,
            guard: self,
            blocked: false,
        })
    }
}

/// Header `x-aigw-guardrail: tool-policy`.
pub(crate) fn guardrail_header(h: &mut aigw_plugins::ResponseHeaders) {
    h.insert(
        HeaderName::from_static(aigw_plugins::GUARDRAIL_HEADER),
        HeaderValue::from_static(TOOL_GUARDRAIL),
    );
}

struct GuardStream {
    inner: EventStream,
    guard: ToolGuard,
    /// Ab hier keine Inhalte mehr; `Stop` wird zu `content_filter`.
    blocked: bool,
}

impl Stream for GuardStream {
    type Item = StreamEvent;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<StreamEvent>> {
        use std::task::Poll;
        loop {
            let ev = match self.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(ev)) => ev,
                other => return other,
            };
            if let StreamEvent::ToolCallStart(s) = &ev {
                let ok = self.guard.call_allowed(&s.name);
                self.guard.count(&s.name, !ok);
                if !ok && !self.blocked {
                    tracing::info!(tool = %s.name, "Tool-Richtlinie: Tool-Aufruf im Stream unterbunden");
                    self.blocked = true;
                }
            }
            if !self.blocked {
                return Poll::Ready(Some(ev));
            }
            match ev {
                StreamEvent::Stop { .. } => {
                    return Poll::Ready(Some(StreamEvent::Stop {
                        reason: StopReason::ContentFilter,
                        stop_sequence: None,
                    }));
                }
                ev @ (StreamEvent::Start { .. }
                | StreamEvent::Usage(_)
                | StreamEvent::Error(_)
                | StreamEvent::Choice { .. }) => return Poll::Ready(Some(ev)),
                // Inhalte (Text, Reasoning, Tool-Aufrufe) verwerfen.
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigw_core::ir::{FunctionTool, Tool};
    use serde_json::json;

    fn tool(name: &str, desc: &str) -> Tool {
        Tool {
            kind: ToolKind::Function(FunctionTool {
                name: name.into(),
                description: Some(desc.into()),
                parameters: Some(
                    json!({"type": "object", "properties": {"q": {"type": "string"}}}),
                ),
                strict: None,
                extra: Default::default(),
            }),
            cache_control: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn hashes_are_stable_and_sensitive() {
        let a = json!({"b": 1, "a": [true, null, "x"]});
        let b = json!({"a": [true, null, "x"], "b": 1});
        assert_eq!(hash_json(Some(&a)), hash_json(Some(&b)));
        assert_ne!(
            hash_json(Some(&a)),
            hash_json(Some(&json!({"b": 2, "a": [true, null, "x"]})))
        );
        assert_ne!(
            hash_json(Some(&json!("ab"))),
            hash_json(Some(&json!(["a", "b"])))
        );
        assert_eq!(hex(&hash_str("x")).len(), 32);
        assert_eq!(parse_hex(&hex(&hash_str("x"))), Some(hash_str("x")));
    }

    #[test]
    fn opaque_views() {
        let t = Tool {
            kind: ToolKind::Opaque {
                format: aigw_core::wire::WireFormat::AnthropicMessages,
                value: json!({"type": "web_search_20250305", "name": "web_search"}),
            },
            cache_control: None,
            extra: Default::default(),
        };
        let v = view(&t);
        assert_eq!(
            (v.name.as_ref(), v.kind),
            ("web_search", ToolKindName::ServerTool)
        );
        let t = Tool {
            kind: ToolKind::Opaque {
                format: aigw_core::wire::WireFormat::OpenAiResponses,
                value: json!({"type": "code_interpreter"}),
            },
            cache_control: None,
            extra: Default::default(),
        };
        let v = view(&t);
        assert_eq!(
            (v.name.as_ref(), v.kind),
            ("code_interpreter", ToolKindName::Builtin)
        );
    }

    #[test]
    fn tool_choice_adjusted() {
        let mut req = ChatRequest {
            tools: vec![tool("a", "")],
            tool_choice: Some(ToolChoice::Tool { name: "b".into() }),
            ..ChatRequest::default()
        };
        let denied: HashSet<Box<str>> = ["b".into()].into_iter().collect();
        adjust_tool_choice(&mut req, &denied);
        assert_eq!(req.tool_choice, Some(ToolChoice::Auto));
        req.tools.clear();
        req.parallel_tool_calls = Some(true);
        adjust_tool_choice(&mut req, &denied);
        assert_eq!(req.tool_choice, None);
        assert_eq!(req.parallel_tool_calls, None);
    }

    #[derive(Debug, Default)]
    struct MemSink(std::sync::Mutex<Vec<ToolBatch>>);

    impl ToolInventorySink for MemSink {
        fn flush<'a>(
            &'a self,
            batch: ToolBatch,
        ) -> aigw_core::tools::BoxFuture<'a, Result<aigw_core::tools::ToolFlushResult, String>>
        {
            let known = batch
                .items
                .iter()
                .map(|i| {
                    (
                        i.key.clone(),
                        (i.description_hash.clone(), i.schema_hash.clone()),
                    )
                })
                .collect();
            self.0.lock().unwrap().push(batch);
            Box::pin(async move {
                Ok(aigw_core::tools::ToolFlushResult {
                    known,
                    events: Vec::new(),
                })
            })
        }
    }

    #[tokio::test]
    async fn inventory_aggregates_and_sends_definitions_once() {
        let sink = Arc::new(MemSink::default());
        let inv = Arc::new(ToolInventory::new(
            sink.clone(),
            &aigw_core::config::ToolsConfig::default(),
        ));
        let mut headers = HeaderMap::new();
        headers.insert("user-agent", HeaderValue::from_static("claude-cli/1.0"));
        let auth = AuthContext::default();
        for _ in 0..3 {
            let mut req = ChatRequest {
                tools: vec![tool("mcp__gh__search", "Sucht."), tool("calc", "Rechnet.")],
                ..ChatRequest::default()
            };
            let g = begin(&inv, &auth, "m", &headers, &mut req, PayloadMode::Off, "r")
                .unwrap()
                .unwrap();
            let mut resp = ChatResponse {
                choices: vec![aigw_core::ir::Choice {
                    index: 0,
                    message: aigw_core::ir::Message {
                        content: Content::Parts(vec![aigw_core::ir::Part::new(
                            PartKind::ToolCall {
                                id: "1".into(),
                                name: "calc".into(),
                                arguments: "{}".into(),
                            },
                        )]),
                        ..aigw_core::ir::Message::text(aigw_core::ir::Role::Assistant, "")
                    },
                    stop_reason: None,
                    stop_sequence: None,
                    extra: Default::default(),
                }],
                ..ChatResponse::default()
            };
            assert!(!g.check_response(&mut resp));
        }
        inv.flush().await;
        {
            let batches = sink.0.lock().unwrap();
            assert_eq!(batches.len(), 1);
            let items = &batches[0].items;
            assert_eq!(items.len(), 2);
            let gh = items
                .iter()
                .find(|i| &*i.key.name == "mcp__gh__search")
                .unwrap();
            assert_eq!((gh.requests, gh.calls), (3, 0));
            assert_eq!(gh.origin, "gh");
            assert_eq!(gh.client.as_deref(), Some("claude-cli/1.0"));
            // payloadMode off: Beschreibung nicht speichern, aber geprüft.
            assert_eq!(gh.definition.as_ref().unwrap().description, None);
            let calc = items.iter().find(|i| &*i.key.name == "calc").unwrap();
            assert_eq!(
                (calc.requests, calc.calls, calc.origin.as_str()),
                (3, 3, "client")
            );
        }
        // Zweiter Durchlauf: Hashes bekannt → keine Definition mehr.
        let mut req = ChatRequest {
            tools: vec![tool("calc", "Rechnet.")],
            ..ChatRequest::default()
        };
        begin(&inv, &auth, "m", &headers, &mut req, PayloadMode::Full, "r").unwrap();
        inv.flush().await;
        assert!(sink.0.lock().unwrap()[1].items[0].definition.is_none());
        // Geänderte Beschreibung → Definition wieder (mit Inhalt bei payloadMode full).
        let mut req = ChatRequest {
            tools: vec![tool("calc", "Rechnet anders.")],
            ..ChatRequest::default()
        };
        begin(&inv, &auth, "m", &headers, &mut req, PayloadMode::Full, "r").unwrap();
        inv.flush().await;
        let b = sink.0.lock().unwrap();
        let d = b[2].items[0].definition.as_ref().unwrap();
        assert_eq!(d.description.as_deref(), Some("Rechnet anders."));
        assert!(d.schema.is_some());
    }
}
