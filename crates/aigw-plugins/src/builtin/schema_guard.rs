// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Plugin `schema-guard` (AIGW-113): prüft strukturierte Ausgaben gegen das angeforderte
//! JSON-Schema.
//!
//! Aktiv wird es nur für Anfragen, die strukturierte Ausgabe verlangen (wie die IR sie
//! abbildet):
//!
//! - `response_format: {type: json_schema, json_schema: {schema, strict}}` (OpenAI) bzw.
//!   `output_config.format: {type: json_schema, schema}` (Anthropic) → Antworttext muss
//!   gültiges JSON nach dem Schema sein,
//! - `response_format: {type: json_object}` (`jsonObject: true`) → Antworttext muss ein
//!   JSON-Objekt sein,
//! - Tool-Erzwingung (`tools: forced`, Standard): `tool_choice` mit bestimmtem Tool bzw.
//!   `required`/`any` → Argumente der Tool-Aufrufe gegen das `parameters`/`input_schema` des
//!   Tools (`tools: all`: jeder Tool-Aufruf, `off`: keiner).
//!
//! ```yaml
//! plugin: schema-guard
//! config:
//!   action: retry        # flag (nur zählen, Header) | reject (Standard) | retry
//!   maxRetries: 1        # nur retry; 1–3
//!   jsonObject: true     # auch type: json_object prüfen (nur JSON-Gültigkeit)
//!   tools: forced        # off | forced | all
//!   stream: live         # live (Prüfung am Stream-Ende, nur flag/reject) | buffer
//!   invalidSchema: reject  # Schema der Anfrage nicht prüfbar: reject (400) | skip
//!   limits: {maxSchemaBytes: 65536, maxSchemaDepth: 32, maxOutputBytes: 1048576}
//! ```
//!
//! - Ablehnung als Guardrail-Ausgabe-Ablehnung ([`Flow::Block`], F160): je
//!   `outputRejectMode` der Bindung `200` mit `finish_reason: content_filter` /
//!   `stop_reason: refusal` (Standard) oder `502 schema_validation_failed`.
//! - `retry`: neue Anfrage über das Routing des Gateways ([`PluginHost::chat`]) mit der
//!   **endgültigen** Anfrage (nach allen Anfrage-Plugins, [`FINAL_REQUEST_META`]) plus der
//!   fehlerhaften Antwort und einer Korrektur-Nachricht. Jeder Versuch wird als
//!   Plugin-Modellaufruf gebucht (Kosten, Budgets, Token-Limits des Keys/Teams; AIGW-89/S-13);
//!   die `usage` der Antwort an den Client bleibt die des ersten Aufrufs (sonst doppelt
//!   gezählt). Die Ersatz-Antwort durchläuft die inneren Ausgabe-Plugins erneut
//!   ([`Plugin::rerun_inner_on_respond`]).
//! - Schema-Schutz: Größe und Tiefe begrenzt, `$ref`/`$dynamicRef`/`$recursiveRef` nur
//!   dokumentintern (`#…`), nichts wird nachgeladen (offline), Muster (`pattern`) mit der
//!   linearen `regex`-Engine (kein Backtracking, kein ReDoS).

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::io;
use std::sync::Arc;

use aigw_core::ir::{
    ApiError, ChatRequest, ChatResponse, Content, ErrorKind, Message, Part, PartKind,
    ResponseFormat, Role, StopReason, StreamEvent, ToolChoice, ToolKind,
};
use http::HeaderValue;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::binding::FailMode;
use crate::host::PluginHost;
use crate::plugin::{
    CallInfo, ChunkCtx, ChunkFlow, FINAL_REQUEST_META, Flow, Hook, Meta, Phases, Plugin,
    PluginError, ReqCtx, RespCtx,
};
use crate::registry::PluginFactory;

const NAME: &str = "schema-guard";
/// Schlüssel in [`Meta`]: zu prüfende Schemas der Anfrage (intern).
pub const META_KEY: &str = "aigw.schema-guard";
/// Antwort-Header: `invalid` (Aktion `flag`, Prüfung fehlgeschlagen) bzw. `retried=<n>`
/// (gültig nach `n` Wiederholungen).
pub const HEADER: &str = "x-aigw-schema-guard";
/// Obergrenze für `maxRetries`.
pub const MAX_RETRIES: u32 = 3;
/// Länge der Fehlerbeschreibung in der Korrektur-Nachricht an das Modell.
const MAX_HINT: usize = 600;

/// Aktion bei ungültiger Ausgabe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    /// Nur zählen (Metrik, Request-Log) und Header `x-aigw-schema-guard: invalid`.
    Flag,
    /// Ablehnen (F160).
    #[default]
    Reject,
    /// Neu anfragen (höchstens `maxRetries`), danach ablehnen.
    Retry,
}

/// Welche Tool-Aufrufe geprüft werden.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolsMode {
    /// Keine.
    Off,
    /// Nur bei erzwungenem Tool (`tool_choice` Tool bzw. `required`/`any`).
    #[default]
    Forced,
    /// Alle Funktions-Tools mit Parameterschema.
    All,
}

/// Verhalten bei gestreamten Anfragen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StreamMode {
    /// Live streamen, am Ende der Variante prüfen: `flag` zählt, `reject`/`retry` beenden die
    /// Variante mit `content_filter` (der Text ist dann schon beim Client; kein Retry möglich).
    #[default]
    Live,
    /// Ganze Antwort puffern (alle gestreamten Anfragen der Kette, `x-aigw-buffered`), dann
    /// wie nicht gestreamt prüfen – alle Aktionen inklusive `retry`.
    Buffer,
}

/// Verhalten bei einem nicht prüfbaren Schema in der Anfrage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InvalidSchema {
    /// `400 response_schema_rejected`.
    #[default]
    Reject,
    /// Anfrage ohne Prüfung dieses Schemas durchlassen (gezählt).
    Skip,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LimitsCfg {
    #[serde(default)]
    max_schema_bytes: Option<usize>,
    #[serde(default)]
    max_schema_depth: Option<usize>,
    #[serde(default)]
    max_output_bytes: Option<usize>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Config {
    #[serde(default)]
    action: Action,
    #[serde(default)]
    max_retries: Option<u32>,
    #[serde(default)]
    json_object: Option<bool>,
    #[serde(default)]
    tools: ToolsMode,
    #[serde(default)]
    stream: StreamMode,
    #[serde(default)]
    invalid_schema: InvalidSchema,
    #[serde(default)]
    limits: LimitsCfg,
}

/// Grenzen gegen teure oder bösartige Schemas und Ausgaben.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Größe eines Schemas (serialisiert, Bytes).
    pub max_schema_bytes: usize,
    /// Verschachtelungstiefe eines Schemas.
    pub max_schema_depth: usize,
    /// Größe der geprüften Ausgabe je Variante bzw. Tool-Aufruf (Bytes).
    pub max_output_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_schema_bytes: 64 * 1024,
            max_schema_depth: 32,
            max_output_bytes: 1024 * 1024,
        }
    }
}

/// Plugin `schema-guard`.
pub struct SchemaGuard {
    action: Action,
    max_retries: u32,
    json_object: bool,
    tools: ToolsMode,
    stream: StreamMode,
    invalid_schema: InvalidSchema,
    limits: Limits,
    host: Arc<dyn PluginHost>,
}

impl std::fmt::Debug for SchemaGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchemaGuard")
            .field("action", &self.action)
            .field("max_retries", &self.max_retries)
            .field("tools", &self.tools)
            .field("stream", &self.stream)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

/// Was für eine Anfrage zu prüfen ist (in [`Meta`] unter [`META_KEY`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Spec {
    /// Antworttext.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<TextSpec>,
    /// Tool-Name → Parameterschema.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    tools: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TextSpec {
    /// Beliebiges JSON-Objekt.
    Object,
    /// JSON nach Schema.
    Schema(Value),
}

impl Spec {
    fn is_empty(&self) -> bool {
        self.text.is_none() && self.tools.is_empty()
    }

    fn load(meta: &Meta) -> Option<Self> {
        meta.get(META_KEY)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .filter(|s: &Self| !s.is_empty())
    }
}

/// Übersetzte Validatoren einer Anfrage.
struct Compiled {
    text: Option<TextCheck>,
    tools: BTreeMap<String, TextCheck>,
}

enum TextCheck {
    Object,
    Schema(Box<jsonschema::Validator>),
}

/// Eine fehlgeschlagene Prüfung.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Failure {
    /// Kategorie (Metrik): `invalid-json`, `schema-mismatch`, `tool-arguments`,
    /// `unknown-tool`, `output-too-large`.
    category: &'static str,
    /// Kurzbeschreibung ohne Inhalte (Fehlermeldung an den Client).
    summary: String,
    /// Ausführliche Beschreibung für die Korrektur-Nachricht an das Modell (kann Teile der
    /// Ausgabe enthalten; nie geloggt).
    hint: String,
    /// Tool-Aufruf (ID), falls die Argumente eines Aufrufs ungültig sind.
    tool_call: Option<String>,
}

impl Failure {
    fn new(category: &'static str, summary: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            category,
            summary: summary.into(),
            hint: hint.into(),
            tool_call: None,
        }
    }
}

/// Zählt Bytes und bricht über `max` ab (Größenprüfung ohne Allokation).
struct Counter {
    n: usize,
    max: usize,
}

impl io::Write for Counter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.n = self.n.saturating_add(buf.len());
        if self.n > self.max {
            return Err(io::Error::other("zu groß"));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Prüft Tiefe und Verweise eines Schemas (rekursiv, durch `max_depth` begrenzt).
fn scan(v: &Value, depth: usize, max_depth: usize) -> Result<(), String> {
    if depth > max_depth {
        return Err(format!("Schema tiefer als {max_depth} Ebenen"));
    }
    match v {
        Value::Object(o) => {
            for (k, x) in o {
                if matches!(k.as_str(), "$ref" | "$dynamicRef" | "$recursiveRef") {
                    // Kein Text = Eigenschaftsname in `properties` o. ä. (kein Verweis).
                    match x.as_str() {
                        Some(r) if r.starts_with('#') => {}
                        None => {}
                        Some(_) => {
                            return Err(format!(
                                "{k}: nur dokumentinterne Verweise (#…) erlaubt, externe werden nie aufgelöst"
                            ));
                        }
                    }
                }
                if k == "$schema"
                    && x.as_str().is_some_and(|u| {
                        !(u.starts_with("https://json-schema.org/")
                            || u.starts_with("http://json-schema.org/"))
                    })
                {
                    return Err("$schema: nur die Meta-Schemas von json-schema.org".into());
                }
                scan(x, depth + 1, max_depth)?;
            }
            Ok(())
        }
        Value::Array(a) => a.iter().try_for_each(|x| scan(x, depth + 1, max_depth)),
        _ => Ok(()),
    }
}

/// Erste Validierungsfehler als (Kurzfassung, ausführlich).
fn describe(v: &jsonschema::Validator, instance: &Value) -> Option<(String, String)> {
    let mut summary = None;
    let mut hint = String::new();
    for e in v.iter_errors(instance).take(3) {
        let path = e.instance_path().to_string();
        let path = if path.is_empty() {
            "/".to_owned()
        } else {
            path
        };
        if summary.is_none() {
            summary = Some(format!("{} bei {path}", e.kind().keyword()));
        }
        if !hint.is_empty() {
            hint.push_str("; ");
        }
        let _ = write!(hint, "{path}: {e}");
    }
    summary.map(|s| (s, truncate(hint, MAX_HINT)))
}

fn truncate(mut s: String, max: usize) -> String {
    if s.len() > max {
        let cut = super::guard::floor_boundary(&s, max);
        s.truncate(cut);
        s.push('…');
    }
    s
}

impl SchemaGuard {
    fn from_config(c: Config, host: Arc<dyn PluginHost>) -> Result<Self, String> {
        let max_retries = c.max_retries.unwrap_or(1);
        if !(1..=MAX_RETRIES).contains(&max_retries) {
            return Err(format!(
                "maxRetries muss zwischen 1 und {MAX_RETRIES} liegen"
            ));
        }
        if c.max_retries.is_some() && c.action != Action::Retry {
            return Err("maxRetries gilt nur mit action: retry".into());
        }
        let d = Limits::default();
        let limits = Limits {
            max_schema_bytes: c.limits.max_schema_bytes.unwrap_or(d.max_schema_bytes),
            max_schema_depth: c.limits.max_schema_depth.unwrap_or(d.max_schema_depth),
            max_output_bytes: c.limits.max_output_bytes.unwrap_or(d.max_output_bytes),
        };
        if !(1024..=1024 * 1024).contains(&limits.max_schema_bytes) {
            return Err("limits.maxSchemaBytes muss zwischen 1024 und 1048576 liegen".into());
        }
        if !(2..=64).contains(&limits.max_schema_depth) {
            return Err("limits.maxSchemaDepth muss zwischen 2 und 64 liegen".into());
        }
        if !(1024..=16 * 1024 * 1024).contains(&limits.max_output_bytes) {
            return Err("limits.maxOutputBytes muss zwischen 1024 und 16777216 liegen".into());
        }
        Ok(Self {
            action: c.action,
            max_retries,
            json_object: c.json_object.unwrap_or(true),
            tools: c.tools,
            stream: c.stream,
            invalid_schema: c.invalid_schema,
            limits,
            host,
        })
    }

    /// Prüft ein Schema der Anfrage (Größe, Tiefe, Verweise) und übersetzt es.
    pub fn compile(&self, schema: &Value) -> Result<jsonschema::Validator, String> {
        compile(schema, &self.limits)
    }

    /// Zu prüfende Schemas der Anfrage.
    fn spec_of(&self, req: &ChatRequest) -> Spec {
        let text = match &req.response_format {
            Some(ResponseFormat::JsonSchema { schema, .. }) => {
                Some(TextSpec::Schema(schema.clone()))
            }
            Some(ResponseFormat::JsonObject) if self.json_object => Some(TextSpec::Object),
            _ => None,
        };
        let mut tools = BTreeMap::new();
        let forced: Option<Option<&str>> = match (&self.tools, &req.tool_choice) {
            (ToolsMode::Off, _) => None,
            (ToolsMode::All, _) | (ToolsMode::Forced, Some(ToolChoice::Required)) => Some(None),
            (ToolsMode::Forced, Some(ToolChoice::Tool { name })) => Some(Some(name.as_str())),
            (ToolsMode::Forced, _) => None,
        };
        if let Some(only) = forced {
            for t in &req.tools {
                if let ToolKind::Function(f) = &t.kind
                    && only.is_none_or(|n| n == f.name)
                    && let Some(p) = &f.parameters
                {
                    tools.insert(f.name.clone(), p.clone());
                }
            }
        }
        Spec { text, tools }
    }

    fn compiled(&self, spec: &Spec) -> Result<Compiled, String> {
        let text = match &spec.text {
            None => None,
            Some(TextSpec::Object) => Some(TextCheck::Object),
            Some(TextSpec::Schema(s)) => Some(TextCheck::Schema(Box::new(self.compile(s)?))),
        };
        let mut tools = BTreeMap::new();
        for (name, s) in &spec.tools {
            tools.insert(name.clone(), TextCheck::Schema(Box::new(self.compile(s)?)));
        }
        Ok(Compiled { text, tools })
    }

    /// Prüft einen JSON-Text.
    fn check_json(&self, check: &TextCheck, text: &str) -> Option<Failure> {
        if text.len() > self.limits.max_output_bytes {
            return Some(Failure::new(
                "output-too-large",
                format!(
                    "Ausgabe größer als {} Bytes, nicht prüfbar",
                    self.limits.max_output_bytes
                ),
                "The output was too large.",
            ));
        }
        let value: Value = match serde_json::from_str(text.trim()) {
            Ok(v) => v,
            Err(e) => {
                return Some(Failure::new(
                    "invalid-json",
                    "kein gültiges JSON",
                    format!("The output is not valid JSON ({e})."),
                ));
            }
        };
        match check {
            TextCheck::Object if !value.is_object() => Some(Failure::new(
                "invalid-json",
                "kein JSON-Objekt",
                "The output must be a single JSON object.",
            )),
            TextCheck::Object => None,
            TextCheck::Schema(v) => describe(v, &value).map(|(summary, hint)| {
                Failure::new(
                    "schema-mismatch",
                    format!("Schema verletzt: {summary}"),
                    hint,
                )
            }),
        }
    }

    /// Prüft eine Antwortnachricht. `known_tools`: nur diese Tools sind erlaubt (Retry).
    fn check_message(
        &self,
        c: &Compiled,
        msg: &Message,
        stop: Option<&StopReason>,
        known_tools: Option<&HashSet<String>>,
    ) -> Option<Failure> {
        if matches!(stop, Some(StopReason::ContentFilter | StopReason::Refusal)) {
            return None;
        }
        let mut calls = Vec::new();
        let mut refusal = false;
        if let Content::Parts(parts) = &msg.content {
            for p in parts {
                match &p.kind {
                    PartKind::ToolCall {
                        id,
                        name,
                        arguments,
                    } => calls.push((id.as_str(), name.as_str(), arguments.as_str())),
                    PartKind::Refusal { .. } => refusal = true,
                    _ => {}
                }
            }
        }
        for (id, name, args) in &calls {
            if let Some(known) = known_tools
                && !known.contains(*name)
            {
                let mut f = Failure::new(
                    "unknown-tool",
                    "Aufruf eines nicht angebotenen Tools",
                    format!("The tool {name:?} does not exist."),
                );
                f.tool_call = Some((*id).to_owned());
                return Some(f);
            }
            if let Some(check) = c.tools.get(*name) {
                let args: &str = if args.trim().is_empty() { "{}" } else { args };
                if let Some(mut f) = self.check_json(check, args) {
                    if f.category != "output-too-large" {
                        f.category = "tool-arguments";
                    }
                    f.summary = format!("Tool {name:?}: {}", f.summary);
                    f.tool_call = Some((*id).to_owned());
                    return Some(f);
                }
            }
        }
        match &c.text {
            Some(check) if calls.is_empty() && !refusal => {
                self.check_json(check, &msg.content.text())
            }
            _ => None,
        }
    }

    fn check_response(
        &self,
        c: &Compiled,
        resp: &ChatResponse,
        known_tools: Option<&HashSet<String>>,
    ) -> Option<Failure> {
        resp.choices
            .iter()
            .find_map(|ch| self.check_message(c, &ch.message, ch.stop_reason.as_ref(), known_tools))
    }

    fn block(f: &Failure) -> Flow {
        Flow::Block(error(f))
    }

    /// Neue Versuche über das Gateway (Aktion `retry`).
    async fn retry(
        &self,
        ctx: &mut RespCtx<'_>,
        mut req: ChatRequest,
        compiled: Compiled,
        mut failure: Failure,
    ) -> Result<Flow, PluginError> {
        let info: &CallInfo = ctx.info;
        let Some(group) = info.model_group.clone() else {
            info.guardrails.add(NAME, failure.category, "reject", 1);
            return Ok(Self::block(&failure));
        };
        req.stream = Some(false);
        req.stream_options = None;
        req.n = None;
        let known: HashSet<String> = req
            .tools
            .iter()
            .filter_map(|t| match &t.kind {
                ToolKind::Function(f) => Some(f.name.clone()),
                ToolKind::Opaque { .. } => None,
            })
            .collect();
        let owner = std::ptr::from_ref(self) as usize;
        let mut last = ctx.response.choices[0].message.clone();
        for attempt in 1..=self.max_retries {
            if !info.plugin_calls.try_reserve(owner, 1, self.max_retries) {
                break;
            }
            info.guardrails.add(NAME, failure.category, "retry", 1);
            append_correction(&mut req, last, &failure);
            let resp = match self
                .host
                .chat(&group, req.clone(), &info.plugin_calls)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        request_id = %info.request_id,
                        error = %e,
                        "schema-guard: Wiederholung fehlgeschlagen"
                    );
                    break;
                }
            };
            match self.check_response(&compiled, &resp, Some(&known)) {
                None if resp.choices.len() == 1 => {
                    let mut resp = resp;
                    // Abgerechnet wird der Wiederholungsaufruf als Plugin-Modellaufruf; die
                    // Usage der Antwort bleibt die des ersten Aufrufs (sonst doppelt).
                    resp.usage = ctx.response.usage.clone();
                    if let Ok(v) = HeaderValue::from_str(&format!("retried={attempt}")) {
                        ctx.response_headers
                            .insert(http::HeaderName::from_static(HEADER), v);
                    }
                    return Ok(Flow::Respond(Box::new(resp)));
                }
                None => {
                    failure = Failure::new(
                        "schema-mismatch",
                        "Wiederholung ohne genau eine Variante",
                        "Return exactly one answer.",
                    );
                    let Some(ch) = resp.choices.into_iter().next() else {
                        break;
                    };
                    last = ch.message;
                }
                Some(f) => {
                    failure = f;
                    let Some(ch) = resp.choices.into_iter().next() else {
                        break;
                    };
                    last = ch.message;
                }
            }
        }
        info.guardrails.add(NAME, failure.category, "reject", 1);
        Ok(Self::block(&failure))
    }
}

/// Prüft ein Schema (Größe, Tiefe, Verweise) und übersetzt es (offline, lineare Regex).
pub fn compile(schema: &Value, limits: &Limits) -> Result<jsonschema::Validator, String> {
    if !matches!(schema, Value::Object(_) | Value::Bool(_)) {
        return Err("Schema muss ein Objekt sein".into());
    }
    let mut counter = Counter {
        n: 0,
        max: limits.max_schema_bytes,
    };
    if serde_json::to_writer(&mut counter, schema).is_err() {
        return Err(format!(
            "Schema größer als {} Bytes",
            limits.max_schema_bytes
        ));
    }
    scan(schema, 0, limits.max_schema_depth)?;
    jsonschema::options()
        .offline()
        .with_pattern_options(
            jsonschema::PatternOptions::regex()
                .size_limit(1 << 20)
                .dfa_size_limit(1 << 20),
        )
        .build(schema)
        .map_err(|e| truncate(format!("Schema ungültig: {e}"), 300))
}

/// Fehler der Ablehnung (Ausgabe): `502 schema_validation_failed` (bei
/// `outputRejectMode: error`; sonst `200` mit `content_filter`).
fn error(f: &Failure) -> ApiError {
    ApiError::new(
        ErrorKind::BadGateway,
        format!(
            "Die Modellantwort entspricht nicht dem angeforderten Format ({})",
            f.summary
        ),
    )
    .with_code("schema_validation_failed")
}

/// Hängt die fehlerhafte Antwort und die Korrektur an die Anfrage an.
fn append_correction(req: &mut ChatRequest, last: Message, f: &Failure) {
    let calls: Vec<String> = match &last.content {
        Content::Parts(parts) => parts
            .iter()
            .filter_map(|p| match &p.kind {
                PartKind::ToolCall { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect(),
        Content::Text(_) => Vec::new(),
    };
    req.messages.push(Message {
        role: Role::Assistant,
        content: last.content,
        name: None,
        extra: Default::default(),
    });
    if calls.is_empty() {
        req.messages.push(Message::text(
            Role::User,
            format!(
                "Your previous response did not match the required JSON format: {} \
                 Respond again with only the JSON value that conforms to the requested schema, \
                 without any other text or code fences.",
                f.hint
            ),
        ));
        return;
    }
    // Jeder Tool-Aufruf braucht ein Ergebnis (OpenAI/Anthropic).
    for id in calls {
        let text = if f.tool_call.as_deref() == Some(id.as_str()) {
            format!(
                "Error: the arguments do not match the tool's JSON schema: {} \
                 Call the tool again with valid arguments.",
                f.hint
            )
        } else {
            "Not executed: another tool call in this turn had invalid arguments. \
             Repeat all tool calls with valid arguments."
                .to_owned()
        };
        req.messages.push(Message {
            role: Role::Tool,
            content: Content::Parts(vec![Part::new(PartKind::ToolResult {
                tool_call_id: id,
                content: Content::Text(text),
                is_error: Some(true),
            })]),
            name: None,
            extra: Default::default(),
        });
    }
}

/// Zustand je Stream: Text und Tool-Argumente der aktuellen Variante.
#[derive(Default)]
struct StreamState {
    spec: Option<Option<Arc<Compiled>>>,
    text: String,
    /// Stream-Index → (ID, Name, Argumente).
    tools: BTreeMap<u32, (String, String, String)>,
    too_large: bool,
}

impl StreamState {
    fn reset(&mut self) {
        self.text.clear();
        self.tools.clear();
        self.too_large = false;
    }
}

impl Plugin for SchemaGuard {
    fn name(&self) -> &'static str {
        NAME
    }

    fn phases(&self) -> Phases {
        Phases::ALL
    }

    fn needs_buffered_output(&self) -> bool {
        self.stream == StreamMode::Buffer
    }

    fn rerun_inner_on_respond(&self) -> bool {
        true
    }

    fn on_request<'a>(&'a self, ctx: &'a mut ReqCtx<'_>) -> Hook<'a, Flow> {
        let mut spec = self.spec_of(&ctx.request);
        if spec.is_empty() {
            return Hook::ok(Flow::Continue);
        }
        // Schemas jetzt prüfen: ungeeignete Schemas kosten keinen Upstream-Aufruf.
        let mut bad = None;
        if let Some(TextSpec::Schema(s)) = &spec.text
            && let Err(e) = self.compile(s)
        {
            bad = Some(e);
            if self.invalid_schema == InvalidSchema::Skip {
                spec.text = None;
            }
        }
        let mut dropped = Vec::new();
        for (name, s) in &spec.tools {
            if let Err(e) = self.compile(s) {
                if bad.is_none() {
                    bad = Some(format!("Tool {name:?}: {e}"));
                }
                dropped.push(name.clone());
            }
        }
        if let Some(e) = bad {
            match self.invalid_schema {
                InvalidSchema::Reject => {
                    ctx.info
                        .guardrails
                        .add(NAME, "schema-rejected", "reject", 1);
                    return Hook::ok(Flow::Block(
                        ApiError::invalid_request(format!(
                            "Das JSON-Schema der Anfrage ist nicht prüfbar: {e}"
                        ))
                        .with_code("response_schema_rejected"),
                    ));
                }
                InvalidSchema::Skip => {
                    ctx.info.guardrails.add(NAME, "schema-rejected", "skip", 1);
                    for n in dropped {
                        spec.tools.remove(&n);
                    }
                    if spec.is_empty() {
                        return Hook::ok(Flow::Continue);
                    }
                }
            }
        }
        if let Ok(v) = serde_json::to_value(&spec) {
            ctx.meta.insert(META_KEY.into(), v);
        }
        let live = ctx.info.stream && self.stream == StreamMode::Live;
        if self.action == Action::Retry && !live {
            ctx.meta.insert(FINAL_REQUEST_META.into(), Value::Null);
        }
        Hook::ok(Flow::Continue)
    }

    fn on_response<'a>(&'a self, ctx: &'a mut RespCtx<'_>) -> Hook<'a, Flow> {
        let Some(spec) = Spec::load(ctx.meta) else {
            return Hook::ok(Flow::Continue);
        };
        let compiled = match self.compiled(&spec) {
            Ok(c) => c,
            // In der Anfrage-Phase schon geprüft – hier nur bei geänderter Konfiguration.
            Err(e) => return Hook::Ready(Err(PluginError::new(e))),
        };
        let Some(failure) = self.check_response(&compiled, &ctx.response, None) else {
            return Hook::ok(Flow::Continue);
        };
        let info = ctx.info;
        match self.action {
            Action::Flag => {
                info.guardrails.add(NAME, failure.category, "flag", 1);
                ctx.response_headers.insert(
                    http::HeaderName::from_static(HEADER),
                    HeaderValue::from_static("invalid"),
                );
                Hook::ok(Flow::Continue)
            }
            Action::Reject => {
                info.guardrails.add(NAME, failure.category, "reject", 1);
                Hook::ok(Self::block(&failure))
            }
            Action::Retry => {
                let req = ctx
                    .meta
                    .get(FINAL_REQUEST_META)
                    .filter(|v| !v.is_null())
                    .and_then(|v| serde_json::from_value::<ChatRequest>(v.clone()).ok())
                    .filter(|r| r.choices() <= 1 && ctx.response.choices.len() == 1);
                match req {
                    Some(req) => Hook::future(self.retry(ctx, req, compiled, failure)),
                    // Mehrere Varianten (`n` > 1) oder keine Anfrage: kein Retry möglich.
                    None => {
                        info.guardrails.add(NAME, failure.category, "reject", 1);
                        Hook::ok(Self::block(&failure))
                    }
                }
            }
        }
    }

    fn on_chunk(
        &self,
        ctx: &mut ChunkCtx<'_>,
        ev: &mut StreamEvent,
    ) -> Result<ChunkFlow, PluginError> {
        if ctx.state::<StreamState>().spec.is_none() {
            let compiled = Spec::load(ctx.meta)
                .map(|s| self.compiled(&s).map(Arc::new))
                .transpose()
                .map_err(PluginError::new)?;
            ctx.state::<StreamState>().spec = Some(compiled);
        }
        let max = self.limits.max_output_bytes;
        let info = ctx.info;
        let st = ctx.state::<StreamState>();
        let Some(Some(compiled)) = st.spec.clone() else {
            return Ok(ChunkFlow::Continue);
        };
        let push = |buf: &mut String, s: &str, too_large: &mut bool| {
            if buf.len() + s.len() > max {
                *too_large = true;
            } else {
                buf.push_str(s);
            }
        };
        match ev {
            StreamEvent::Choice { .. } => st.reset(),
            StreamEvent::TextDelta { text, .. } => {
                push(&mut st.text, text.as_str(), &mut st.too_large);
            }
            StreamEvent::ToolCallStart(s) => {
                st.tools
                    .insert(s.index, (s.id.clone(), s.name.clone(), String::new()));
            }
            StreamEvent::ToolCallDelta { index, arguments } => {
                if let Some(t) = st.tools.get_mut(&*index) {
                    push(&mut t.2, arguments.as_str(), &mut st.too_large);
                }
            }
            StreamEvent::Stop { reason, .. } => {
                let failure = if st.too_large {
                    Some(Failure::new(
                        "output-too-large",
                        format!("Ausgabe größer als {max} Bytes, nicht prüfbar"),
                        "",
                    ))
                } else {
                    let parts: Vec<Part> =
                        std::iter::once(Part::text(std::mem::take(&mut st.text)))
                            .chain(st.tools.values().map(|(id, name, args)| {
                                Part::new(PartKind::ToolCall {
                                    id: id.clone(),
                                    name: name.clone(),
                                    arguments: args.clone(),
                                })
                            }))
                            .collect();
                    let msg = Message {
                        role: Role::Assistant,
                        content: Content::Parts(parts),
                        name: None,
                        extra: Default::default(),
                    };
                    self.check_message(&compiled, &msg, Some(&*reason), None)
                };
                st.reset();
                if let Some(f) = failure {
                    // Live: der Text ist schon beim Client – `flag` zählt nur, `reject`/`retry`
                    // beenden die Variante mit `content_filter` (bzw. Fehler).
                    return Ok(match self.action {
                        Action::Flag => {
                            info.guardrails.add(NAME, f.category, "flag", 1);
                            ChunkFlow::Continue
                        }
                        Action::Reject | Action::Retry => {
                            info.guardrails.add(NAME, f.category, "reject", 1);
                            ChunkFlow::Block(error(&f))
                        }
                    });
                }
            }
            _ => {}
        }
        Ok(ChunkFlow::Continue)
    }
}

/// Fabrik von [`SchemaGuard`]; `retry` ruft Modellgruppen über den Gateway-Host auf.
#[derive(Debug, Clone)]
pub struct Factory {
    host: Arc<dyn PluginHost>,
}

impl Factory {
    /// Fabrik mit den Diensten des Gateways.
    pub fn new(host: Arc<dyn PluginHost>) -> Self {
        Self { host }
    }
}

impl PluginFactory for Factory {
    fn name(&self) -> &'static str {
        NAME
    }

    fn build(&self, config: &Value) -> Result<Arc<dyn Plugin>, String> {
        let c: Config = serde_json::from_value(config.clone()).map_err(|e| e.to_string())?;
        Ok(Arc::new(SchemaGuard::from_config(
            c,
            Arc::clone(&self.host),
        )?))
    }

    fn default_fail_mode(&self) -> FailMode {
        FailMode::Closed
    }

    /// Wiederholungen sind ganze Modellaufrufe: Zeitlimit für alle Versuche zusammen.
    fn default_timeout(&self) -> std::time::Duration {
        crate::binding::MAX_TIMEOUT
    }
}

#[cfg(test)]
mod tests;
