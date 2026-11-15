// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Plugin `content-safety`: externe Inhaltsprüfung (AIGW-38).
//!
//! ```yaml
//! plugin: content-safety
//! config:
//!   provider: azure                 # azure | openai | model
//!   endpoint: https://acme.cognitiveservices.azure.com
//!   secret: azure-content-safety    # Secret (F149; Feld value, apiKey oder api_key)
//!   # credential: …                 # älter: ProviderCredential (Feld apiKey/api_key)
//!   # auth: entraId                 # nur azure: Entra ID statt API-Key (AIGW-89), dann
//!   # credential: cs-entra          #   ProviderCredential mit spec.entra (Pflicht)
//!   thresholds: {hate: 2, violence: 4} # azure: Schweregrad 0–7 (Standard 4);
//!                                      # openai: Score 0–1 (ohne Angabe: flagged des Dienstes)
//!   categories: [hate, violence]    # nur diese blockieren (Standard: alle)
//!   direction: input                # input | output | both
//!   outputMode: buffered            # buffered | sentence (Stream satzweise)
//!   action: reject                  # reject (400 content_filtered) | flag
//!   roles: [user]                   # geprüfte Eingabe-Nachrichten
//!   maxChars: 10000                 # Fenstergröße; Eingabe/Ausgabe vollständig in Fenstern (max. 16)
//!   sentenceTimeoutMs: 2000         # Satzmodus: Zeitlimit je Prüfung
//!   sentenceOnError: block          # Satzmodus: block | allow bei Dienstfehlern
//!   maxChecks: 100                  # Obergrenze Prüfungen je Anfrage (AIGW-89/S-13)
//!   timeoutMs: 1500                 # Zeitlimit (Kettensteuerung), failMode: open|closed
//! ```
//!
//! - **Azure AI Content Safety:** `POST {endpoint}/contentsafety/text:analyze?api-version=…`,
//!   Header `Ocp-Apim-Subscription-Key`; mit `auth: entraId` stattdessen
//!   `Authorization: Bearer` mit einem Entra-ID-Token (Scope
//!   `https://cognitiveservices.azure.com/.default` bzw. `spec.entra.scope`) aus der
//!   Token-Quelle des Gateways (Managed Identity, Workload Identity, Client Credentials –
//!   wie Deployments mit `auth: entraId`). Der Endpunkt muss dann ein Azure-Host sein
//!   ([`ENTRA_HOST_SUFFIXES`]), damit das Token nie an fremde Hosts geht.
//! - **OpenAI Moderation:** `POST {endpoint|https://api.openai.com}/v1/moderations`,
//!   Modell `omni-moderation-latest` (änderbar über `model`).
//! - **Modell über das Gateway** (Llama Guard o. ä.): `modelGroup` + `promptTemplate`
//!   (Platzhalter `{{role}}`, `{{conversation}}`), Antwort `safe` bzw. `unsafe` + Kategorien
//!   (`S1,S10`) in der nächsten Zeile.
//!
//! Zugangsdaten nie in der Plugin-Konfiguration: nur der Name einer `ProviderCredential`, zur
//! Laufzeit aus dem aktuellen Snapshot aufgelöst. Endpunkte unterliegen den SSRF-Regeln
//! (`admin.allow_*_upstreams`).
//!
//! **Kosten und Obergrenze (AIGW-89/S-13):** Aufrufe des Prüfmodells (`provider: model`)
//! bucht der Gateway-Host in [`CallInfo::plugin_calls`]; der Recorder rechnet sie am Ende dem
//! Key/Team der Anfrage zu (Kosten im Request-Log, Budgets, Token-Limits). Je Anfrage laufen
//! höchstens `maxChecks` Prüfungen (Fenster, Sätze, Tool-Blöcke; alle Anbieter). Darüber
//! hinaus: in Anfrage/Antwort ein Plugin-Fehler (`failMode`), im Stream wie jeder Fehler einer
//! Satzprüfung (`sentenceOnError`: `block` beendet den Stream, `allow` gibt den Rest ungeprüft
//! frei).
//!
//! **Tool-Aufrufe im Stream (AIGW-89/S-10):** Im Satzmodus sammelt das Plugin die Argumente
//! jedes Tool-Blocks in einem eigenen Puffer und prüft sie am Blockende (nächster Block,
//! Text, Variantenwechsel, Stream-Ende) bzw. in Fenstern zu `maxChars`; die Ereignisse des
//! Blocks bleiben bis dahin zurückgehalten.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use aigw_core::ir::{
    ApiError, ChatRequest, ChatResponse, Content, ErrorKind, Message, Role, StreamEvent,
};
use aigw_core::observe::PluginCalls;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use super::guard::{Direction, RoleSel, floor_boundary, messages, visit};
use crate::binding::FailMode;
use crate::host::{HttpCall, NoHost, PluginHost};
use crate::plugin::{
    CallInfo, ChunkCtx, ChunkFlow, Flow, Hook, Phases, Plugin, PluginError, ReqCtx, RespCtx,
};
use crate::registry::{BuildCtx, PluginFactory};

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

/// Anmeldung beim Prüfdienst (`auth`, AIGW-89).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
enum AuthMode {
    /// API-Key aus `secret` bzw. `credential` (Standard).
    #[default]
    ApiKey,
    /// Microsoft Entra ID (nur `provider: azure`): `credential` mit `spec.entra`.
    EntraId,
}

/// Erlaubte Host-Endungen bei `auth: entraId` (Azure AI Services / Content Safety in den
/// Azure-Clouds) – das Bearer-Token der Gateway-Identität geht an keinen anderen Host.
pub const ENTRA_HOST_SUFFIXES: &[&str] = &[
    ".cognitiveservices.azure.com",
    ".cognitiveservices.azure.us",
    ".cognitiveservices.azure.cn",
    ".services.ai.azure.com",
];

/// Standard-Obergrenze der Prüfungen je Anfrage (`maxChecks`).
pub const DEFAULT_MAX_CHECKS: u32 = 100;

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
    secret: Option<String>,
    #[serde(default)]
    auth: AuthMode,
    #[serde(default)]
    max_checks: Option<u32>,
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
    secret: Option<String>,
    auth: AuthMode,
    max_checks: u32,
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
        // F149: bevorzugt ein `Secret`, sonst (älter) eine `ProviderCredential`.
        let (name, data, what) = match (&self.secret, &self.credential) {
            (Some(n), _) => (n, self.host.secret(n), "Geheimnis"),
            (None, Some(n)) => (n, self.host.credential(n), "Zugangsdaten"),
            (None, None) => return Err(PluginError::new("keine Zugangsdaten konfiguriert")),
        };
        let data =
            data.ok_or_else(|| PluginError::new(format!("{what} {name:?} nicht gefunden")))?;
        data.get("apiKey")
            .or_else(|| data.get("api_key"))
            .or_else(|| data.get(aigw_core::resource::SecretSpec::DEFAULT_FIELD))
            .map(|s| s.expose().to_owned())
            .ok_or_else(|| PluginError::new(format!("{what} {name:?} ohne apiKey/value")))
    }

    /// Header der Anmeldung beim Azure-Dienst: API-Key oder Entra-ID-Token.
    async fn azure_auth(&self) -> Result<(&'static str, String), PluginError> {
        match self.auth {
            AuthMode::ApiKey => Ok(("ocp-apim-subscription-key", self.api_key()?)),
            AuthMode::EntraId => {
                let name = self.credential.as_deref().unwrap_or_default();
                let token = self.host.entra_token(name).await?;
                Ok(("authorization", format!("Bearer {}", token.expose())))
            }
        }
    }

    /// Reserviert `n` Prüfungen dieser Anfrage (Obergrenze `maxChecks`, S-13).
    fn reserve(&self, calls: &PluginCalls, n: usize) -> Result<(), PluginError> {
        let n = u32::try_from(n).unwrap_or(u32::MAX);
        if calls.try_reserve(self.owner(), n, self.max_checks) {
            Ok(())
        } else {
            Err(PluginError::new(format!(
                "Obergrenze von {} Prüfungen je Anfrage erreicht (maxChecks)",
                self.max_checks
            )))
        }
    }

    /// Kennung dieser Plugin-Instanz für den Zähler je Anfrage.
    fn owner(&self) -> usize {
        std::ptr::from_ref(self) as usize
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

    /// Prüft `text` (Rolle: Eingabe = `User`, Ausgabe = `Agent`). Modellaufrufe werden in
    /// `calls` gebucht.
    async fn analyze(
        &self,
        text: String,
        output: bool,
        calls: &PluginCalls,
    ) -> Result<Verdict, PluginError> {
        let mut flagged = Vec::new();
        match self.provider {
            Provider::Azure => {
                let (header, value) = self.azure_auth().await?;
                let v = self
                    .post(
                        header,
                        value,
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
                let resp = self.host.chat(&self.model_group, req, calls).await?;
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

    /// Prüft `text` vollständig (AIGW-58): in Fenstern zu `max_chars` Bytes mit 10 %
    /// Überlappung, parallel; die Kategorien aller Fenster werden vereinigt. Früher wurde nur
    /// das Ende geprüft – schädlicher Inhalt am Anfang plus genügend harmloser Text dahinter
    /// blieb unentdeckt. Mehr als [`MAX_WINDOWS`] Fenster → Fehler (wirkt nach `failMode`).
    async fn analyze_all(
        &self,
        text: String,
        output: bool,
        calls: &PluginCalls,
    ) -> Result<Verdict, PluginError> {
        let parts = windows(&text, self.max_chars);
        if parts.len() > MAX_WINDOWS {
            return Err(PluginError::new(format!(
                "Text zu lang für die Inhaltsprüfung ({} Bytes, höchstens {} Fenster zu \
                 maxChars {})",
                text.len(),
                MAX_WINDOWS,
                self.max_chars
            )));
        }
        self.reserve(calls, parts.len())?;
        if parts.len() == 1 {
            return self.analyze(text, output, calls).await;
        }
        let checks = parts
            .into_iter()
            .map(|p| self.analyze(p.to_owned(), output, calls));
        let mut flagged: Vec<String> = futures_util::future::try_join_all(checks)
            .await?
            .into_iter()
            .flat_map(|v| v.flagged)
            .collect();
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

/// Endpunkt-Host gehört zu Azure AI Services (siehe [`ENTRA_HOST_SUFFIXES`]).
fn entra_host_allowed(endpoint: &str) -> bool {
    endpoint
        .parse::<http::Uri>()
        .ok()
        .and_then(|u| u.host().map(str::to_ascii_lowercase))
        .is_some_and(|h| {
            let h = h.trim_end_matches('.');
            ENTRA_HOST_SUFFIXES
                .iter()
                .any(|s| h.ends_with(s) && h.len() > s.len())
        })
}

/// Höchstzahl der Prüffenster je Anfrage/Antwort (AIGW-58).
const MAX_WINDOWS: usize = 16;

/// Zerlegt `text` in Fenster zu höchstens `max` Bytes (auf Zeichengrenzen) mit etwa 10 %
/// Überlappung, damit Treffer an Fenstergrenzen nicht verloren gehen.
fn windows(text: &str, max: usize) -> Vec<&str> {
    if text.len() <= max {
        return vec![text];
    }
    let step = (max - max / 10).max(1);
    let mut out = Vec::new();
    let mut start = 0;
    loop {
        let end = floor_boundary(text, start + max);
        out.push(&text[start..end]);
        if end >= text.len() {
            break;
        }
        let mut next = floor_boundary(text, start + step);
        if next <= start {
            // Zeichen länger als der Schritt (nur bei winzigem `max`): weiter am Fensterende.
            next = end;
        }
        start = next;
    }
    out
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
            match (&c.secret, &c.credential) {
                (Some(_), Some(_)) => Err("secret und credential: nur eines angeben".into()),
                (Some(n), None) | (None, Some(n)) if !n.trim().is_empty() => Ok(()),
                _ => Err(
                    "secret: Name eines Secret angeben (oder credential: ProviderCredential)"
                        .into(),
                ),
            }
        };
        if c.auth == AuthMode::EntraId && c.provider != Provider::Azure {
            return Err("auth: entraId nur bei provider: azure".into());
        }
        let url = match c.provider {
            Provider::Azure => {
                if c.auth == AuthMode::EntraId {
                    if c.secret.is_some() {
                        return Err("auth: entraId braucht credential (ProviderCredential mit \
                                    spec.entra), kein secret"
                            .into());
                    }
                    if c.credential.as_deref().is_none_or(|n| n.trim().is_empty()) {
                        return Err("auth: entraId: credential angeben (ProviderCredential \
                                    mit spec.entra)"
                            .into());
                    }
                } else {
                    needs_credential()?;
                }
                let v = c.api_version.as_deref().unwrap_or("2024-09-01");
                if !v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                    return Err("apiVersion ungültig".into());
                }
                let e = endpoint(None)?;
                if c.auth == AuthMode::EntraId && !entra_host_allowed(&e) {
                    return Err(format!(
                        "endpoint: bei auth: entraId nur Azure-Hosts ({})",
                        ENTRA_HOST_SUFFIXES.join(", ")
                    ));
                }
                format!("{e}/contentsafety/text:analyze?api-version={v}")
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
                if c.endpoint.is_some() || c.credential.is_some() || c.secret.is_some() {
                    return Err(
                        "provider model: endpoint/credential/secret kommen aus der Modellgruppe"
                            .into(),
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
        let max_checks = c.max_checks.unwrap_or(DEFAULT_MAX_CHECKS);
        if !(1..=10_000).contains(&max_checks) {
            return Err("maxChecks muss zwischen 1 und 10000 liegen".into());
        }
        Ok(Self {
            inner: Arc::new(Inner {
                host,
                provider: c.provider,
                url,
                credential: c.credential,
                secret: c.secret,
                auth: c.auth,
                max_checks,
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
        s
    }

    fn output_text(resp: &ChatResponse) -> String {
        let mut s = String::new();
        for c in &resp.choices {
            visit(&c.message.content, &mut |t: &str| s.push_str(t));
        }
        s
    }
}

/// Satzmodus: Ereignis wartet auf eine noch nicht gestartete Prüfung seines Tool-Blocks.
const PENDING: usize = usize::MAX;

/// Zurückgehaltenes Ereignis.
#[derive(Debug)]
struct Held {
    ev: StreamEvent,
    /// Freigabe, sobald so viele Prüfungen bestanden sind (`passed >= need`); [`PENDING`] =
    /// Prüfung des Tool-Blocks `tool` steht noch aus.
    need: usize,
    /// Tool-Block (Variante, Blockindex), zu dem das Ereignis gehört.
    tool: Option<(u32, u32)>,
}

/// Argumente eines offenen Tool-Blocks seit der letzten Prüfung (S-10).
#[derive(Debug)]
struct ToolBuf {
    key: (u32, u32),
    args: String,
    /// Enthält `args` ungeprüften Text (sonst nur die Überlappung zum vorigen Fenster)?
    fresh: bool,
}

/// Satzmodus: Zustand je Stream.
#[derive(Debug, Default)]
struct SentenceState {
    /// Zurückgehaltene Ereignisse in Stream-Reihenfolge.
    held: VecDeque<Held>,
    /// Laufende Prüfungen (Nummer, Ergebnis).
    checks: VecDeque<(usize, oneshot::Receiver<Result<Verdict, PluginError>>)>,
    /// Alle Prüfungen < `passed` sind bestanden.
    passed: usize,
    next: usize,
    /// Text seit der letzten Prüfung.
    sentence: String,
    /// Gesamter Ausgabetext (Kontext für die Prüfung).
    all: String,
    /// Offene Tool-Blöcke (S-10).
    tools: Vec<ToolBuf>,
    /// Aktuelle Antwortvariante (`StreamEvent::Choice`).
    choice: u32,
    /// Obergrenze `maxChecks` erreicht: keine weiteren Prüfungen.
    capped: bool,
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
    /// Startet eine Prüfung von `text` und gibt ihre Nummer zurück. Ist die Obergrenze
    /// `maxChecks` erreicht, liefert die erste überzählige Prüfung einen Fehler
    /// (`sentenceOnError` entscheidet), alle weiteren gelten als bestanden.
    fn spawn(&self, st: &mut SentenceState, calls: &PluginCalls, text: String) -> usize {
        let seq = st.next;
        st.next += 1;
        let (tx, rx) = oneshot::channel();
        if st.capped || self.inner.reserve(calls, 1).is_err() {
            let first = !st.capped;
            st.capped = true;
            let _ = tx.send(if first {
                Err(PluginError::new(format!(
                    "Obergrenze von {} Prüfungen je Anfrage erreicht (maxChecks)",
                    self.inner.max_checks
                )))
            } else {
                Ok(Verdict::default())
            });
        } else {
            let inner = Arc::clone(&self.inner);
            let calls = calls.clone();
            let timeout = self.sentence_timeout;
            tokio::spawn(async move {
                let r = match tokio::time::timeout(timeout, inner.analyze(text, true, &calls)).await
                {
                    Ok(r) => r,
                    Err(_) => Err(PluginError::new("Satzprüfung: Zeitlimit überschritten")),
                };
                let _ = tx.send(r);
            });
        }
        st.checks.push_back((seq, rx));
        seq
    }

    /// Prüft den Text seit der letzten Prüfung (mit dem Ende des bisherigen Textes als
    /// Kontext).
    fn spawn_text_check(&self, st: &mut SentenceState, calls: &PluginCalls) {
        st.sentence.clear();
        let text = self.inner.tail(&st.all);
        self.spawn(st, calls, text);
    }

    /// Schließt den Text vor einem Tool-Block mit eigener Prüfung ab.
    fn close_text(&self, st: &mut SentenceState, calls: &PluginCalls) {
        if st.sentence.trim().is_empty() {
            st.sentence.clear();
        } else {
            self.spawn_text_check(st, calls);
        }
    }

    /// Prüft volle Fenster (`maxChars`) des Tool-Blocks `i`; mit `end` auch den Rest und
    /// schließt den Block. Erst dann hängen die wartenden Ereignisse des Blocks an allen bis
    /// dahin gestarteten Prüfungen (ein Fragment kann über eine Fenstergrenze reichen).
    fn check_tool(&self, st: &mut SentenceState, calls: &PluginCalls, i: usize, end: bool) {
        let max = self.inner.max_chars;
        loop {
            let buf = &mut st.tools[i];
            let full = buf.args.len() >= max;
            if !full && !(end && buf.fresh && !buf.args.trim().is_empty()) {
                break;
            }
            let cut = if full {
                floor_boundary(&buf.args, max).max(1)
            } else {
                buf.args.len()
            };
            let mut cut = cut;
            while !buf.args.is_char_boundary(cut) {
                cut += 1;
            }
            let text = buf.args[..cut].to_owned();
            // 10 % Überlappung, damit Treffer an Fenstergrenzen nicht verloren gehen.
            let keep = if full {
                floor_boundary(&buf.args, cut.saturating_sub(max / 10))
            } else {
                cut
            };
            buf.args.drain(..keep);
            buf.fresh = buf.args.len() > cut - keep;
            self.spawn(st, calls, text);
            if !full {
                break;
            }
        }
        if !end {
            return;
        }
        let key = st.tools.remove(i).key;
        let need = st.next;
        for h in &mut st.held {
            if h.tool == Some(key) && h.need == PENDING {
                h.need = need;
            }
        }
    }

    /// Blockende: alle offenen Tool-Blöcke prüfen und schließen.
    fn flush_tools(&self, st: &mut SentenceState, calls: &PluginCalls) {
        while !st.tools.is_empty() {
            self.check_tool(st, calls, 0, true);
        }
    }

    /// Wertet fertige Prüfungen aus (`wait` = auf alle offenen warten, Stream-Ende) und
    /// hängt freigegebene Ereignisse an `out` an. `Some(flow)` = Stream endet (Rest
    /// verworfen): [`ChunkFlow::Block`] bei `unsafe`, [`ChunkFlow::Reject`] bei
    /// fehlgeschlagener Prüfung mit `sentenceOnError: block`.
    ///
    /// Fehler einer Satzprüfung (Dienst, Zeitlimit, Obergrenze) behandelt `sentenceOnError`:
    /// `block` beendet den Stream, `allow` gibt den Satz frei. Der Fehlermodus der Bindung
    /// greift im synchronen Chunk-Pfad nicht sauber (zurückgehaltene Ereignisse), daher eigene
    /// Regel.
    fn settle(
        &self,
        st: &mut SentenceState,
        info: &CallInfo,
        wait: bool,
        out: &mut Vec<StreamEvent>,
    ) -> Option<ChunkFlow> {
        loop {
            while st.held.front().is_some_and(|h| h.need <= st.passed) {
                if let Some(h) = st.held.pop_front() {
                    out.push(h.ev);
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
                st.tools.clear();
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
            let v = self
                .inner
                .analyze_all(text, false, &info.plugin_calls)
                .await?;
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
        let info = ctx.info;
        Hook::future(async move {
            let v = self
                .inner
                .analyze_all(text, true, &info.plugin_calls)
                .await?;
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
        let calls = &info.plugin_calls;
        let st = ctx.state::<SentenceState>();
        let mut tool = None;
        match &*ev {
            StreamEvent::ToolCallDelta { index, arguments } => {
                let key = (st.choice, *index);
                let i = match st.tools.iter().position(|b| b.key == key) {
                    Some(i) => i,
                    // Argumente ohne vorherigen Start: eigener Puffer.
                    None => {
                        self.close_text(st, calls);
                        st.tools.push(ToolBuf {
                            key,
                            args: String::new(),
                            fresh: false,
                        });
                        st.tools.len() - 1
                    }
                };
                st.tools[i].args.push_str(arguments);
                st.tools[i].fresh |= !arguments.is_empty();
                tool = Some((key, i));
            }
            other => {
                // Jedes andere Ereignis beendet offene Tool-Blöcke (S-10).
                self.flush_tools(st, calls);
                match other {
                    StreamEvent::ToolCallStart(s) => {
                        self.close_text(st, calls);
                        let key = (st.choice, s.index);
                        st.tools.push(ToolBuf {
                            key,
                            args: String::new(),
                            fresh: false,
                        });
                        tool = Some((key, st.tools.len() - 1));
                    }
                    StreamEvent::Choice { index } => st.choice = *index,
                    StreamEvent::TextDelta { text, .. } => {
                        st.sentence.push_str(text);
                        st.all.push_str(text);
                        if st.all.len() > 4 * self.inner.max_chars {
                            let cut =
                                floor_boundary(&st.all, st.all.len() - 2 * self.inner.max_chars);
                            st.all.drain(..cut);
                        }
                        if let Some(end) = sentence_end(&st.sentence)
                            && end >= MIN_SENTENCE
                        {
                            let rest = st.sentence.split_off(end);
                            self.spawn_text_check(st, calls);
                            st.sentence = rest;
                        }
                    }
                    _ => {}
                }
            }
        }
        // Ereignis zurückhalten, bis die Prüfung für seinen Inhalt bestanden ist.
        let need = match tool {
            Some(_) => PENDING,
            None if st.sentence.is_empty() => st.next,
            None => st.next + 1,
        };
        // Ereignisse ohne ausstehende Prüfung dürfen sofort hinaus, wenn nichts davor wartet.
        if need <= st.passed && st.held.is_empty() {
            return Ok(ChunkFlow::Continue);
        }
        st.held.push_back(Held {
            ev: std::mem::replace(ev, StreamEvent::Usage(Default::default())),
            need,
            tool: tool.map(|(k, _)| k),
        });
        if let Some((_, i)) = tool {
            // Volle Fenster des Blocks schon während des Streams prüfen.
            self.check_tool(st, calls, i, false);
        }
        let mut out = Vec::new();
        let res = self.settle(st, info, false, &mut out);
        for e in out {
            ctx.emit(e);
        }
        Ok(res.unwrap_or(ChunkFlow::Drop))
    }

    fn on_stream_end(&self, ctx: &mut ChunkCtx<'_>) -> Result<ChunkFlow, PluginError> {
        let info = ctx.info;
        let calls = &info.plugin_calls;
        let st = ctx.state::<SentenceState>();
        self.flush_tools(st, calls);
        if !st.sentence.trim().is_empty() {
            self.spawn_text_check(st, calls);
        }
        let mut out = Vec::new();
        let res = self.settle(st, info, true, &mut out);
        if res.is_none() {
            // Alle Prüfungen bestanden: Rest (z. B. nur Leerraum) freigeben.
            out.extend(st.held.drain(..).map(|h| h.ev));
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

    /// S-22: Bindungen auf Organisation/Team/Key nur gegen öffentliche Ziele.
    fn build_in(&self, ctx: &BuildCtx<'_>, config: &Value) -> Result<Arc<dyn Plugin>, String> {
        let c: Config = serde_json::from_value(config.clone()).map_err(|e| e.to_string())?;
        let host = if ctx.private_targets() {
            Arc::clone(&self.host)
        } else {
            self.host.public_only()
        };
        Ok(Arc::new(ContentSafety::from_config(c, host)?))
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

    /// AIGW-58: Der ganze Text wird geprüft, nicht nur das Ende.
    #[test]
    fn windows_cover_whole_text() {
        assert_eq!(windows("kurz", 100), vec!["kurz"]);
        let text = format!("BÖSE{}", "ä".repeat(500)); // 1004 Bytes, Mehrbyte-Zeichen
        let w = windows(&text, 100);
        assert!(w[0].starts_with("BÖSE"));
        assert!(w.iter().all(|p| p.len() <= 100 && !p.is_empty()));
        assert!(text.ends_with(w.last().unwrap()));
        // Lückenlos: jedes Fenster beginnt vor dem Ende des vorigen.
        let mut covered = 0;
        for p in &w {
            let start = p.as_ptr() as usize - text.as_ptr() as usize;
            assert!(start <= covered, "Lücke bei {start}");
            covered = start + p.len();
        }
        assert_eq!(covered, text.len());
        assert!(w.len() <= 14, "{}", w.len());
    }

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
        assert!(ok(json!({"provider": "openai", "secret": "oai"})).is_ok());
        assert!(ok(json!({"provider": "openai", "secret": "a", "credential": "b"})).is_err());
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
