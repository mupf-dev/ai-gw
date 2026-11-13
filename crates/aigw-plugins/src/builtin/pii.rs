// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Plugin `pii`: personenbezogene Daten und Geheimnisse erkennen (Engine `aigw-pii`) und je
//! Kategorie maskieren, entfernen, ablehnen, pseudonymisieren oder nur zählen.
//!
//! ```yaml
//! plugin: pii
//! config:
//!   action: mask                 # Standard für alle Kategorien: mask | redact | reject |
//!                                #   pseudonymize | log-only
//!   direction: both              # input | output | both
//!   categories:                  # ohne Angabe: alle Kategorien mit `action`
//!     email: pseudonymize
//!     creditCard: reject
//!     ipAddress: {action: log-only, direction: input}
//!   allow: ["support@acme.example", "*@example.com"]
//!   entropy: {enabled: true, minLength: 32, minBits: 4.0}   # API-Key-Heuristik (abschaltbar, F164)
//!   streamWindowBytes: 128       # Zurückhalten im Stream (nur bei Ausgabe-Erkennung)
//! ```
//!
//! - `mask`: `[EMAIL_1]` (je Anfrage und Wert stabil nummeriert, nicht rückübersetzt),
//!   `redact`: `[REDACTED]`, `reject`: [`crate::Flow::Block`] mit `pii_detected` und Kategorie
//!   (ohne Fundstelle) – Eingabe `400`, Ausgabe je `outputRejectMode` `200` mit
//!   `content_filter` (Standard) oder `400` (F160),
//!   `pseudonymize`: wie `mask`, aber die Antwort (JSON, Stream) bekommt die Originalwerte
//!   zurück, `log-only`: nur zählen.
//! - Zuordnung Platzhalter → Original liegt nur im Speicher der Anfrage (`Meta`, Schlüssel
//!   [`META_KEY`]) und wird nie geloggt.
//! - Stream: Platzhalter über Chunk-Grenzen werden zurückgehalten, bis sie vollständig sind;
//!   mit Ausgabe-Erkennung bleiben zusätzlich die letzten `streamWindowBytes` zurück (Funde
//!   über Chunk-Grenzen hinweg).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use aigw_core::ir::{ApiError, Content, PartKind, StreamEvent};
use aigw_pii::{Category, CategorySet, Engine, EngineConfig, EntropyConfig, Finding};
use serde::Deserialize;
use serde_json::{Map, Value};

use super::guard::{Direction, floor_boundary, visit_mut};
use crate::binding::FailMode;
use crate::plugin::{
    CallInfo, ChunkCtx, ChunkFlow, Flow, Hook, Meta, Phases, Plugin, PluginError, ReqCtx, RespCtx,
};
use crate::registry::PluginFactory;

/// Schlüssel in [`Meta`]: Zähler und Pseudonym-Zuordnung (nur für eingebaute Plugins; nie an
/// externe Dienste weitergeben).
pub const META_KEY: &str = "aigw.pii";
const NAME: &str = "pii";
/// Längster Platzhalter (`[URL_CREDENTIALS_99999]` = 23 Bytes) mit Reserve.
const MAX_PLACEHOLDER: usize = 32;
const REDACTED: &str = "[REDACTED]";

/// Aktion je Kategorie.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    /// Typ-Platzhalter `[EMAIL_1]`.
    Mask,
    /// `[REDACTED]`.
    Redact,
    /// Ablehnen (`400 pii_detected`).
    Reject,
    /// Platzhalter mit Rückübersetzung in der Antwort.
    Pseudonymize,
    /// Nur zählen.
    LogOnly,
}

impl Action {
    /// Label (Metrik `action`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mask => "mask",
            Self::Redact => "redact",
            Self::Reject => "reject",
            Self::Pseudonymize => "pseudonymize",
            Self::LogOnly => "log-only",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum CategoryRule {
    Action(Action),
    Detailed(Detailed),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Detailed {
    #[serde(default)]
    action: Option<Action>,
    #[serde(default)]
    direction: Option<Direction>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EntropyCfg {
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default)]
    min_length: Option<usize>,
    #[serde(default)]
    min_bits: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Config {
    #[serde(default)]
    action: Option<Action>,
    #[serde(default)]
    direction: Option<Direction>,
    #[serde(default)]
    categories: Option<BTreeMap<String, CategoryRule>>,
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    entropy: Option<EntropyCfg>,
    #[serde(default)]
    stream_window_bytes: Option<usize>,
}

/// Plugin `pii`.
#[derive(Debug, Clone)]
pub struct Pii {
    rules: [Option<(Action, Direction)>; Category::ALL.len()],
    input: Engine,
    output: Engine,
    restore: bool,
    window: usize,
}

impl Pii {
    fn from_config(c: Config) -> Result<Self, String> {
        let action = c.action.unwrap_or(Action::Mask);
        let direction = c.direction.unwrap_or_default();
        let mut rules = [None; Category::ALL.len()];
        match &c.categories {
            None => {
                for cat in Category::ALL {
                    rules[cat as usize] = Some((action, direction));
                }
            }
            Some(map) => {
                if map.is_empty() {
                    return Err("categories: mindestens eine Kategorie angeben".into());
                }
                for (name, rule) in map {
                    let cat = Category::parse(name).ok_or_else(|| {
                        format!(
                            "categories: unbekannte Kategorie {name:?} (bekannt: {})",
                            Category::ALL.map(Category::as_str).join(", ")
                        )
                    })?;
                    let (a, d) = match rule {
                        CategoryRule::Action(a) => (*a, direction),
                        CategoryRule::Detailed(d) => {
                            (d.action.unwrap_or(action), d.direction.unwrap_or(direction))
                        }
                    };
                    rules[cat as usize] = Some((a, d));
                }
            }
        }
        let mut input = CategorySet::EMPTY;
        let mut output = CategorySet::EMPTY;
        let mut restore = false;
        for cat in Category::ALL {
            let Some((a, d)) = rules[cat as usize] else {
                continue;
            };
            if a == Action::Pseudonymize {
                if !d.input() {
                    return Err(format!(
                        "categories.{}: pseudonymize braucht die Richtung input oder both",
                        cat.as_str()
                    ));
                }
                restore = true;
                input.insert(cat);
                continue;
            }
            if d.input() {
                input.insert(cat);
            }
            if d.output() {
                output.insert(cat);
            }
        }
        let entropy = match &c.entropy {
            Some(e) if !e.enabled => None,
            Some(e) => {
                let mut cfg = EntropyConfig::default();
                if let Some(l) = e.min_length {
                    if !(16..=256).contains(&l) {
                        return Err("entropy.minLength muss zwischen 16 und 256 liegen".into());
                    }
                    cfg.min_len = l;
                }
                if let Some(b) = e.min_bits {
                    if !(b.is_finite() && (2.0..=6.0).contains(&b)) {
                        return Err("entropy.minBits muss zwischen 2 und 6 liegen".into());
                    }
                    cfg.min_bits = b;
                }
                Some(cfg)
            }
            None => Some(EntropyConfig::default()),
        };
        let window = c.stream_window_bytes.unwrap_or(128);
        if !(16..=4096).contains(&window) {
            return Err("streamWindowBytes muss zwischen 16 und 4096 liegen".into());
        }
        let engine = |categories| {
            Engine::new(EngineConfig {
                categories,
                allow: c.allow.clone(),
                entropy,
            })
        };
        Ok(Self {
            rules,
            input: engine(input),
            output: engine(output),
            restore,
            window,
        })
    }

    fn action(&self, c: Category) -> Action {
        self.rules[c as usize].map_or(Action::LogOnly, |(a, _)| a)
    }

    fn detects_output(&self) -> bool {
        !self.output.categories().is_empty()
    }

    /// Prüft und ersetzt `text`; `Err(Kategorie)` bei `reject`.
    fn apply(
        &self,
        engine: &Engine,
        text: &str,
        st: &mut Pseudo,
        info: &CallInfo,
        output: bool,
    ) -> Result<Option<String>, Category> {
        let found = engine.find(text);
        if found.is_empty() {
            return Ok(None);
        }
        if let Some(f) = found
            .iter()
            .find(|f| self.action(f.category) == Action::Reject)
        {
            info.guardrails
                .add(NAME, f.category.as_str(), Action::Reject.as_str(), 1);
            return Err(f.category);
        }
        let mut changed = false;
        let out = rewrite(text, &found, |f, value, out| {
            let action = self.action(f.category);
            info.guardrails
                .add(NAME, f.category.as_str(), action.as_str(), 1);
            match action {
                Action::Mask | Action::Pseudonymize => {
                    // In der Ausgabe nie in die Rückübersetzung aufnehmen.
                    let keep = action == Action::Pseudonymize && !output;
                    out.push_str(&st.placeholder(f.category, value, keep));
                    changed = true;
                }
                Action::Redact => {
                    out.push_str(REDACTED);
                    changed = true;
                }
                Action::LogOnly | Action::Reject => out.push_str(value),
            }
        });
        Ok(changed.then_some(out))
    }

    /// Ausgabe: erkennen/ersetzen, dann Platzhalter zurückübersetzen.
    fn process_output(
        &self,
        text: &str,
        st: &mut Pseudo,
        info: &CallInfo,
    ) -> Result<Option<String>, Category> {
        let replaced = if self.detects_output() {
            self.apply(&self.output, text, st, info, true)?
        } else {
            None
        };
        if !self.restore || st.map.is_empty() {
            return Ok(replaced);
        }
        let base = replaced.as_deref().unwrap_or(text);
        Ok(st.restore(base).or(replaced))
    }

    /// Wie viel vom Stream-Puffer darf jetzt hinaus (Byte-Index auf Zeichengrenze)?
    fn cut(&self, buf: &str) -> usize {
        let mut cut = buf.len();
        if self.detects_output() {
            let limit = floor_boundary(buf, buf.len().saturating_sub(self.window));
            cut = match buf[..limit].rfind(char::is_whitespace) {
                Some(p) => p + 1, // Leerzeichen sind ASCII (1 Byte) – Zeichengrenze
                None => limit,
            };
            if cut > 0 {
                for f in self.output.find(buf) {
                    if f.start < cut && cut < f.end {
                        cut = f.start;
                    }
                }
            }
        }
        while !buf.is_char_boundary(cut) {
            cut -= 1;
        }
        if self.restore
            && let Some(q) = buf[..cut].rfind('[')
        {
            let tail = &buf[q + 1..cut];
            if cut - q < MAX_PLACEHOLDER
                && tail
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
            {
                cut = q;
            }
        }
        while !buf.is_char_boundary(cut) {
            cut -= 1;
        }
        cut
    }

    fn reject_error(cat: Category, output: bool) -> ApiError {
        let msg = if output {
            format!(
                "Die Antwort enthielt personenbezogene Daten (Kategorie: {cat}) und wurde blockiert"
            )
        } else {
            format!("Die Anfrage enthält personenbezogene Daten (Kategorie: {cat})")
        };
        ApiError::invalid_request(msg).with_code("pii_detected")
    }
}

/// Ersetzt die Fundstellen in `text`.
fn rewrite(
    text: &str,
    found: &[Finding],
    mut f: impl FnMut(&Finding, &str, &mut String),
) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut last = 0;
    for fd in found {
        out.push_str(&text[last..fd.start]);
        f(fd, &text[fd.start..fd.end], &mut out);
        last = fd.end;
    }
    out.push_str(&text[last..]);
    out
}

/// Platzhalter-Zustand einer Anfrage (geteilt über [`Meta`] zwischen Bindungen und Phasen).
#[derive(Debug, Default, Clone)]
struct Pseudo {
    counters: HashMap<String, u64>,
    /// Wert → Platzhalter (je Kategorie), nur innerhalb dieses Laufs.
    by_value: HashMap<(Category, String), String>,
    /// Platzhalter → Original (Pseudonymisierung).
    map: HashMap<String, String>,
    dirty: bool,
}

impl Pseudo {
    fn load(meta: &Meta) -> Self {
        let mut p = Self::default();
        let Some(Value::Object(o)) = meta.get(META_KEY) else {
            return p;
        };
        if let Some(Value::Object(c)) = o.get("counters") {
            for (k, v) in c {
                if let Some(n) = v.as_u64() {
                    p.counters.insert(k.clone(), n);
                }
            }
        }
        if let Some(Value::Object(m)) = o.get("map") {
            for (k, v) in m {
                if let Some(s) = v.as_str() {
                    p.map.insert(k.clone(), s.to_owned());
                }
            }
        }
        p
    }

    fn save(&self, meta: &mut Meta) {
        if !self.dirty {
            return;
        }
        let counters: Map<String, Value> = self
            .counters
            .iter()
            .map(|(k, v)| (k.clone(), Value::from(*v)))
            .collect();
        let map: Map<String, Value> = self
            .map
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        let mut o = Map::new();
        o.insert("counters".into(), Value::Object(counters));
        o.insert("map".into(), Value::Object(map));
        meta.insert(META_KEY.into(), Value::Object(o));
    }

    fn placeholder(&mut self, cat: Category, value: &str, keep: bool) -> String {
        if let Some(p) = self.by_value.get(&(cat, value.to_owned())) {
            return p.clone();
        }
        // Wert schon von einer früheren Bindung pseudonymisiert?
        if keep
            && let Some((p, _)) = self
                .map
                .iter()
                .find(|(p, v)| v.as_str() == value && p.starts_with(&format!("[{}_", cat.label())))
        {
            return p.clone();
        }
        let n = self.counters.entry(cat.label().to_owned()).or_insert(0);
        *n += 1;
        let p = format!("[{}_{}]", cat.label(), n);
        self.by_value.insert((cat, value.to_owned()), p.clone());
        if keep {
            self.map.insert(p.clone(), value.to_owned());
        }
        self.dirty = true;
        p
    }

    /// Ersetzt bekannte Platzhalter durch die Originale (`None` = nichts zu tun).
    fn restore(&self, text: &str) -> Option<String> {
        if !text.contains('[') {
            return None;
        }
        let mut out = String::with_capacity(text.len());
        let mut last = 0;
        let mut changed = false;
        let mut i = 0;
        while let Some(q) = text[i..].find('[').map(|q| q + i) {
            let end = text[q..]
                .char_indices()
                .take(MAX_PLACEHOLDER)
                .find(|(_, c)| *c == ']')
                .map(|(k, _)| q + k + 1);
            match end.and_then(|e| self.map.get(&text[q..e]).map(|v| (e, v))) {
                Some((e, v)) => {
                    out.push_str(&text[last..q]);
                    out.push_str(v);
                    last = e;
                    i = e;
                    changed = true;
                }
                None => i = q + 1,
            }
        }
        if !changed {
            return None;
        }
        out.push_str(&text[last..]);
        Some(out)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    Text(u32),
    Tool(u32),
}

impl Block {
    fn event(self, text: String) -> StreamEvent {
        match self {
            Self::Text(index) => StreamEvent::TextDelta { index, text },
            Self::Tool(index) => StreamEvent::ToolCallDelta {
                index,
                arguments: text,
            },
        }
    }
}

#[derive(Debug, Default)]
struct StreamState {
    init: bool,
    pseudo: Pseudo,
    block: Option<Block>,
    buf: String,
}

impl Pii {
    /// Gibt den ganzen Puffer aus (verarbeitet) – `Err` bei `reject`.
    fn flush(
        &self,
        st: &mut StreamState,
        info: &CallInfo,
    ) -> Result<Option<StreamEvent>, Category> {
        let Some(block) = st.block else {
            return Ok(None);
        };
        if st.buf.is_empty() {
            return Ok(None);
        }
        let text = std::mem::take(&mut st.buf);
        let out = self
            .process_output(&text, &mut st.pseudo, info)?
            .unwrap_or(text);
        Ok(Some(block.event(out)))
    }

    fn on_delta(
        &self,
        st: &mut StreamState,
        info: &CallInfo,
        block: Block,
        text: &mut String,
    ) -> (Option<StreamEvent>, Result<ChunkFlow, Category>) {
        let mut before = None;
        if st.block != Some(block) {
            match self.flush(st, info) {
                Ok(e) => before = e,
                Err(c) => return (None, Err(c)),
            }
            st.block = Some(block);
        }
        st.buf.push_str(text);
        let cut = self.cut(&st.buf);
        if cut == 0 {
            return (before, Ok(ChunkFlow::Drop));
        }
        let head: String = st.buf.drain(..cut).collect();
        match self.process_output(&head, &mut st.pseudo, info) {
            Ok(out) => {
                *text = out.unwrap_or(head);
                (before, Ok(ChunkFlow::Continue))
            }
            Err(c) => (before, Err(c)),
        }
    }
}

/// AIGW-58: Lesbares Reasoning entfernen (nicht schwärzbar, siehe `on_chunk`).
/// Verschlüsseltes Reasoning (`RedactedReasoning`) bleibt – der Client kann es nicht lesen.
fn strip_reasoning(c: &mut Content) {
    if let Content::Parts(parts) = c {
        parts.retain(|p| !matches!(p.kind, PartKind::Reasoning { .. }));
    }
}

impl Plugin for Pii {
    fn name(&self) -> &'static str {
        NAME
    }

    fn phases(&self) -> Phases {
        let out = self.detects_output() || self.restore;
        Phases {
            request: !self.input.categories().is_empty(),
            response: out,
            chunk: out,
        }
    }

    fn on_request<'a>(&'a self, ctx: &'a mut ReqCtx<'_>) -> Hook<'a, Flow> {
        let info = ctx.info;
        let mut st = Pseudo::load(&ctx.meta);
        let mut rejected = None;
        for m in &mut ctx.request.messages {
            let r = visit_mut(&mut m.content, &mut |text: &mut String| {
                if let Some(new) = self.apply(&self.input, text, &mut st, info, false)? {
                    *text = new;
                }
                Ok::<(), Category>(())
            });
            if let Err(c) = r {
                rejected = Some(c);
                break;
            }
        }
        if let Some(c) = rejected {
            return Hook::ok(Flow::Block(Self::reject_error(c, false)));
        }
        st.save(&mut ctx.meta);
        Hook::ok(Flow::Continue)
    }

    fn on_response<'a>(&'a self, ctx: &'a mut RespCtx<'_>) -> Hook<'a, Flow> {
        let info = ctx.info;
        let mut st = Pseudo::load(ctx.meta);
        for choice in &mut ctx.response.choices {
            if self.detects_output() {
                strip_reasoning(&mut choice.message.content);
            }
            let r = visit_mut(&mut choice.message.content, &mut |text: &mut String| {
                if let Some(new) = self.process_output(text, &mut st, info)? {
                    *text = new;
                }
                Ok::<(), Category>(())
            });
            if let Err(c) = r {
                return Hook::ok(Flow::Block(Self::reject_error(c, true)));
            }
        }
        st.save(ctx.meta);
        Hook::ok(Flow::Continue)
    }

    fn on_chunk(
        &self,
        ctx: &mut ChunkCtx<'_>,
        ev: &mut StreamEvent,
    ) -> Result<ChunkFlow, PluginError> {
        let info = ctx.info;
        if !ctx.state::<StreamState>().init {
            let p = Pseudo::load(ctx.meta);
            let st = ctx.state::<StreamState>();
            st.pseudo = p;
            st.init = true;
        }
        let st = ctx.state::<StreamState>();
        let (before, res) = match ev {
            StreamEvent::TextDelta { index, text } => {
                self.on_delta(st, info, Block::Text(*index), text)
            }
            StreamEvent::ToolCallDelta { index, arguments } => {
                self.on_delta(st, info, Block::Tool(*index), arguments)
            }
            // AIGW-58: Reasoning lässt sich nicht schwärzen (Signaturen der Anbieter) – bei
            // Ausgabeprüfung wird es deshalb verworfen, sonst gingen PII ungeprüft hinaus.
            StreamEvent::ReasoningDelta { .. } | StreamEvent::ReasoningSignature { .. } => {
                if self.detects_output() {
                    return Ok(ChunkFlow::Drop);
                }
                (None, Ok(ChunkFlow::Continue))
            }
            _ => match self.flush(st, info) {
                Ok(e) => {
                    st.block = None;
                    (e, Ok(ChunkFlow::Continue))
                }
                Err(c) => (None, Err(c)),
            },
        };
        if let Some(e) = before {
            ctx.emit(e);
        }
        Ok(match res {
            Ok(flow) => flow,
            Err(c) => ChunkFlow::Block(Self::reject_error(c, true)),
        })
    }

    fn on_stream_end(&self, ctx: &mut ChunkCtx<'_>) -> Result<ChunkFlow, PluginError> {
        let info = ctx.info;
        let st = ctx.state::<StreamState>();
        match self.flush(st, info) {
            Ok(Some(e)) => {
                ctx.emit(e);
                Ok(ChunkFlow::Continue)
            }
            Ok(None) => Ok(ChunkFlow::Continue),
            Err(c) => Ok(ChunkFlow::Block(Self::reject_error(c, true))),
        }
    }
}

/// Fabrik von [`Pii`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Factory;

impl PluginFactory for Factory {
    fn name(&self) -> &'static str {
        NAME
    }

    fn build(&self, config: &Value) -> Result<Arc<dyn Plugin>, String> {
        let c: Config = serde_json::from_value(config.clone()).map_err(|e| e.to_string())?;
        Ok(Arc::new(Pii::from_config(c)?))
    }

    fn default_fail_mode(&self) -> FailMode {
        FailMode::Closed
    }
}

#[cfg(test)]
mod tests;
