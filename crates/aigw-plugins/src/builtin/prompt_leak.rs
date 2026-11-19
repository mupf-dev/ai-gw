// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Plugin `prompt-leak` (AIGW-112): System-Prompt-Leck erkennen.
//!
//! ```yaml
//! plugin: prompt-leak
//! order: 90                      # spät in der Anfrage = innerste Schicht der Ausgabe
//! config:
//!   action: block                # block (F160) | flag (zählen, Canary entfernen)
//!   canary:
//!     enabled: true              # Standard: an
//!     rotation: request          # request (je Anfrage neu, Standard) | stable
//!   verbatim:                    # wörtliche Auszüge des System-Prompts (Standard: aus)
//!     ngram: 8                   # Wörter je N-Gramm (4–32)
//!     minNgrams: 12              # Leck ab so vielen übereinstimmenden N-Grammen
//!     ratio: 0.5                 # … oder ab diesem Anteil der N-Gramme des System-Prompts
//!     action: flag
//!   streamWindowBytes: 64        # Zurückhalten im Stream (32–4096)
//! ```
//!
//! - **Canary:** Hat die Anfrage einen System-/Developer-Prompt, hängt das Plugin an dessen
//!   letzte (führende) Nachricht `<!-- aigw-ref-<20 Hex> -->` an (80 Bit Zufall). Taucht ein solches
//!   Token in Antwort oder Stream auf (Text, Tool-Argumente, Reasoning), ist der System-Prompt
//!   (teilweise) ausgegeben worden. `block`: Guardrail-Blockade (F160, `content_filter` /
//!   `refusal`, Header `x-aigw-guardrail: prompt-leak`); `flag`: zählen und das Token
//!   entfernen. Im Reasoning (Signaturen der Anbieter, nicht veränderbar) wird immer
//!   blockiert. Erkannt wird **jedes** Token dieser Form – auch aus älteren (gecachten)
//!   Antworten.
//! - **Nie zum Client:** Das Token steht nur in der Upstream-Anfrage und in internem Meta
//!   (`aigw.prompt-leak`, verlässt das Gateway nie). Request-Payload-Logging schneidet die
//!   Client-Anfrage mit (vor den Plugins), Antwort/Stream werden nach den Plugins
//!   protokolliert – das Token ist dort schon entfernt bzw. die Ausgabe blockiert. Der
//!   Antwort-Cache rechnet den Schlüssel ohne Canary ([`cache_view`]).
//! - **`rotation: stable`:** statt Zufall je Anfrage ein HMAC-SHA256 (Prozess-Geheimnis) über
//!   Key-ID und System-Prompt – für gleiche Prompts gleich, damit Prompt-Caching der Anbieter
//!   (Präfix ab dem System-Prompt) weiter greift. Der Client sieht das Token nie, die
//!   Erkennung bleibt gleich.
//! - **Verbatim:** Wort-N-Gramme (Kleinschreibung, nur Buchstaben/Ziffern) des System-Prompts
//!   (höchstens 20 000) liegen im Meta; die Antwort zählt übereinstimmende N-Gramme. Im
//!   Live-Stream wird beim Erreichen der Schwelle abgebrochen – bis dahin ausgegebener Text
//!   ist draußen (vollständig verhindern: `outputRejectMode`/gepufferte Ausgabe eines
//!   anderen Plugins oder `action: block` mit kleiner Schwelle).

use std::borrow::Cow;
use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, LazyLock};

use aigw_core::ir::{ApiError, ChatRequest, Content, Part, PartKind, Role, StreamEvent};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Map, Value};

use super::guard::{HoldBack, StreamBlock, visit_output};
use crate::binding::FailMode;
use crate::plugin::{
    CallInfo, ChunkCtx, ChunkFlow, Flow, Hook, Meta, Phases, Plugin, PluginError, ReqCtx, RespCtx,
};
use crate::registry::PluginFactory;

const NAME: &str = "prompt-leak";
/// Schlüssel in [`Meta`] (intern, verlässt das Gateway nie).
pub const META_KEY: &str = "aigw.prompt-leak";
const PREFIX: &str = "aigw-ref-";
/// Höchstzahl gespeicherter N-Gramme des System-Prompts.
const MAX_NGRAMS: usize = 20_000;

/// Jedes Canary-Token (mit optionalem Kommentar-Rahmen), ohne Groß-/Kleinschreibung.
static CANARY_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"(?i)(?:<!--\s*)?aigw-ref-[0-9a-f]{20}(?:\s*-->)?").ok());

fn find_canary(text: &str) -> bool {
    text.len() >= PREFIX.len() + 20 && CANARY_RE.as_ref().is_some_and(|re| re.is_match(text))
}

fn redact_canary(text: &str) -> Cow<'_, str> {
    match CANARY_RE.as_ref() {
        Some(re) => re.replace_all(text, ""),
        None => Cow::Borrowed(text),
    }
}

/// Aktion bei einem Leck.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LeakAction {
    /// Ausgabe blockieren (F160).
    Block,
    /// Zählen; ein Canary wird aus der Ausgabe entfernt.
    Flag,
}

impl LeakAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Flag => "flag",
        }
    }
}

/// Erzeugung des Canary-Tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Rotation {
    /// Zufällig je Anfrage.
    #[default]
    Request,
    /// HMAC über Key-ID und System-Prompt mit Prozess-Geheimnis.
    Stable,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CanaryCfg {
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default)]
    action: Option<LeakAction>,
    #[serde(default)]
    rotation: Rotation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct VerbatimCfg {
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default)]
    action: Option<LeakAction>,
    #[serde(default)]
    ngram: Option<usize>,
    #[serde(default)]
    min_ngrams: Option<usize>,
    #[serde(default)]
    ratio: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Config {
    #[serde(default)]
    action: Option<LeakAction>,
    #[serde(default)]
    canary: Option<CanaryCfg>,
    #[serde(default)]
    verbatim: Option<VerbatimCfg>,
    #[serde(default)]
    stream_window_bytes: Option<usize>,
}

#[derive(Debug, Clone, Copy)]
struct Canary {
    action: LeakAction,
    rotation: Rotation,
}

#[derive(Debug, Clone, Copy)]
struct Verbatim {
    action: LeakAction,
    n: usize,
    min: usize,
    ratio: f64,
}

/// Plugin `prompt-leak`.
#[derive(Debug)]
pub struct PromptLeak {
    canary: Option<Canary>,
    verbatim: Option<Verbatim>,
    window: usize,
}

/// Zustand je Anfrage (aus [`Meta`]).
#[derive(Debug, Default, Clone)]
struct State {
    canary: Option<String>,
    ngrams: HashSet<u64>,
    total: usize,
}

impl State {
    fn load(meta: &Meta) -> Self {
        let mut s = Self::default();
        let Some(Value::Object(o)) = meta.get(META_KEY) else {
            return s;
        };
        s.canary = o.get("canary").and_then(Value::as_str).map(str::to_owned);
        if let Some(Value::Array(a)) = o.get("ngrams") {
            s.ngrams = a.iter().filter_map(Value::as_u64).collect();
        }
        s.total = s.ngrams.len();
        s
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Prozess-Geheimnis für `rotation: stable`.
static STABLE_KEY: LazyLock<aws_lc_rs::hmac::Key> = LazyLock::new(|| {
    let mut k = [0u8; 32];
    if aws_lc_rs::rand::fill(&mut k).is_err() {
        // Ohne Zufallsquelle: Zeit + Adresse (nur Unvorhersagbarkeit für Clients nötig).
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        k[..16].copy_from_slice(&t.to_le_bytes());
    }
    aws_lc_rs::hmac::Key::new(aws_lc_rs::hmac::HMAC_SHA256, &k)
});

fn new_canary(rotation: Rotation, key_id: Option<String>, system: &str) -> Option<String> {
    let bytes: Vec<u8> = match rotation {
        Rotation::Request => {
            let mut b = [0u8; 10];
            aws_lc_rs::rand::fill(&mut b).ok()?;
            b.to_vec()
        }
        Rotation::Stable => {
            let mut ctx = aws_lc_rs::hmac::Context::with_key(&STABLE_KEY);
            ctx.update(key_id.unwrap_or_default().as_bytes());
            ctx.update(&[0]);
            ctx.update(system.as_bytes());
            ctx.sign().as_ref()[..10].to_vec()
        }
    };
    Some(format!("{PREFIX}{}", hex(&bytes)))
}

fn marker(canary: &str) -> String {
    format!("<!-- {canary} -->")
}

fn is_system(r: Role) -> bool {
    matches!(r, Role::System | Role::Developer)
}

/// Wörter (klein, Buchstaben/Ziffern) als Hashes.
fn word_hashes(text: &str) -> Vec<u64> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut h = DefaultHasher::new();
            for c in w.chars().flat_map(char::to_lowercase) {
                c.hash(&mut h);
            }
            h.finish()
        })
        .collect()
}

fn ngram_hash(words: &[u64]) -> u64 {
    let mut h = DefaultHasher::new();
    words.hash(&mut h);
    h.finish()
}

fn ngrams(text: &str, n: usize) -> impl Iterator<Item = u64> {
    let w = word_hashes(text);
    let k = w.len().saturating_sub(n - 1);
    (0..k).map(move |i| ngram_hash(&w[i..i + n]))
}

/// Sicht der Anfrage für den Cache-Schlüssel: ohne Canary (sonst wäre jede Anfrage ein
/// anderer Schlüssel). `Cow::Borrowed`, wenn kein Canary eingefügt wurde.
pub fn cache_view<'a>(meta: &Meta, req: &'a ChatRequest) -> Cow<'a, ChatRequest> {
    let Some(canary) = State::load(meta).canary else {
        return Cow::Borrowed(req);
    };
    let mark = marker(&canary);
    let suffix = format!("\n\n{mark}");
    let mut r = req.clone();
    for m in r.messages.iter_mut().filter(|m| is_system(m.role)) {
        match &mut m.content {
            Content::Text(t) => {
                if let Some(s) = t.strip_suffix(suffix.as_str()) {
                    *t = s.to_owned();
                }
            }
            Content::Parts(parts) => {
                parts.retain(|p| !matches!(&p.kind, PartKind::Text { text } if *text == mark));
            }
        }
    }
    Cow::Owned(r)
}

impl PromptLeak {
    fn from_config(c: Config) -> Result<Self, String> {
        let action = c.action.unwrap_or(LeakAction::Block);
        let canary = match c.canary {
            None => Some(Canary {
                action,
                rotation: Rotation::Request,
            }),
            Some(k) if k.enabled => Some(Canary {
                action: k.action.unwrap_or(action),
                rotation: k.rotation,
            }),
            Some(_) => None,
        };
        let verbatim = match c.verbatim {
            Some(v) if v.enabled => {
                let n = v.ngram.unwrap_or(8);
                if !(4..=32).contains(&n) {
                    return Err("verbatim.ngram muss zwischen 4 und 32 liegen".into());
                }
                let min = v.min_ngrams.unwrap_or(12);
                if min == 0 {
                    return Err("verbatim.minNgrams muss ≥ 1 sein".into());
                }
                let ratio = v.ratio.unwrap_or(0.5);
                if !(ratio.is_finite() && ratio > 0.0 && ratio <= 1.0) {
                    return Err("verbatim.ratio muss in (0, 1] liegen".into());
                }
                Some(Verbatim {
                    action: v.action.unwrap_or(action),
                    n,
                    min,
                    ratio,
                })
            }
            _ => None,
        };
        if canary.is_none() && verbatim.is_none() {
            return Err("prompt-leak: canary oder verbatim muss aktiv sein".into());
        }
        let window = c.stream_window_bytes.unwrap_or(64);
        if !(32..=4096).contains(&window) {
            return Err("streamWindowBytes muss zwischen 32 und 4096 liegen".into());
        }
        Ok(Self {
            canary,
            verbatim,
            window,
        })
    }

    fn leaked(&self, v: &Verbatim, matched: usize, total: usize) -> bool {
        matched > 0 && (matched >= v.min || (total > 0 && matched as f64 >= v.ratio * total as f64))
    }

    fn error() -> ApiError {
        ApiError::invalid_request(
            "Die Antwort enthielt Teile des System-Prompts und wurde blockiert",
        )
        .with_code("prompt_leak")
    }

    /// Zählt einen Fund; `Err` bei `block`.
    fn hit(info: &CallInfo, category: &str, action: LeakAction) -> Result<(), ApiError> {
        info.guardrails.add(NAME, category, action.as_str(), 1);
        tracing::info!(
            request_id = %info.request_id,
            category,
            action = action.as_str(),
            "prompt-leak: System-Prompt in der Ausgabe erkannt"
        );
        match action {
            LeakAction::Block => Err(Self::error()),
            LeakAction::Flag => Ok(()),
        }
    }
}

/// Verändert alle Textstellen der Ausgabe einschließlich Reasoning (`true` = Reasoning).
fn visit_output_mut(c: &mut Content, f: &mut impl FnMut(&mut String, bool)) {
    match c {
        Content::Text(t) => f(t, false),
        Content::Parts(parts) => {
            for p in parts {
                match &mut p.kind {
                    PartKind::Text { text } | PartKind::Refusal { text } => f(text, false),
                    PartKind::ToolCall { arguments, .. } => f(arguments, false),
                    PartKind::ToolResult { content, .. } => visit_output_mut(content, f),
                    PartKind::Reasoning { text, .. } => f(text, true),
                    _ => {}
                }
            }
        }
    }
}

/// Stream-Zustand.
#[derive(Debug, Default)]
struct StreamState {
    init: bool,
    state: State,
    hold: HoldBack,
    canary_seen: bool,
    verbatim_seen: bool,
    /// Letzte `n − 1` Wort-Hashes und angefangenes Wort (über Chunk-Grenzen).
    words: Vec<u64>,
    partial: String,
    matched: HashSet<u64>,
}

impl StreamState {
    /// Füttert Text in die N-Gramm-Zählung; liefert die Zahl übereinstimmender N-Gramme.
    fn feed(&mut self, text: &str, n: usize) -> usize {
        for c in text.chars() {
            if c.is_alphanumeric() {
                self.partial.push(c);
                continue;
            }
            self.end_word(n);
        }
        self.matched.len()
    }

    fn end_word(&mut self, n: usize) {
        if self.partial.is_empty() {
            return;
        }
        let w = std::mem::take(&mut self.partial);
        if let Some(h) = word_hashes(&w).first() {
            self.words.push(*h);
        }
        if self.words.len() >= n {
            let start = self.words.len() - n;
            let g = ngram_hash(&self.words[start..]);
            if self.state.ngrams.contains(&g) {
                self.matched.insert(g);
            }
            self.words.drain(..=start);
        }
    }
}

impl Plugin for PromptLeak {
    fn name(&self) -> &'static str {
        NAME
    }

    fn phases(&self) -> Phases {
        Phases::ALL
    }

    fn on_request<'a>(&'a self, ctx: &'a mut ReqCtx<'_>) -> Hook<'a, Flow> {
        // Schon von einer anderen Bindung vorbereitet (Vererbung): nichts doppelt einfügen.
        if ctx.meta.contains_key(META_KEY) {
            return Hook::ok(Flow::Continue);
        }
        // Letzte der führenden System-/Developer-Nachrichten (bei Anthropic der `system`-Block),
        // sonst die letzte System-Nachricht überhaupt.
        let msgs = &ctx.request.messages;
        let leading = msgs.iter().take_while(|m| is_system(m.role)).count();
        let last = match leading {
            0 => msgs.iter().rposition(|m| is_system(m.role)),
            n => Some(n - 1),
        };
        let Some(last) = last else {
            return Hook::ok(Flow::Continue);
        };
        let system: String = ctx
            .request
            .messages
            .iter()
            .filter(|m| is_system(m.role))
            .map(|m| m.content.text())
            .collect::<Vec<_>>()
            .join("\n");
        let mut o = Map::new();
        if let Some(v) = &self.verbatim {
            let mut set = HashSet::new();
            let list: Vec<Value> = ngrams(&system, v.n)
                .filter(|g| set.insert(*g))
                .take(MAX_NGRAMS)
                .map(Value::from)
                .collect();
            o.insert("ngrams".into(), Value::Array(list));
        }
        if let Some(c) = &self.canary {
            let key_id = ctx.info.key.as_ref().map(|k| k.key.id.to_string());
            let Some(canary) = new_canary(c.rotation, key_id, &system) else {
                return Hook::Ready(Err(PluginError::new(
                    "prompt-leak: keine Zufallsquelle für das Canary-Token",
                )));
            };
            let mark = marker(&canary);
            match &mut ctx.request.messages[last].content {
                Content::Text(t) => {
                    t.push_str("\n\n");
                    t.push_str(&mark);
                }
                Content::Parts(parts) => parts.push(Part::text(mark)),
            }
            o.insert("canary".into(), Value::String(canary));
        }
        ctx.meta.insert(META_KEY.into(), Value::Object(o));
        Hook::ok(Flow::Continue)
    }

    fn on_response<'a>(&'a self, ctx: &'a mut RespCtx<'_>) -> Hook<'a, Flow> {
        let st = State::load(ctx.meta);
        let info = ctx.info;
        if st.canary.is_none() && st.ngrams.is_empty() {
            return Hook::ok(Flow::Continue);
        }
        // Canary: Fund in beliebiger Textstelle; Reasoning lässt sich nicht verändern.
        if let Some(c) = &self.canary {
            let mut found = false;
            let mut in_reasoning = false;
            for ch in &ctx.response.choices {
                visit_output(&ch.message.content, &mut |t: &str, reasoning| {
                    if find_canary(t) {
                        found = true;
                        in_reasoning |= reasoning;
                    }
                });
            }
            if found {
                let action = if in_reasoning {
                    LeakAction::Block
                } else {
                    c.action
                };
                if let Err(e) = Self::hit(info, "canary", action) {
                    return Hook::ok(Flow::Block(e));
                }
                for ch in &mut ctx.response.choices {
                    visit_output_mut(&mut ch.message.content, &mut |t: &mut String, _| {
                        if let Cow::Owned(s) = redact_canary(t) {
                            *t = s;
                        }
                    });
                }
            }
        }
        if let Some(v) = &self.verbatim
            && !st.ngrams.is_empty()
        {
            let mut matched = HashSet::new();
            for ch in &ctx.response.choices {
                visit_output(&ch.message.content, &mut |t: &str, _| {
                    matched.extend(ngrams(t, v.n).filter(|g| st.ngrams.contains(g)));
                });
            }
            if self.leaked(v, matched.len(), st.total)
                && let Err(e) = Self::hit(info, "verbatim", v.action)
            {
                return Hook::ok(Flow::Block(e));
            }
        }
        Hook::ok(Flow::Continue)
    }

    fn on_chunk(
        &self,
        ctx: &mut ChunkCtx<'_>,
        ev: &mut StreamEvent,
    ) -> Result<ChunkFlow, PluginError> {
        let info = ctx.info;
        if !ctx.state::<StreamState>().init {
            let s = State::load(ctx.meta);
            let st = ctx.state::<StreamState>();
            st.state = s;
            st.init = true;
        }
        let st = ctx.state::<StreamState>();
        if st.state.canary.is_none() && st.state.ngrams.is_empty() {
            return Ok(ChunkFlow::Continue);
        }
        // Verbatim auf dem eingehenden Text (vor dem Zurückhalten).
        if let Some(v) = &self.verbatim
            && !st.verbatim_seen
            && !st.state.ngrams.is_empty()
        {
            let matched = match ev {
                StreamEvent::TextDelta { text, .. }
                | StreamEvent::ReasoningDelta { text, .. }
                | StreamEvent::ToolCallDelta {
                    arguments: text, ..
                } => st.feed(text, v.n),
                StreamEvent::Stop { .. } => {
                    st.end_word(v.n);
                    st.matched.len()
                }
                _ => st.matched.len(),
            };
            if self.leaked(v, matched, st.state.total) {
                st.verbatim_seen = true;
                if let Err(e) = Self::hit(info, "verbatim", v.action) {
                    return Ok(ChunkFlow::Block(e));
                }
            }
        }
        let Some(c) = self.canary.filter(|_| st.state.canary.is_some()) else {
            return Ok(ChunkFlow::Continue);
        };
        let mut emitted = Vec::new();
        let StreamState {
            hold, canary_seen, ..
        } = st;
        let res = hold.on_event(
            ev,
            self.window,
            &mut |e| emitted.push(e),
            &mut |block: StreamBlock, buf: &mut String, last| {
                scan_canary(info, c, canary_seen, block, buf, last)
            },
        );
        for e in emitted {
            ctx.emit(e);
        }
        Ok(res.unwrap_or_else(ChunkFlow::Block))
    }

    fn on_stream_end(&self, ctx: &mut ChunkCtx<'_>) -> Result<ChunkFlow, PluginError> {
        let info = ctx.info;
        let Some(c) = self.canary else {
            return Ok(ChunkFlow::Continue);
        };
        let st = ctx.state::<StreamState>();
        let StreamState {
            hold, canary_seen, ..
        } = st;
        match hold.flush(&mut |block: StreamBlock, buf: &mut String, last| {
            scan_canary(info, c, canary_seen, block, buf, last)
        }) {
            Ok(Some(e)) => {
                ctx.emit(e);
                Ok(ChunkFlow::Continue)
            }
            Ok(None) => Ok(ChunkFlow::Continue),
            Err(e) => Ok(ChunkFlow::Block(e)),
        }
    }
}

/// Canary im zurückgehaltenen Puffer: zählen (einmal je Stream), blockieren oder entfernen.
fn scan_canary(
    info: &CallInfo,
    c: Canary,
    seen: &mut bool,
    block: StreamBlock,
    buf: &mut String,
    last: bool,
) -> Result<(), ApiError> {
    if !find_canary(buf) {
        return Ok(());
    }
    // Flag: erst entfernen, wenn ein möglicher Kommentar-Abschluss ` -->` angekommen ist.
    if !last
        && c.action == LeakAction::Flag
        && !block.is_reasoning()
        && CANARY_RE
            .as_ref()
            .and_then(|re| re.find_iter(buf).last())
            .is_some_and(|m| {
                let tail = buf[m.end()..].trim_start();
                m.as_str().starts_with("<!--")
                    && !m.as_str().ends_with("-->")
                    && "-->".starts_with(tail)
            })
    {
        return Ok(());
    }
    let action = if block.is_reasoning() {
        LeakAction::Block
    } else {
        c.action
    };
    if !*seen || action == LeakAction::Block {
        *seen = true;
        PromptLeak::hit(info, "canary", action)?;
    }
    if let Cow::Owned(s) = redact_canary(buf) {
        *buf = s;
    }
    Ok(())
}

/// Fabrik von [`PromptLeak`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Factory;

impl PluginFactory for Factory {
    fn name(&self) -> &'static str {
        NAME
    }

    fn build(&self, config: &Value) -> Result<Arc<dyn Plugin>, String> {
        let c: Config = serde_json::from_value(config.clone()).map_err(|e| e.to_string())?;
        Ok(Arc::new(PromptLeak::from_config(c)?))
    }

    fn default_fail_mode(&self) -> FailMode {
        FailMode::Closed
    }
}

#[cfg(test)]
mod tests;
