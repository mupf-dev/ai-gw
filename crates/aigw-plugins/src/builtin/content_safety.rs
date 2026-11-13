// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Plugin `content-safety`: externe Inhaltsprüfung (AIGW-38).
//!
//! ```yaml
//! plugin: content-safety
//! config:
//!   provider: azure                 # azure | openai | model
//!   endpoint: https://acme.cognitiveservices.azure.com
//!   credential: azure-content-safety  # ProviderCredential (Feld apiKey/api_key)
//!   thresholds: {hate: 2, violence: 4} # azure: Schweregrad 0–7 (Standard 4);
//!                                      # openai: Score 0–1 (ohne Angabe: flagged des Dienstes)
//!   categories: [hate, violence]    # nur diese blockieren (Standard: alle)
//!   direction: input                # input | output | both
//!   outputMode: buffered            # buffered | sentence (Stream satzweise)
//!   action: reject                  # reject (400 content_filtered) | flag
//!   roles: [user]                   # geprüfte Eingabe-Nachrichten
//!   maxChars: 10000                 # geprüft wird das Ende der Eingabe/Ausgabe
//!   sentenceTimeoutMs: 2000         # Satzmodus: Zeitlimit je Prüfung
//!   sentenceOnError: block          # Satzmodus: block | allow bei Dienstfehlern
//!   timeoutMs: 1500                 # Zeitlimit (Kettensteuerung), failMode: open|closed
//! ```
//!
//! - **Azure AI Content Safety:** `POST {endpoint}/contentsafety/text:analyze?api-version=…`,
//!   Header `Ocp-Apim-Subscription-Key`.
//! - **OpenAI Moderation:** `POST {endpoint|https://api.openai.com}/v1/moderations`,
//!   Modell `omni-moderation-latest` (änderbar über `model`).
//! - **Modell über das Gateway** (Llama Guard o. ä.): `modelGroup` + `promptTemplate`
//!   (Platzhalter `{{role}}`, `{{conversation}}`), Antwort `safe` bzw. `unsafe` + Kategorien
//!   (`S1,S10`) in der nächsten Zeile.
//!
//! Zugangsdaten nie in der Plugin-Konfiguration: nur der Name einer `ProviderCredential`, zur
//! Laufzeit aus dem aktuellen Snapshot aufgelöst. Endpunkte unterliegen den SSRF-Regeln
//! (`admin.allow_*_upstreams`).

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use aigw_core::ir::{
    ApiError, ChatRequest, ChatResponse, Content, ErrorKind, Message, Role, StreamEvent,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use super::guard::{Direction, RoleSel, floor_boundary, messages, visit};
use crate::binding::FailMode;
use crate::host::{HttpCall, NoHost, PluginHost};
use crate::plugin::{
    CallInfo, ChunkCtx, ChunkFlow, Flow, Hook, Phases, Plugin, PluginError, ReqCtx, RespCtx,
};
use crate::registry::PluginFactory;

const NAME: &str = "content-safety";
/// Mindestlänge eines Satzes, bevor er einzeln geprüft wird (kurze werden zusammengefasst).
const MIN_SENTENCE: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Provider {
    Azure,
    Openai,
    Model,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
enum OutputMode {
    #[default]
    Buffered,
    Sentence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
enum OnError {
    #[default]
    Block,
    Allow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
enum Action {
    #[default]
    Reject,
    Flag,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Config {
    provider: Provider,
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    credential: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    api_version: Option<String>,
    #[serde(default)]
    model_group: Option<String>,
    #[serde(default)]
    prompt_template: Option<String>,
    #[serde(default)]
    thresholds: BTreeMap<String, f64>,
    #[serde(default)]
    categories: Option<Vec<String>>,
    #[serde(default)]
    direction: Option<Direction>,
    #[serde(default)]
    output_mode: OutputMode,
    #[serde(default)]
    action: Action,
    #[serde(default)]
    roles: Option<Vec<RoleSel>>,
    #[serde(default)]
    max_chars: Option<usize>,
    #[serde(default)]
    sentence_timeout_ms: Option<u64>,
    #[serde(default)]
    sentence_on_error: OnError,
}

/// Standard-Prompt im Stil von Llama Guard 3 (Kategorien S1–S14).
pub const DEFAULT_TEMPLATE: &str = "Task: Check if there is unsafe content in '{{role}}' messages in conversations according to our safety policy with the below categories.

<BEGIN UNSAFE CONTENT CATEGORIES>
S1: Violent Crimes.
S2: Non-Violent Crimes.
S3: Sex Crimes.
S4: Child Exploitation.
S5: Defamation.
S6: Specialized Advice.
S7: Privacy.
S8: Intellectual Property.
S9: Indiscriminate Weapons.
S10: Hate.
S11: Self-Harm.
S12: Sexual Content.
S13: Elections.
S14: Code Interpreter Abuse.
<END UNSAFE CONTENT CATEGORIES>

<BEGIN CONVERSATION>

{{conversation}}

<END CONVERSATION>

Provide your safety assessment for ONLY THE LAST {{role}} message in the above conversation:
 - First line must read 'safe' or 'unsafe'.
 - If unsafe, a second line must include a comma-separated list of violated categories.";

/// Geprüfte Konfiguration (geteilt mit Hintergrundprüfungen im Satzmodus).
#[derive(Debug)]
struct Inner {
    host: Arc<dyn PluginHost>,
    provider: Provider,
    url: String,
    credential: Option<String>,
    model: String,
    model_group: String,
    template: String,
    thresholds: BTreeMap<String, f64>,
    categories: Option<Vec<String>>,
    max_chars: usize,
    call_timeout: Duration,
}

/// Ergebnis einer Prüfung: verletzte Kategorien (normalisiert, z. B. `self_harm`, `s10`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Verdict {
    /// Verletzte Kategorien nach Schwellen und Kategorie-Filter.
    pub flagged: Vec<String>,
}

/// Kategorie-Name normalisieren: `SelfHarm`/`self-harm/intent` → `self_harm`/`self_harm_intent`.
fn norm(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    let mut prev_lower = false;
    for c in name.trim().chars() {
        if c.is_ascii_uppercase() {
            if prev_lower {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
            prev_lower = false;
        } else if matches!(c, '-' | '/' | ' ' | '_') {
            if !out.ends_with('_') {
                out.push('_');
            }
            prev_lower = false;
        } else {
            out.push(c);
            prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        }
    }
    out
}

/// Liest die Antwort eines Prüfmodells (`safe` / `unsafe` + Kategorien).
pub fn parse_guard_reply(text: &str) -> Result<Vec<String>, PluginError> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines
        .next()
        .ok_or_else(|| PluginError::new("Prüfmodell: leere Antwort"))?
        .to_ascii_lowercase();
    if first == "safe" || first.starts_with("safe ") || first == "safe." {
        return Ok(Vec::new());
    }
    let Some(rest) = first.strip_prefix("unsafe") else {
        return Err(PluginError::new("Prüfmodell: Antwort nicht auswertbar"));
    };
    let list = match rest.trim_start_matches([':', ' ', '.']) {
        "" => lines.next().unwrap_or("").to_owned(),
        r => r.to_owned(),
    };
    let mut cats: Vec<String> = list
        .split([',', ' ', ';'])
        .map(|c| c.trim().trim_end_matches('.'))
        .filter(|c| !c.is_empty())
        .map(norm)
        .collect();
    if cats.is_empty() {
        cats.push("unsafe".into());
    }
    Ok(cats)
}

impl Inner {
    fn keep(&self, cat: &str) -> bool {
        self.categories
            .as_ref()
            .is_none_or(|list| list.iter().any(|c| c == cat))
    }

    fn api_key(&self) -> Result<String, PluginError> {
        let Some(name) = &self.credential else {
            return Err(PluginError::new("keine Zugangsdaten konfiguriert"));
        };
        let data = self
            .host
            .credential(name)
            .ok_or_else(|| PluginError::new(format!("Zugangsdaten {name:?} nicht gefunden")))?;
        data.get("apiKey")
            .or_else(|| data.get("api_key"))
            .map(|s| s.expose().to_owned())
            .ok_or_else(|| PluginError::new(format!("Zugangsdaten {name:?} ohne apiKey")))
    }

    async fn post(
        &self,
        header: &'static str,
        value: String,
        body: Value,
    ) -> Result<Value, PluginError> {
        let reply = self
            .host
            .post_json(HttpCall {
                url: self.url.clone(),
                headers: vec![(header, value)],
                body: serde_json::to_vec(&body).map_err(|e| PluginError::new(e.to_string()))?,
                timeout: self.call_timeout,
            })
            .await?;
        if !(200..300).contains(&reply.status) {
            return Err(PluginError::new(format!(
                "Inhaltsprüfung: HTTP {}",
                reply.status
            )));
        }
        serde_json::from_slice(&reply.body)
            .map_err(|_| PluginError::new("Inhaltsprüfung: Antwort ist kein JSON"))
    }

    /// Prüft `text` (Rolle: Eingabe = `User`, Ausgabe = `Agent`).
    async fn analyze(&self, text: String, output: bool) -> Result<Verdict, PluginError> {
        let mut flagged = Vec::new();
        match self.provider {
            Provider::Azure => {
                let key = self.api_key()?;
                let v = self
                    .post(
                        "ocp-apim-subscription-key",
                        key,
                        json!({"text": text, "outputType": "FourSeverityLevels"}),
                    )
                    .await?;
                let list = v["categoriesAnalysis"].as_array().ok_or_else(|| {
                    PluginError::new("Azure Content Safety: categoriesAnalysis fehlt")
                })?;
                for c in list {
                    let (Some(name), Some(sev)) = (c["category"].as_str(), c["severity"].as_f64())
                    else {
                        continue;
                    };
                    let name = norm(name);
                    let limit = self.thresholds.get(&name).copied().unwrap_or(4.0);
                    if sev >= limit && self.keep(&name) {
                        flagged.push(name);
                    }
                }
            }
            Provider::Openai => {
                let key = self.api_key()?;
                let v = self
                    .post(
                        "authorization",
                        format!("Bearer {key}"),
                        json!({"model": self.model, "input": text}),
                    )
                    .await?;
                let r = &v["results"][0];
                let cats = r["categories"]
                    .as_object()
                    .ok_or_else(|| PluginError::new("OpenAI Moderation: categories fehlt"))?;
                for (name, hit) in cats {
                    let n = norm(name);
                    let over = match self.thresholds.get(&n) {
                        Some(t) => r["category_scores"][name].as_f64().is_some_and(|s| s >= *t),
                        None => hit.as_bool() == Some(true),
                    };
                    if over && self.keep(&n) {
                        flagged.push(n);
                    }
                }
            }
            Provider::Model => {
                let role = if output { "Agent" } else { "User" };
                let prompt = self
                    .template
                    .replace("{{role}}", role)
                    .replace("{{conversation}}", &format!("{role}: {text}"));
                let req = ChatRequest {
                    model: self.model_group.clone(),
                    messages: vec![Message {
                        role: Role::User,
                        content: Content::Text(prompt),
                        name: None,
                        extra: Default::default(),
                    }],
                    max_tokens: Some(64),
                    stream: Some(false),
                    ..ChatRequest::default()
                };
                let mut req = req;
                req.sampling.temperature = Some(0.0);
                let resp = self.host.chat(&self.model_group, req).await?;
                let reply = resp
                    .choices
                    .first()
                    .map(|c| c.message.content.text())
                    .unwrap_or_default();
                for c in parse_guard_reply(&reply)? {
                    if self.keep(&c) {
                        flagged.push(c);
                    }
                }
            }
        }
        flagged.sort();
        flagged.dedup();
        Ok(Verdict { flagged })
    }

    /// Ende von `text` (höchstens `max_chars` Bytes, auf Zeichengrenze).
    fn tail(&self, text: &str) -> String {
        let start = text.len().saturating_sub(self.max_chars);
        let mut s = start;
        while !text.is_char_boundary(s) {
            s += 1;
        }
        text[s..].to_owned()
    }
}

/// Plugin `content-safety`.
#[derive(Debug)]
pub struct ContentSafety {
    inner: Arc<Inner>,
    direction: Direction,
    output_mode: OutputMode,
    action: Action,
    roles: Vec<RoleSel>,
    sentence_timeout: Duration,
    sentence_on_error: OnError,
}

impl ContentSafety {
    fn from_config(c: Config, host: Arc<dyn PluginHost>) -> Result<Self, String> {
        let endpoint = |default: Option<&str>| -> Result<String, String> {
            let e = c
                .endpoint
                .as_deref()
                .or(default)
                .ok_or("endpoint fehlt")?
                .trim_end_matches('/')
                .to_owned();
            host.check_endpoint(&e)
                .map_err(|m| format!("endpoint: {m}"))?;
            Ok(e)
        };
        let needs_credential = || -> Result<(), String> {
            match &c.credential {
                Some(n) if !n.trim().is_empty() => Ok(()),
                _ => Err("credential: Name einer ProviderCredential angeben".into()),
            }
        };
        let url = match c.provider {
            Provider::Azure => {
                needs_credential()?;
                let v = c.api_version.as_deref().unwrap_or("2024-09-01");
                if !v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                    return Err("apiVersion ungültig".into());
                }
                format!(
                    "{}/contentsafety/text:analyze?api-version={v}",
                    endpoint(None)?
                )
            }
            Provider::Openai => {
                needs_credential()?;
                format!(
                    "{}/v1/moderations",
                    endpoint(Some("https://api.openai.com"))?
                )
            }
            Provider::Model => {
                if c.model_group.as_deref().is_none_or(str::is_empty) {
                    return Err("modelGroup: Modellgruppe des Prüfmodells angeben".into());
                }
                if c.endpoint.is_some() || c.credential.is_some() {
                    return Err(
                        "provider model: endpoint/credential kommen aus der Modellgruppe".into(),
                    );
                }
                String::new()
            }
        };
        let template = c
            .prompt_template
            .clone()
            .unwrap_or_else(|| DEFAULT_TEMPLATE.to_owned());
        if !template.contains("{{conversation}}") {
            return Err("promptTemplate braucht den Platzhalter {{conversation}}".into());
        }
        for (k, v) in &c.thresholds {
            if !v.is_finite() || *v < 0.0 {
                return Err(format!("thresholds.{k}: Wert ≥ 0 erwartet"));
            }
        }
        let max_chars = c.max_chars.unwrap_or(10_000);
        if !(100..=100_000).contains(&max_chars) {
            return Err("maxChars muss zwischen 100 und 100000 liegen".into());
        }
        let sentence_timeout =
            Duration::from_millis(c.sentence_timeout_ms.unwrap_or(2000).clamp(10, 60_000));
        Ok(Self {
            inner: Arc::new(Inner {
                host,
                provider: c.provider,
                url,
                credential: c.credential,
                model: c.model.unwrap_or_else(|| "omni-moderation-latest".into()),
                model_group: c.model_group.unwrap_or_default(),
                template,
                thresholds: c
                    .thresholds
                    .into_iter()
                    .map(|(k, v)| (norm(&k), v))
                    .collect(),
                categories: c.categories.map(|l| l.iter().map(|s| norm(s)).collect()),
                max_chars,
                call_timeout: Duration::from_secs(30),
            }),
            direction: c.direction.unwrap_or(Direction::Input),
            output_mode: c.output_mode,
            action: c.action,
            roles: c.roles.unwrap_or_else(|| vec![RoleSel::User]),
            sentence_timeout,
            sentence_on_error: c.sentence_on_error,
        })
    }

    /// Ergebnis → Flow (zählt Treffer).
    fn judge(&self, info: &CallInfo, v: &Verdict, output: bool) -> Option<ApiError> {
        if v.flagged.is_empty() {
            return None;
        }
        let action = match self.action {
            Action::Reject => "reject",
            Action::Flag => "flag",
        };
        for c in &v.flagged {
            info.guardrails.add(NAME, c, action, 1);
        }
        if self.action == Action::Flag {
            tracing::info!(request_id = %info.request_id, categories = ?v.flagged, "content-safety: markiert");
            return None;
        }
        let what = if output { "Antwort" } else { "Anfrage" };
        Some(
            ApiError::invalid_request(format!(
                "{what} von der Inhaltsprüfung blockiert (Kategorien: {})",
                v.flagged.join(", ")
            ))
            .with_code("content_filtered"),
        )
    }

    fn input_text(&self, req: &ChatRequest) -> String {
        let mut s = String::new();
        for m in messages(req, &self.roles) {
            visit(&m.content, &mut |t: &str| {
                if !s.is_empty() {
                    s.push_str("\n\n");
                }
                s.push_str(t);
            });
        }
        self.inner.tail(&s)
    }

    fn output_text(resp: &ChatResponse) -> String {
        let mut s = String::new();
        for c in &resp.choices {
            visit(&c.message.content, &mut |t: &str| s.push_str(t));
        }
        s
    }
}

/// Satzmodus: Zustand je Stream.
#[derive(Debug, Default)]
struct SentenceState {
    /// Zurückgehaltene Ereignisse mit der Nummer der Prüfung, die sie freigibt.
    held: VecDeque<(StreamEvent, usize)>,
    /// Laufende Prüfungen (Nummer, Ergebnis).
    checks: VecDeque<(usize, oneshot::Receiver<Result<Verdict, PluginError>>)>,
    /// Alle Prüfungen < `passed` sind bestanden.
    passed: usize,
    next: usize,
    /// Text seit der letzten Prüfung.
    sentence: String,
    /// Gesamter Ausgabetext (Kontext für die Prüfung).
    all: String,
}

/// Index hinter dem letzten Satzende (`.`, `!`, `?`, `\n` + Leerraum).
fn sentence_end(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    (0..b.len()).rev().find_map(|i| {
        let end = matches!(b[i], b'.' | b'!' | b'?' | b'\n')
            && b.get(i + 1).is_some_and(u8::is_ascii_whitespace);
        end.then_some(i + 1)
    })
}

impl ContentSafety {
    fn spawn_check(&self, st: &mut SentenceState) {
        let seq = st.next;
        st.next += 1;
        st.sentence.clear();
        let text = self.inner.tail(&st.all);
        let inner = Arc::clone(&self.inner);
        let timeout = self.sentence_timeout;
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let r = match tokio::time::timeout(timeout, inner.analyze(text, true)).await {
                Ok(r) => r,
                Err(_) => Err(PluginError::new("Satzprüfung: Zeitlimit überschritten")),
            };
            let _ = tx.send(r);
        });
        st.checks.push_back((seq, rx));
    }

    /// Wertet fertige Prüfungen aus (`wait` = auf alle offenen warten, Stream-Ende) und
    /// hängt freigegebene Ereignisse an `out` an. `Some(flow)` = Stream endet (Rest
    /// verworfen): [`ChunkFlow::Block`] bei `unsafe`, [`ChunkFlow::Reject`] bei
    /// fehlgeschlagener Prüfung mit `sentenceOnError: block`.
    ///
    /// Fehler einer Satzprüfung (Dienst, Zeitlimit) behandelt `sentenceOnError`: `block`
    /// beendet den Stream, `allow` gibt den Satz frei. Der Fehlermodus der Bindung greift im
    /// synchronen Chunk-Pfad nicht sauber (zurückgehaltene Ereignisse), daher eigene Regel.
    fn settle(
        &self,
        st: &mut SentenceState,
        info: &CallInfo,
        wait: bool,
        out: &mut Vec<StreamEvent>,
    ) -> Option<ChunkFlow> {
        loop {
            while st.held.front().is_some_and(|(_, need)| *need < st.passed) {
                if let Some((e, _)) = st.held.pop_front() {
                    out.push(e);
                }
            }
            let (seq, rx) = st.checks.front_mut()?;
            let seq = *seq;
            let r = match rx.try_recv() {
                Ok(r) => r,
                Err(oneshot::error::TryRecvError::Closed) => {
                    Err(PluginError::new("Satzprüfung abgebrochen"))
                }
                Err(oneshot::error::TryRecvError::Empty) if wait => wait_for(rx),
                Err(oneshot::error::TryRecvError::Empty) => return None,
            };
            st.checks.pop_front();
            let blocked = match r {
                Ok(verdict) => self.judge(info, &verdict, true).map(ChunkFlow::Block),
                Err(e) => {
                    tracing::warn!(
                        request_id = %info.request_id,
                        error = %e,
                        on_error = ?self.sentence_on_error,
                        "content-safety: Satzprüfung fehlgeschlagen"
                    );
                    (self.sentence_on_error == OnError::Block).then(|| {
                        ChunkFlow::Reject(
                            ApiError::new(ErrorKind::Internal, "Inhaltsprüfung fehlgeschlagen")
                                .with_code("plugin_error"),
                        )
                    })
                }
            };
            if let Some(flow) = blocked {
                st.held.clear();
                st.checks.clear();
                return Some(flow);
            }
            st.passed = seq + 1;
        }
    }
}

/// Wartet am Stream-Ende synchron auf eine Prüfung (nur auf der Mehrthread-Laufzeit möglich).
fn wait_for(
    rx: &mut oneshot::Receiver<Result<Verdict, PluginError>>,
) -> Result<Verdict, PluginError> {
    use tokio::runtime::{Handle, RuntimeFlavor};
    let handle = Handle::try_current().map_err(|_| PluginError::new("keine Tokio-Laufzeit"))?;
    if handle.runtime_flavor() != RuntimeFlavor::MultiThread {
        return Err(PluginError::new(
            "Satzprüfung am Stream-Ende offen (Satzmodus braucht die Mehrthread-Laufzeit)",
        ));
    }
    tokio::task::block_in_place(|| handle.block_on(rx))
        .map_err(|_| PluginError::new("Satzprüfung abgebrochen"))?
}

impl Plugin for ContentSafety {
    fn name(&self) -> &'static str {
        NAME
    }

    fn phases(&self) -> Phases {
        let out = self.direction.output();
        Phases {
            request: self.direction.input(),
            response: out,
            chunk: out && self.output_mode == OutputMode::Sentence,
        }
    }

    fn needs_buffered_output(&self) -> bool {
        self.direction.output() && self.output_mode == OutputMode::Buffered
    }

    fn on_request<'a>(&'a self, ctx: &'a mut ReqCtx<'_>) -> Hook<'a, Flow> {
        let text = self.input_text(&ctx.request);
        if text.trim().is_empty() {
            return Hook::ok(Flow::Continue);
        }
        let info = ctx.info;
        Hook::future(async move {
            let v = self.inner.analyze(text, false).await?;
            Ok(match self.judge(info, &v, false) {
                Some(e) => Flow::Block(e),
                None => Flow::Continue,
            })
        })
    }

    fn on_response<'a>(&'a self, ctx: &'a mut RespCtx<'_>) -> Hook<'a, Flow> {
        let text = Self::output_text(&ctx.response);
        if text.trim().is_empty() {
            return Hook::ok(Flow::Continue);
        }
        let text = self.inner.tail(&text);
        let info = ctx.info;
        Hook::future(async move {
            let v = self.inner.analyze(text, true).await?;
            Ok(match self.judge(info, &v, true) {
                Some(e) => Flow::Block(e),
                None => Flow::Continue,
            })
        })
    }

    fn on_chunk(
        &self,
        ctx: &mut ChunkCtx<'_>,
        ev: &mut StreamEvent,
    ) -> Result<ChunkFlow, PluginError> {
        let info = ctx.info;
        let st = ctx.state::<SentenceState>();
        if let StreamEvent::TextDelta { text, .. } = &*ev {
            st.sentence.push_str(text);
            st.all.push_str(text);
            if st.all.len() > 4 * self.inner.max_chars {
                let cut = floor_boundary(&st.all, st.all.len() - 2 * self.inner.max_chars);
                st.all.drain(..cut);
            }
            if let Some(end) = sentence_end(&st.sentence)
                && end >= MIN_SENTENCE
            {
                let rest = st.sentence.split_off(end);
                self.spawn_check(st);
                st.sentence = rest;
            }
        }
        // Ereignis zurückhalten, bis die Prüfung für seinen Text bestanden ist.
        let need = if st.sentence.is_empty() {
            st.next.saturating_sub(1)
        } else {
            st.next
        };
        // Ereignisse ganz ohne Text vor der ersten Prüfung dürfen sofort hinaus.
        if st.next == 0 && st.sentence.is_empty() && st.held.is_empty() {
            return Ok(ChunkFlow::Continue);
        }
        st.held.push_back((
            std::mem::replace(ev, StreamEvent::Usage(Default::default())),
            need,
        ));
        let mut out = Vec::new();
        let res = self.settle(st, info, false, &mut out);
        for e in out {
            ctx.emit(e);
        }
        Ok(res.unwrap_or(ChunkFlow::Drop))
    }

    fn on_stream_end(&self, ctx: &mut ChunkCtx<'_>) -> Result<ChunkFlow, PluginError> {
        let info = ctx.info;
        let st = ctx.state::<SentenceState>();
        if !st.sentence.trim().is_empty() {
            self.spawn_check(st);
        }
        let mut out = Vec::new();
        let res = self.settle(st, info, true, &mut out);
        if res.is_none() {
            // Alle Prüfungen bestanden: Rest (z. B. nur Leerraum) freigeben.
            out.extend(st.held.drain(..).map(|(e, _)| e));
        }
        for e in out {
            ctx.emit(e);
        }
        Ok(res.unwrap_or(ChunkFlow::Continue))
    }
}

/// Fabrik von [`ContentSafety`].
#[derive(Debug, Clone)]
pub struct Factory {
    host: Arc<dyn PluginHost>,
}

impl Default for Factory {
    fn default() -> Self {
        Self::new(Arc::new(NoHost::default()))
    }
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
        Ok(Arc::new(ContentSafety::from_config(
            c,
            Arc::clone(&self.host),
        )?))
    }

    fn default_fail_mode(&self) -> FailMode {
        FailMode::Closed
    }

    fn default_timeout(&self) -> Duration {
        Duration::from_secs(3)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn category_names() {
        assert_eq!(norm("SelfHarm"), "self_harm");
        assert_eq!(norm("self-harm/intent"), "self_harm_intent");
        assert_eq!(norm("harassment/threatening"), "harassment_threatening");
        assert_eq!(norm("S10"), "s10");
        assert_eq!(norm("Hate"), "hate");
    }

    #[test]
    fn guard_replies() {
        assert_eq!(parse_guard_reply("safe").unwrap(), Vec::<String>::new());
        assert_eq!(
            parse_guard_reply("\n\nsafe\n").unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(parse_guard_reply("unsafe\nS1,S10").unwrap(), ["s1", "s10"]);
        assert_eq!(parse_guard_reply("UNSAFE: S2").unwrap(), ["s2"]);
        assert_eq!(parse_guard_reply("unsafe").unwrap(), ["unsafe"]);
        assert!(parse_guard_reply("Ich kann das nicht beurteilen").is_err());
        assert!(parse_guard_reply("").is_err());
    }

    #[test]
    fn sentence_boundaries() {
        assert_eq!(sentence_end("Hallo Welt. Wie"), Some(11));
        assert_eq!(sentence_end("Version 3.5 ist"), None);
        assert_eq!(sentence_end("Ende."), None); // erst mit folgendem Leerraum
    }

    #[test]
    fn config_validation() {
        let f = Factory::default();
        let ok = |v: Value| f.build(&v).map(drop);
        assert!(ok(json!({"provider": "openai", "credential": "oai"})).is_ok());
        assert!(ok(json!({"provider": "openai"})).is_err()); // Zugangsdaten fehlen
        assert!(ok(json!({"provider": "azure", "credential": "c"})).is_err()); // endpoint
        assert!(
            ok(json!({"provider": "azure", "credential": "c",
                      "endpoint": "https://x.cognitiveservices.azure.com"}))
            .is_ok()
        );
        // SSRF: private Ziele und http nur nach Freischaltung.
        assert!(
            ok(json!({"provider": "azure", "credential": "c", "endpoint": "https://10.0.0.1"}))
                .is_err()
        );
        assert!(
            ok(json!({"provider": "openai", "credential": "c", "endpoint": "http://example.com"}))
                .is_err()
        );
        assert!(ok(json!({"provider": "model"})).is_err());
        assert!(ok(json!({"provider": "model", "modelGroup": "llama-guard"})).is_ok());
        assert!(
            ok(json!({"provider": "model", "modelGroup": "g", "promptTemplate": "kein Platzhalter"}))
                .is_err()
        );
        assert!(ok(json!({"provider": "openai", "credential": "c", "apiKey": "sk-x"})).is_err());
    }
}
