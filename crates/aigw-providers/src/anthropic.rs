// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Adapter für die Anthropic Messages API (`POST /v1/messages`, AIGW-16).
//!
//! - URL `{endpoint}/v1/messages` (Standard `https://api.anthropic.com`; endet `endpoint` auf
//!   `/v1`, wird nur `/messages` angehängt).
//! - Header `x-api-key` (Zugangsdaten `apiKey`, Pflicht), `anthropic-version`
//!   (`settings.anthropicVersion`, Standard [`DEFAULT_ANTHROPIC_VERSION`]), `anthropic-beta`
//!   (`settings.anthropicBeta` plus – wenn `settings.forwardClientBeta` nicht `false` ist – die
//!   Beta-Kennungen aus dem `anthropic-beta`-Header von Anthropic-Clients).
//! - `max_tokens` ist Pflicht: fehlt es, gilt `settings.defaultMaxTokens` (Deployment oder
//!   geerbt von der Modellgruppe, siehe [`with_group_defaults`](crate::with_group_defaults)),
//!   sonst [`anthropic::DEFAULT_MAX_TOKENS`] (4096).
//! - Reasoning von Nicht-Anthropic-Clients (`reasoning_effort`) wird nach
//!   `settings.reasoningMode` abgebildet ([`ReasoningMode`], Tabelle [`budget_for_effort`]).
//! - Tools, `tool_choice`, Bilder (Base64 und URL), Dokumente, Thinking-Blöcke und
//!   `cache_control` übersetzt `aigw_core::wire::anthropic`.
//! - Stream: SSE-Ereignisse inkl. `ping` (ignoriert) und `error` (→ [`StreamEvent::Error`]);
//!   Usage inkl. `cache_creation_input_tokens`/`cache_read_input_tokens`.

use std::sync::Arc;

use aigw_core::ir::{
    ApiError, ChatRequest, ChatResponse, ErrorClass, ErrorKind, Sampling, StreamEvent, Thinking,
    UpstreamError,
};
use aigw_core::resource::SecretString;
use aigw_core::snapshot::DeploymentCtx;
use aigw_core::types::ProviderKind;
use aigw_core::wire::anthropic::{self, MessagesRequest};
use aigw_core::wire::sse::{SseEvent, SseParser};
use aigw_core::wire::{Losses, WireFormat};
use bytes::Bytes;
use http::header::{ACCEPT, CONTENT_TYPE, USER_AGENT};
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use serde_json::Value;

use crate::openai::{
    USER_AGENT_VALUE, internal, join, parse_uri, retry_after, sensitive, truncate,
};
use crate::settings::Settings;
use crate::{Capabilities, ConfigError, MaxTokensField, Provider, StreamDecoder, UpstreamRequest};

/// Standard-Endpunkt von Anthropic (ohne `/v1`).
pub const ANTHROPIC_DEFAULT_ENDPOINT: &str = "https://api.anthropic.com";
/// Standard-`anthropic-version`.
pub const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";
/// Ab dieser `anthropic-version` lehnt die API `temperature`/`top_p`/`top_k` ab.
const SAMPLING_REMOVED_SINCE: &str = "2026-10-21";

const X_API_KEY: HeaderName = HeaderName::from_static("x-api-key");
const ANTHROPIC_VERSION: HeaderName = HeaderName::from_static("anthropic-version");
const ANTHROPIC_BETA: HeaderName = HeaderName::from_static("anthropic-beta");

/// Abbildung von `reasoning_effort` (OpenAI-Clients) auf Anthropic (`settings.reasoningMode`).
///
/// Gilt nur, wenn die Anfrage **nicht** aus dem Anthropic-Format stammt und keine eigene
/// `thinking`-Konfiguration trägt; Anthropic-Clients steuern `thinking`/`output_config`
/// selbst. `reasoning_effort: none` entfernt das Reasoning in allen Modi.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningMode {
    /// `thinking: {type: adaptive}` + `output_config.effort` (Claude 4.6 und neuer; Standard).
    Adaptive,
    /// Nur `output_config.effort` (Modelle mit Thinking ohne Schalter, z. B. Opus 5.5).
    Effort,
    /// `thinking: {type: enabled, budget_tokens}` nach [`budget_for_effort`] (ältere Modelle wie
    /// Sonnet 4.5, Haiku 4.5; `effort` wird nicht gesendet).
    Budget,
    /// `reasoning_effort` wird verworfen.
    Off,
}

impl ReasoningMode {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "adaptive" => Self::Adaptive,
            "effort" => Self::Effort,
            "budget" => Self::Budget,
            "off" => Self::Off,
            _ => return None,
        })
    }
}

/// Thinking-Budget (Tokens) je `reasoning_effort` im Modus [`ReasoningMode::Budget`].
///
/// | `reasoning_effort` | `budget_tokens` |
/// |---|---|
/// | `minimal` | 1 024 (Minimum der API) |
/// | `low` | 2 048 |
/// | `medium` (und Unbekanntes) | 8 192 |
/// | `high` | 16 384 |
/// | `xhigh` | 24 576 |
/// | `max` | 32 768 |
pub fn budget_for_effort(effort: &str) -> u64 {
    match effort {
        "minimal" => 1024,
        "low" => 2048,
        "high" => 16_384,
        "xhigh" => 24_576,
        "max" => 32_768,
        _ => 8192,
    }
}

/// Umgang mit `max_tokens` ≤ Thinking-Budget im Modus [`ReasoningMode::Budget`]
/// (`settings.thinkingBudgetPolicy`, F59). Anthropic verlangt `budget_tokens < max_tokens`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThinkingBudgetPolicy {
    /// `max_tokens` um das Budget erhöhen (die Antwort behält ihren Platz; Standard). Kosten
    /// und Laufzeit können über das vom Client gesetzte `max_tokens` steigen.
    #[default]
    RaiseMaxTokens,
    /// `max_tokens` des Clients bleibt die harte Obergrenze: das Budget wird auf
    /// `max_tokens / 2` gekürzt (Hälfte für Denken, Hälfte für die Antwort). Liegt das unter
    /// dem API-Minimum von 1 024 Tokens, wird ohne Thinking angefragt.
    CapBudget,
}

impl ThinkingBudgetPolicy {
    /// Aus `settings.thinkingBudgetPolicy` (`raise-max-tokens` | `cap-budget`).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "raise-max-tokens" => Some(Self::RaiseMaxTokens),
            "cap-budget" => Some(Self::CapBudget),
            _ => None,
        }
    }
}

/// Kleinstes Thinking-Budget der Anthropic-API.
const MIN_THINKING_BUDGET: u64 = 1024;

/// Wie eine IR-Anfrage für Anthropic vorbereitet wird (wiederverwendbar für Anthropic-Modelle
/// auf anderen Plattformen, z. B. Bedrock/Vertex).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestOptions {
    /// `max_tokens`, wenn die Anfrage keines setzt.
    pub default_max_tokens: u64,
    /// Abbildung von `reasoning_effort`.
    pub reasoning_mode: ReasoningMode,
    /// `temperature`/`top_p`/`top_k` verwerfen (API-Version ≥ 2026-10-21).
    pub drop_sampling: bool,
    /// Budget größer als `max_tokens` (F59).
    pub thinking_budget_policy: ThinkingBudgetPolicy,
}

impl Default for RequestOptions {
    fn default() -> Self {
        Self {
            default_max_tokens: anthropic::DEFAULT_MAX_TOKENS,
            reasoning_mode: ReasoningMode::Adaptive,
            drop_sampling: false,
            thinking_budget_policy: ThinkingBudgetPolicy::RaiseMaxTokens,
        }
    }
}

impl RequestOptions {
    /// IR → Anthropic-Wire-Typ (Modell muss bereits gesetzt sein).
    pub fn to_wire(
        &self,
        mut req: ChatRequest,
        losses: &mut Losses,
    ) -> Result<MessagesRequest, ApiError> {
        let from_anthropic = req.source == Some(WireFormat::AnthropicMessages);
        if req.max_tokens.is_none() {
            req.max_tokens = Some(self.default_max_tokens);
        }
        // Anthropic kennt keine `stream_options` (Usage kommt immer).
        req.stream_options = None;
        if !from_anthropic {
            self.map_reasoning(&mut req, losses);
        }
        if self.drop_sampling && clear_sampling(&mut req.sampling) {
            losses.note("temperature/top_p/top_k verworfen (anthropic-version ≥ 2026-10-21)");
        }
        anthropic::request_to_wire(req, losses)
    }

    fn map_reasoning(&self, req: &mut ChatRequest, losses: &mut Losses) {
        let Some(r) = req.reasoning.as_mut() else {
            return;
        };
        if r.thinking.is_some() {
            return;
        }
        let Some(effort) = r.effort.clone() else {
            return;
        };
        if effort == "none" {
            req.reasoning = None;
            return;
        }
        let thinking = |mode: &str, budget: Option<u64>| Thinking {
            mode: mode.to_owned(),
            budget_tokens: budget,
            display: None,
            extra: serde_json::Map::new(),
        };
        let enables_thinking = match self.reasoning_mode {
            ReasoningMode::Off => {
                losses.note(format!(
                    "reasoning_effort '{effort}' verworfen (reasoningMode: off)"
                ));
                req.reasoning = None;
                return;
            }
            ReasoningMode::Effort => false,
            ReasoningMode::Adaptive => {
                r.thinking = Some(thinking("adaptive", None));
                true
            }
            ReasoningMode::Budget => {
                let mut budget = budget_for_effort(&effort);
                r.effort = None;
                // budget_tokens muss kleiner als max_tokens sein (F59).
                let max = req.max_tokens.unwrap_or(self.default_max_tokens);
                if max <= budget {
                    match self.thinking_budget_policy {
                        ThinkingBudgetPolicy::RaiseMaxTokens => {
                            // Die Antwort behält ihren Platz.
                            req.max_tokens = Some(budget.saturating_add(max));
                            losses.note(format!(
                                "max_tokens {max} um das Thinking-Budget {budget} erhöht"
                            ));
                        }
                        ThinkingBudgetPolicy::CapBudget if max / 2 >= MIN_THINKING_BUDGET => {
                            losses.note(format!(
                                "Thinking-Budget {budget} auf {} gekürzt (max_tokens {max})",
                                max / 2
                            ));
                            budget = max / 2;
                        }
                        ThinkingBudgetPolicy::CapBudget => {
                            losses.note(format!(
                                "reasoning_effort '{effort}' verworfen: max_tokens {max} lässt \
                                 kein Thinking-Budget ≥ {MIN_THINKING_BUDGET} zu"
                            ));
                            req.reasoning = None;
                            return;
                        }
                    }
                }
                r.thinking = Some(thinking("enabled", Some(budget)));
                true
            }
        };
        if enables_thinking {
            // Mit Thinking akzeptiert die API nur Standardwerte für das Sampling.
            if clear_sampling(&mut req.sampling) {
                losses.note("temperature/top_p/top_k mit Thinking verworfen");
            }
        }
    }
}

/// Entfernt `temperature`/`top_p`/`top_k`; `true`, wenn etwas gesetzt war.
fn clear_sampling(s: &mut Sampling) -> bool {
    let removed = [
        s.temperature.take().is_some(),
        s.top_p.take().is_some(),
        s.top_k.take().is_some(),
    ];
    removed.contains(&true)
}

/// Adapter für die Anthropic Messages API.
#[derive(Debug, Clone)]
pub struct AnthropicAdapter {
    uri: Uri,
    provider_model: String,
    api_key: HeaderValue,
    version: HeaderValue,
    betas: Vec<String>,
    forward_client_beta: bool,
    options: RequestOptions,
    caps: Capabilities,
}

fn valid_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

impl AnthropicAdapter {
    /// Fabrik für die [`Registry`](crate::Registry).
    pub fn factory(dep: &DeploymentCtx) -> Result<Arc<dyn Provider>, ConfigError> {
        Ok(Arc::new(Self::from_deployment(dep)?))
    }

    /// Baut den Adapter aus einem Deployment (`provider: anthropic`).
    pub fn from_deployment(dep: &DeploymentCtx) -> Result<Self, ConfigError> {
        let s = Settings::of(dep);
        let spec = &dep.deployment.spec;
        let key = dep
            .credential
            .as_ref()
            .and_then(|c| c.get("apiKey").or_else(|| c.get("api_key")))
            .map(SecretString::expose)
            .ok_or_else(|| s.missing("Zugangsdaten (apiKey)"))?;
        let api_key = sensitive(key)
            .ok_or_else(|| s.invalid("Zugangsdaten enthalten unzulässige Zeichen"))?;

        let base = spec
            .endpoint
            .as_deref()
            .unwrap_or(ANTHROPIC_DEFAULT_ENDPOINT)
            .trim_end_matches('/');
        let url = if base.ends_with("/v1") {
            join(base, "/messages")
        } else {
            join(base, "/v1/messages")
        };

        let version = s
            .str("anthropicVersion")?
            .unwrap_or(DEFAULT_ANTHROPIC_VERSION);
        if !valid_token(version) {
            return Err(s.invalid(format!("settings.anthropicVersion {version:?} ungültig")));
        }
        let mut betas = Vec::new();
        for b in s.str_list("anthropicBeta")? {
            if !valid_token(b) {
                return Err(s.invalid(format!("settings.anthropicBeta: {b:?} ungültig")));
            }
            betas.push(b.to_owned());
        }
        let reasoning_mode = match s.str("reasoningMode")? {
            None => ReasoningMode::Adaptive,
            Some(m) => ReasoningMode::parse(m).ok_or_else(|| {
                s.invalid(format!(
                    "settings.reasoningMode = {m:?} unbekannt (adaptive, effort, budget, off)"
                ))
            })?,
        };
        let drop_sampling = match s.bool("dropSampling")? {
            Some(b) => b,
            None => version >= SAMPLING_REMOVED_SINCE,
        };

        Ok(Self {
            uri: parse_uri(&s, &url)?,
            provider_model: spec.provider_model.clone(),
            api_key,
            version: HeaderValue::from_str(version)
                .map_err(|_| s.invalid("settings.anthropicVersion ungültig"))?,
            betas,
            forward_client_beta: s.bool("forwardClientBeta")?.unwrap_or(true),
            options: RequestOptions {
                default_max_tokens: s
                    .positive_u64("defaultMaxTokens")?
                    .unwrap_or(anthropic::DEFAULT_MAX_TOKENS),
                reasoning_mode,
                drop_sampling,
                thinking_budget_policy: match s.str("thinkingBudgetPolicy")? {
                    None => ThinkingBudgetPolicy::default(),
                    Some(p) => ThinkingBudgetPolicy::parse(p).ok_or_else(|| {
                        s.invalid(format!(
                            "settings.thinkingBudgetPolicy = {p:?} unbekannt \
                             (raise-max-tokens, cap-budget)"
                        ))
                    })?,
                },
            },
            caps: Capabilities {
                chat: true,
                streaming: true,
                tools: true,
                vision: true,
                json_schema: true,
                reasoning: true,
                embeddings: false,
                images: false,
                audio_transcription: false,
                audio_speech: false,
                stream_usage: true,
                max_tokens_field: MaxTokensField::MaxTokens,
                n_choices: false,
            },
        })
    }

    /// Ziel-URL (für Tests und Diagnose).
    pub fn uri(&self) -> &Uri {
        &self.uri
    }

    /// Optionen der Anfrage-Übersetzung.
    pub fn options(&self) -> &RequestOptions {
        &self.options
    }

    /// `anthropic-beta`: konfigurierte Kennungen, dann die des Clients (ohne Duplikate).
    fn beta_header(&self, client: &[String]) -> Option<HeaderValue> {
        let mut all: Vec<&str> = self.betas.iter().map(String::as_str).collect();
        if self.forward_client_beta {
            for b in client.iter().map(|b| b.trim()) {
                if valid_token(b) && !all.contains(&b) {
                    all.push(b);
                }
            }
        }
        if all.is_empty() {
            return None;
        }
        HeaderValue::from_str(&all.join(",")).ok()
    }
}

impl Provider for AnthropicAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Anthropic
    }

    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    fn native_format(&self) -> WireFormat {
        WireFormat::AnthropicMessages
    }

    fn provider_model(&self) -> &str {
        &self.provider_model
    }

    fn build_request(
        &self,
        mut req: ChatRequest,
        losses: &mut Losses,
    ) -> Result<UpstreamRequest, ApiError> {
        req.model.clone_from(&self.provider_model);
        let stream = req.is_stream();
        let beta = self.beta_header(&req.hints.anthropic_beta);
        let wire = self.options.to_wire(req, losses)?;
        let body = serde_json::to_vec(&wire).map_err(internal)?;

        let mut builder = http::Request::builder()
            .method(Method::POST)
            .uri(self.uri.clone())
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .header(
                ACCEPT,
                HeaderValue::from_static(if stream {
                    "text/event-stream"
                } else {
                    "application/json"
                }),
            )
            .header(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE))
            .header(X_API_KEY, self.api_key.clone())
            .header(ANTHROPIC_VERSION, self.version.clone());
        if let Some(b) = beta {
            builder = builder.header(ANTHROPIC_BETA, b);
        }
        builder.body(Bytes::from(body)).map_err(internal)
    }

    fn parse_response(&self, body: &[u8]) -> Result<ChatResponse, ApiError> {
        anthropic::decode_response(body).map_err(|e| {
            ApiError::new(
                ErrorKind::BadGateway,
                format!("Antwort des Anbieters nicht lesbar: {}", e.message),
            )
            .with_code("upstream_protocol_error")
        })
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(AnthropicSseDecoder::new())
    }

    fn classify_error(
        &self,
        status: StatusCode,
        headers: &HeaderMap,
        body: &[u8],
    ) -> UpstreamError {
        classify_anthropic_error(status, headers, body)
    }
}

// ---------------------------------------------------------------------------------------------
// Fehlerklassifikation
// ---------------------------------------------------------------------------------------------

fn is_context_length(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("prompt is too long")
        || m.contains("context limit")
        || m.contains("context window")
        || m.contains("context length")
}

/// Klassifiziert eine Fehlerantwort von Anthropic
/// (`{"type":"error","error":{"type":…,"message":…},"request_id":…}`).
///
/// | `error.type` / Status | Klasse | retrybar |
/// |---|---|---|
/// | `overloaded_error` / 529, 503 | `Overloaded` | ja |
/// | `rate_limit_error` / 429 | `RateLimit` (`retry-after`) | ja |
/// | `billing_error` / 402 | `Quota` | nein (Fallback) |
/// | `invalid_request_error` „prompt is too long“ | `ContextLength` | nein |
/// | `authentication_error`, `permission_error` / 401, 403 | `Auth` (Meldung verborgen, 502) | nein |
/// | `not_found_error` / 404 | `NotFound` | nein |
/// | `request_too_large` / 413 | `InvalidRequest` | nein |
/// | `api_error` / 500 | `ServerError` | ja |
/// | `timeout_error` / 504 | `Timeout` | ja |
pub fn classify_anthropic_error(
    status: StatusCode,
    headers: &HeaderMap,
    body: &[u8],
) -> UpstreamError {
    let s = status.as_u16();
    let (kind, message) = match serde_json::from_slice::<Value>(body) {
        Ok(v) => {
            let e = v.get("error");
            let t = e
                .and_then(|e| e.get("type"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let m = e
                .and_then(|e| e.get("message").or(Some(e)))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            (t, m)
        }
        Err(_) => (None, String::from_utf8_lossy(body).trim().to_owned()),
    };
    let class = match (kind.as_deref(), s) {
        (Some("overloaded_error"), _) | (_, 503 | 529) => ErrorClass::Overloaded,
        (Some("rate_limit_error"), _) | (_, 429) => ErrorClass::RateLimit,
        (Some("billing_error"), _) | (_, 402) => ErrorClass::Quota,
        (Some("authentication_error" | "permission_error"), _) | (_, 401 | 403) => ErrorClass::Auth,
        (Some("not_found_error"), _) | (_, 404) => ErrorClass::NotFound,
        (_, 400 | 413) if is_context_length(&message) => ErrorClass::ContextLength,
        (Some("timeout_error"), _) | (_, 408 | 504) => ErrorClass::Timeout,
        (Some("api_error"), _) | (_, 500..=599) => ErrorClass::ServerError,
        (_, 400..=499) => ErrorClass::InvalidRequest,
        _ => ErrorClass::Protocol,
    };
    let message = if message.is_empty() {
        format!("Anbieter meldet HTTP {s}")
    } else {
        truncate(message)
    };
    let mut api = ApiError {
        kind: match class {
            ErrorClass::Overloaded => ErrorKind::Overloaded,
            ErrorClass::RateLimit | ErrorClass::Quota => ErrorKind::RateLimit,
            ErrorClass::Timeout => ErrorKind::Timeout,
            ErrorClass::ServerError => ErrorKind::BadGateway,
            _ => ErrorKind::from_status(s),
        },
        message,
        param: None,
        code: kind,
        exact_status: None,
    };
    // F152: sonstige 4xx (z. B. 422) mit exaktem Status durchreichen.
    if class == ErrorClass::InvalidRequest && s != 400 {
        api = api.with_status(s);
    }
    match class {
        ErrorClass::Auth => {
            api = ApiError::new(
                ErrorKind::BadGateway,
                format!("Anbieter lehnt die Zugangsdaten des Gateways ab (HTTP {s})"),
            )
            .with_code("upstream_authentication_error");
        }
        ErrorClass::NotFound => {
            api.kind = ErrorKind::BadGateway;
            api.message = format!("Anbieter kennt das Modell nicht: {}", api.message);
            api.code = Some("upstream_not_found".to_owned());
        }
        ErrorClass::Quota => {
            api.code = Some("insufficient_quota".to_owned());
        }
        ErrorClass::ContextLength => {
            api.kind = ErrorKind::InvalidRequest;
            api.code = Some("context_length_exceeded".to_owned());
        }
        ErrorClass::Protocol => {
            api.kind = ErrorKind::BadGateway;
            api.code = Some("upstream_protocol_error".to_owned());
        }
        _ => {}
    }
    UpstreamError::new(class, api)
        .with_status(s)
        .with_retry_after(retry_after(headers))
}

// ---------------------------------------------------------------------------------------------
// Stream-Decoder
// ---------------------------------------------------------------------------------------------

/// Anthropic-SSE (`event: …` / `data: {…}`) → IR-Ereignisse.
///
/// `ping` und unbekannte Ereignisse werden ignoriert, `error` beendet den Stream mit
/// [`StreamEvent::Error`]. Endet der Body ohne `message_stop` und ohne Endgrund, folgt
/// `upstream_stream_truncated`.
#[derive(Debug, Default)]
pub struct AnthropicSseDecoder {
    sse: SseParser,
    inner: anthropic::StreamDecoder,
    events: Vec<SseEvent>,
    stopped: bool,
    done: bool,
}

impl AnthropicSseDecoder {
    /// Neuer Decoder.
    pub fn new() -> Self {
        Self::default()
    }

    fn data(&mut self, data: &str, out: &mut Vec<StreamEvent>) {
        if self.done {
            return;
        }
        match self.inner.decode(data) {
            Ok(events) => {
                for ev in events {
                    match ev {
                        StreamEvent::Stop { .. } => self.stopped = true,
                        StreamEvent::Error(_) => self.done = true,
                        _ => {}
                    }
                    out.push(match ev {
                        // `api_error` des Anbieters ist kein interner Fehler des Gateways.
                        StreamEvent::Error(mut e) if e.kind == ErrorKind::Internal => {
                            e.kind = ErrorKind::BadGateway;
                            StreamEvent::Error(e)
                        }
                        other => other,
                    });
                }
                if self.inner.is_done() {
                    self.done = true;
                }
            }
            Err(e) => {
                out.push(StreamEvent::Error(
                    ApiError::new(
                        ErrorKind::BadGateway,
                        format!("Anbieter-Stream nicht lesbar: {e}"),
                    )
                    .with_code("upstream_protocol_error"),
                ));
                self.done = true;
            }
        }
    }
}

impl StreamDecoder for AnthropicSseDecoder {
    fn push(&mut self, chunk: &[u8], out: &mut Vec<StreamEvent>) {
        if self.done {
            return;
        }
        let mut events = std::mem::take(&mut self.events);
        self.sse.push_into(chunk, &mut events);
        for ev in events.drain(..) {
            self.data(&ev.data, out);
        }
        self.events = events;
    }

    fn finish(&mut self, out: &mut Vec<StreamEvent>) {
        if self.done {
            return;
        }
        self.done = true;
        if !self.stopped {
            out.push(StreamEvent::Error(
                ApiError::new(
                    ErrorKind::BadGateway,
                    "Anbieter-Stream endete unerwartet (ohne message_stop)",
                )
                .with_code("upstream_stream_truncated"),
            ));
        }
    }

    fn is_done(&self) -> bool {
        self.done
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::collections::BTreeMap;

    use aigw_core::ir::{
        Content, MediaSource, Message, Part, PartKind, Reasoning, Role, StopReason, ToolChoice,
    };
    use aigw_core::resource::{
        CredentialData, Deployment, DeploymentSpec, JsonObject, ObjectMeta, new_id,
    };
    use serde_json::json;

    use super::*;

    fn dep(endpoint: Option<&str>, key: Option<&str>, settings: Value) -> DeploymentCtx {
        let Value::Object(settings) = settings else {
            panic!()
        };
        let settings: JsonObject = settings;
        DeploymentCtx {
            deployment: Deployment {
                id: new_id(),
                metadata: ObjectMeta::named("claude"),
                spec: DeploymentSpec {
                    model_group: "g".into(),
                    provider: ProviderKind::Anthropic,
                    endpoint: endpoint.map(str::to_owned),
                    provider_model: "claude-opus-5-5".into(),
                    credential: key.map(|_| "cred".to_owned()),
                    weight: 1,
                    priority: 0,
                    capacity: Default::default(),
                    enabled: true,
                    settings,
                },
            },
            credential: key.map(|k| {
                Arc::new(CredentialData(BTreeMap::from([(
                    "apiKey".to_owned(),
                    SecretString::new(k),
                )])))
            }),
        }
    }

    fn openai_req() -> ChatRequest {
        ChatRequest {
            model: "alias".into(),
            messages: vec![
                Message::text(Role::System, "sei kurz"),
                Message::text(Role::User, "hi"),
            ],
            source: Some(WireFormat::OpenAiChat),
            ..ChatRequest::default()
        }
    }

    fn body(r: &UpstreamRequest) -> Value {
        serde_json::from_slice(r.body()).unwrap()
    }

    #[test]
    fn headers_url_and_default_max_tokens() {
        let a = AnthropicAdapter::from_deployment(&dep(
            None,
            Some("sk-ant"),
            json!({"anthropicBeta": ["context-1m-2025-08-07"]}),
        ))
        .unwrap();
        assert_eq!(a.uri(), "https://api.anthropic.com/v1/messages");
        let mut req = openai_req();
        req.hints.anthropic_beta = vec![
            "files-api-2025-04-14".into(),
            "context-1m-2025-08-07".into(),
        ];
        let r = a.build_request(req, &mut Losses::new()).unwrap();
        assert_eq!(r.headers()["x-api-key"], "sk-ant");
        assert!(r.headers()["x-api-key"].is_sensitive());
        assert_eq!(r.headers()["anthropic-version"], "2023-06-01");
        assert_eq!(
            r.headers()["anthropic-beta"],
            "context-1m-2025-08-07,files-api-2025-04-14"
        );
        assert!(r.headers().get("authorization").is_none());
        let b = body(&r);
        assert_eq!(b["model"], "claude-opus-5-5");
        assert_eq!(b["max_tokens"], 4096);
        assert_eq!(b["system"], "sei kurz");
        assert!(b.get("stream_options").is_none());

        let a = AnthropicAdapter::from_deployment(&dep(
            Some("http://127.0.0.1:9/v1/"),
            Some("k"),
            json!({"defaultMaxTokens": 1000, "forwardClientBeta": false, "anthropicVersion": "2026-10-21"}),
        ))
        .unwrap();
        assert_eq!(a.uri(), "http://127.0.0.1:9/v1/messages");
        let mut req = openai_req();
        req.hints.anthropic_beta = vec!["x".into()];
        req.sampling = Sampling {
            temperature: Some(0.2),
            ..Sampling::default()
        };
        let mut losses = Losses::new();
        let r = a.build_request(req, &mut losses).unwrap();
        assert!(r.headers().get("anthropic-beta").is_none());
        assert_eq!(r.headers()["anthropic-version"], "2026-10-21");
        let b = body(&r);
        assert_eq!(b["max_tokens"], 1000);
        assert!(b.get("temperature").is_none());
        assert!(!losses.is_empty());

        assert!(AnthropicAdapter::from_deployment(&dep(None, None, json!({}))).is_err());
        assert!(
            AnthropicAdapter::from_deployment(&dep(
                None,
                Some("k"),
                json!({"reasoningMode": "magic"})
            ))
            .is_err()
        );
        assert!(
            AnthropicAdapter::from_deployment(&dep(
                None,
                Some("k"),
                json!({"anthropicVersion": "2023 06"})
            ))
            .is_err()
        );
    }

    #[test]
    fn tools_vision_and_cache_control() {
        let a = AnthropicAdapter::from_deployment(&dep(None, Some("k"), json!({}))).unwrap();
        let mut req = openai_req();
        let mut img = Part::new(PartKind::Image {
            source: MediaSource::Url {
                url: "https://example.org/a.png".into(),
            },
            detail: None,
        });
        img.cache_control = Some(json!({"type": "ephemeral"}));
        req.messages.push(Message {
            role: Role::User,
            content: Content::Parts(vec![
                Part::text("was ist das?"),
                img,
                Part::new(PartKind::Image {
                    source: MediaSource::Base64 {
                        media_type: Some("image/jpeg".into()),
                        data: "AAAA".into(),
                    },
                    detail: None,
                }),
            ]),
            name: None,
            extra: serde_json::Map::new(),
        });
        req.tools = vec![aigw_core::ir::Tool {
            kind: aigw_core::ir::ToolKind::Function(aigw_core::ir::FunctionTool {
                name: "wetter".into(),
                description: Some("Wetter".into()),
                parameters: Some(
                    json!({"type": "object", "properties": {"ort": {"type": "string"}}}),
                ),
                strict: None,
                extra: serde_json::Map::new(),
            }),
            cache_control: None,
            extra: serde_json::Map::new(),
        }];
        req.tool_choice = Some(ToolChoice::Required);
        req.parallel_tool_calls = Some(false);
        let b = body(&a.build_request(req, &mut Losses::new()).unwrap());
        assert_eq!(b["tools"][0]["name"], "wetter");
        assert_eq!(b["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(
            b["tool_choice"],
            json!({"type": "any", "disable_parallel_tool_use": true})
        );
        let content = &b["messages"][1]["content"];
        assert_eq!(
            content[1]["source"],
            json!({"type": "url", "url": "https://example.org/a.png"})
        );
        assert_eq!(content[1]["cache_control"]["type"], "ephemeral");
        assert_eq!(content[2]["source"]["type"], "base64");
        assert_eq!(content[2]["source"]["media_type"], "image/jpeg");
    }

    #[test]
    fn thinking_budget_policy() {
        let build = |policy: Option<&str>, max: u64| {
            let mut settings = json!({"reasoningMode": "budget"});
            if let Some(p) = policy {
                settings["thinkingBudgetPolicy"] = json!(p);
            }
            let a = AnthropicAdapter::from_deployment(&dep(None, Some("k"), settings)).unwrap();
            let mut req = with_effort("high");
            req.max_tokens = Some(max);
            body(&a.build_request(req, &mut Losses::new()).unwrap())
        };
        // Standard: max_tokens wird erhöht.
        let b = build(None, 4000);
        assert_eq!(b["max_tokens"], 16384 + 4000);
        assert_eq!(b["thinking"]["budget_tokens"], 16384);
        // cap-budget: max_tokens bleibt, Budget = Hälfte.
        let b = build(Some("cap-budget"), 4000);
        assert_eq!(b["max_tokens"], 4000);
        assert_eq!(b["thinking"]["budget_tokens"], 2000);
        // Zu klein für das API-Minimum: ohne Thinking.
        let b = build(Some("cap-budget"), 1500);
        assert_eq!(b["max_tokens"], 1500);
        assert!(b.get("thinking").is_none());
        // Budget passt ohnehin: unverändert.
        let b = build(Some("cap-budget"), 40_000);
        assert_eq!(b["max_tokens"], 40_000);
        assert_eq!(b["thinking"]["budget_tokens"], 16384);
        assert!(
            AnthropicAdapter::from_deployment(&dep(
                None,
                Some("k"),
                json!({"thinkingBudgetPolicy": "viel"})
            ))
            .is_err()
        );
    }

    fn with_effort(effort: &str) -> ChatRequest {
        let mut req = openai_req();
        req.reasoning = Some(Reasoning {
            effort: Some(effort.into()),
            thinking: None,
        });
        req.max_tokens = Some(1000);
        req.sampling.temperature = Some(0.3);
        req
    }

    #[test]
    fn reasoning_effort_mapping() {
        let build = |mode: &str, req: ChatRequest| {
            let a = AnthropicAdapter::from_deployment(&dep(
                None,
                Some("k"),
                json!({"reasoningMode": mode}),
            ))
            .unwrap();
            body(&a.build_request(req, &mut Losses::new()).unwrap())
        };
        let b = build("adaptive", with_effort("high"));
        assert_eq!(b["thinking"], json!({"type": "adaptive"}));
        assert_eq!(b["output_config"]["effort"], "high");
        assert!(b.get("temperature").is_none());

        let b = build("effort", with_effort("medium"));
        assert!(b.get("thinking").is_none());
        assert_eq!(b["output_config"]["effort"], "medium");
        assert_eq!(b["temperature"], 0.3);

        let b = build("budget", with_effort("high"));
        assert_eq!(
            b["thinking"],
            json!({"type": "enabled", "budget_tokens": 16384})
        );
        assert!(b.get("output_config").is_none());
        // 1000 ≤ 16384 → max_tokens = Budget + 1000.
        assert_eq!(b["max_tokens"], 17384);

        let b = build("off", with_effort("high"));
        assert!(b.get("thinking").is_none() && b.get("output_config").is_none());

        let b = build("adaptive", with_effort("none"));
        assert!(b.get("thinking").is_none() && b.get("output_config").is_none());

        // Anthropic-Clients steuern selbst: kein thinking ergänzt.
        let mut req = with_effort("low");
        req.source = Some(WireFormat::AnthropicMessages);
        let b = build("budget", req);
        assert!(b.get("thinking").is_none());
        assert_eq!(b["output_config"]["effort"], "low");

        for (e, n) in [
            ("minimal", 1024),
            ("low", 2048),
            ("medium", 8192),
            ("xhigh", 24_576),
            ("max", 32_768),
        ] {
            assert_eq!(budget_for_effort(e), n);
        }
    }

    fn classify(status: u16, headers: &[(&str, &str)], body: Value) -> UpstreamError {
        let mut h = HeaderMap::new();
        for (k, v) in headers {
            h.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        classify_anthropic_error(
            StatusCode::from_u16(status).unwrap(),
            &h,
            body.to_string().as_bytes(),
        )
    }

    fn err(t: &str, m: &str) -> Value {
        json!({"type": "error", "error": {"type": t, "message": m}, "request_id": "req_1"})
    }

    #[test]
    fn classifies_errors() {
        let e = classify(529, &[], err("overloaded_error", "Overloaded"));
        assert_eq!(e.class, ErrorClass::Overloaded);
        assert!(e.retryable);
        assert_eq!(e.error.kind, ErrorKind::Overloaded);
        assert_eq!(e.error.code.as_deref(), Some("overloaded_error"));

        let e = classify(
            429,
            &[("retry-after", "7")],
            err(
                "rate_limit_error",
                "Number of request tokens has exceeded your per-minute rate limit",
            ),
        );
        assert_eq!(e.class, ErrorClass::RateLimit);
        assert!(e.retryable);
        assert_eq!(e.retry_after, Some(std::time::Duration::from_secs(7)));

        let e = classify(402, &[], err("billing_error", "credit balance too low"));
        assert_eq!(e.class, ErrorClass::Quota);
        assert!(!e.retryable);

        let e = classify(
            400,
            &[],
            err(
                "invalid_request_error",
                "prompt is too long: 210000 tokens > 200000 maximum",
            ),
        );
        assert_eq!(e.class, ErrorClass::ContextLength);
        assert_eq!(e.error.code.as_deref(), Some("context_length_exceeded"));

        let e = classify(
            401,
            &[],
            err("authentication_error", "invalid x-api-key sk-ant-xyz"),
        );
        assert_eq!(e.class, ErrorClass::Auth);
        assert!(!e.error.message.contains("sk-ant"));
        assert_eq!(e.error.kind, ErrorKind::BadGateway);

        let e = classify(404, &[], err("not_found_error", "model: x"));
        assert_eq!(e.class, ErrorClass::NotFound);
        let e = classify(500, &[], err("api_error", "Internal server error"));
        assert_eq!(e.class, ErrorClass::ServerError);
        assert!(e.retryable);
        let e = classify(504, &[], err("timeout_error", "timeout"));
        assert_eq!(e.class, ErrorClass::Timeout);
        let e = classify(413, &[], err("request_too_large", "too large"));
        assert_eq!(e.class, ErrorClass::InvalidRequest);
        assert_eq!(e.error.kind, ErrorKind::RequestTooLarge);
        let e = classify(400, &[], err("invalid_request_error", "messages: bad"));
        assert_eq!(e.class, ErrorClass::InvalidRequest);
        assert_eq!(e.error.message, "messages: bad");
    }

    #[test]
    fn decoder_handles_events_ping_usage_and_split_chunks() {
        let input = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":5,\"output_tokens\":1,\"cache_creation_input_tokens\":3,\"cache_read_input_tokens\":7}}}\n\n",
            "event: ping\ndata: {\"type\":\"ping\"}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hä\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"wetter\",\"input\":{}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"ort\\\":\\\"Berlin\\\"}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":12}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let mut d = AnthropicSseDecoder::new();
        let mut out = Vec::new();
        for c in input.as_bytes().chunks(7) {
            d.push(c, &mut out);
        }
        d.finish(&mut out);
        assert!(d.is_done());
        assert!(matches!(&out[0], StreamEvent::Start { id, .. } if id == "msg_1"));
        assert!(
            matches!(&out[1], StreamEvent::Usage(u) if u.input_tokens == 15 && u.cache_read_tokens == Some(7) && u.cache_write_tokens == Some(3))
        );
        assert!(matches!(&out[2], StreamEvent::TextDelta { text, .. } if text == "Hä"));
        assert!(matches!(&out[3], StreamEvent::ToolCallStart(t) if t.name == "wetter"));
        assert!(
            matches!(&out[4], StreamEvent::ToolCallDelta { arguments, .. } if arguments.contains("Berlin"))
        );
        assert!(matches!(
            &out[5],
            StreamEvent::Stop {
                reason: StopReason::ToolUse,
                ..
            }
        ));
        assert!(
            matches!(&out[6], StreamEvent::Usage(u) if u.output_tokens == 12 && u.input_tokens == 15)
        );
        assert_eq!(out.len(), 7);
    }

    #[test]
    fn decoder_reports_error_events_and_truncation() {
        let mut d = AnthropicSseDecoder::new();
        let mut out = Vec::new();
        d.push(
            b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
            &mut out,
        );
        assert!(
            matches!(&out[0], StreamEvent::Error(e) if e.kind == ErrorKind::Overloaded && e.code.as_deref() == Some("overloaded_error"))
        );
        assert!(d.is_done());

        let mut d = AnthropicSseDecoder::new();
        let mut out = Vec::new();
        d.push(
            b"data: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"x\"}}\n\n",
            &mut out,
        );
        assert!(matches!(&out[0], StreamEvent::Error(e) if e.kind == ErrorKind::BadGateway));

        let mut d = AnthropicSseDecoder::new();
        let mut out = Vec::new();
        d.push(b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"c\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n", &mut out);
        d.finish(&mut out);
        assert!(
            matches!(out.last(), Some(StreamEvent::Error(e)) if e.code.as_deref() == Some("upstream_stream_truncated"))
        );
    }

    #[test]
    fn parse_response_with_cache_usage() {
        let a = AnthropicAdapter::from_deployment(&dep(None, Some("k"), json!({}))).unwrap();
        let r = a
            .parse_response(
                json!({"id": "msg_1", "type": "message", "role": "assistant", "model": "claude",
                       "content": [{"type": "thinking", "thinking": "hm", "signature": "sig"},
                                   {"type": "text", "text": "Hallo"}],
                       "stop_reason": "end_turn", "stop_sequence": null,
                       "usage": {"input_tokens": 10, "output_tokens": 3,
                                 "cache_creation_input_tokens": 100, "cache_read_input_tokens": 50}})
                .to_string()
                .as_bytes(),
            )
            .unwrap();
        let u = r.usage.unwrap();
        assert_eq!(u.input_tokens, 160);
        assert_eq!(u.cache_write_tokens, Some(100));
        assert_eq!(u.cache_read_tokens, Some(50));
        assert_eq!(r.choices[0].message.content.text(), "Hallo");
        assert!(a.parse_response(b"{nope").is_err());
    }
}
