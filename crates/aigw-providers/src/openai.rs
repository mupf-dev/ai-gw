// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Adapter der OpenAI-Familie: OpenAI, Azure OpenAI und generisch OpenAI-kompatible
//! Endpunkte (vLLM, Ollama, LM Studio, …).
//!
//! | | OpenAI | Azure OpenAI | kompatibel |
//! |---|---|---|---|
//! | URL | `{endpoint}/chat/completions`, Standard `https://api.openai.com/v1` | `{endpoint}/openai/deployments/{providerModel}/chat/completions?api-version=…` bzw. `{endpoint}/openai/v1/chat/completions` (`apiStyle: v1`) | `{endpoint}/chat/completions` |
//! | Auth | `Authorization: Bearer` (`apiKey`) | `api-key` (`apiKey`); Entra-ID über [`TokenSource`] | `Authorization: Bearer`, optional |
//! | Ausgabegrenze | `max_completion_tokens` | ab `api-version` 2024-09-01 bzw. `v1`: `max_completion_tokens`, sonst `max_tokens` | `max_tokens` |
//!
//! Profile derselben Implementierung: Ollama, vLLM ([`crate::local`]) und Mistral
//! ([`crate::mistral`]).
//!
//! Einstellungen (`spec.settings`): `organization`, `project` (OpenAI), `apiVersion`,
//! `apiStyle` (`deployment`/`v1`), `auth` (`apiKey`/`entraId`) (Azure), `maxTokensField`
//! (`max_tokens`/`max_completion_tokens`), `streamUsage` (bool), `defaultMaxTokens`,
//! `safePrompt` (Mistral), `timeouts`.

use std::sync::Arc;

use aigw_core::ir::audio::{
    AudioTask, SpeechRequest, TranscriptFormat, TranscriptionRequest, TranscriptionResponse,
};
use aigw_core::ir::embeddings::{EmbeddingsRequest, EmbeddingsResponse, EncodingFormat};
use aigw_core::ir::images::{ImageRequest, ImageResponse};
use aigw_core::ir::{
    ApiError, ChatRequest, ChatResponse, ErrorClass, ErrorKind, StreamEvent, UpstreamError,
};
use aigw_core::resource::SecretString;
use aigw_core::snapshot::DeploymentCtx;
use aigw_core::types::ProviderKind;
use aigw_core::wire::openai::{audio, embeddings, images};
use aigw_core::wire::sse::{SseEvent, SseParser};
use aigw_core::wire::{Losses, WireFormat, openai};
use bytes::Bytes;
use http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use serde_json::Value;

use crate::auth::TokenSource;
use crate::settings::Settings;
use crate::{
    Capabilities, Capability, ConfigError, MaxTokensField, Provider, StreamDecoder, UpstreamRequest,
};

/// Standard-Endpunkt von OpenAI.
pub const OPENAI_DEFAULT_ENDPOINT: &str = "https://api.openai.com/v1";
/// Standard-`api-version` für Azure OpenAI (letzte GA-Version mit Datum).
pub const AZURE_DEFAULT_API_VERSION: &str = "2024-10-21";
/// Erste Azure-`api-version`, die `max_completion_tokens` kennt.
const AZURE_MAX_COMPLETION_TOKENS_SINCE: &str = "2024-09-01";

/// Längste weitergegebene Fehlermeldung des Anbieters (Zeichen).
const MAX_ERROR_MESSAGE: usize = 1000;

/// Authentifizierung gegenüber dem Anbieter.
#[derive(Debug, Clone)]
enum Auth {
    /// Keine (lokale kompatible Server).
    None,
    /// Fester Header (`Authorization: Bearer …` bzw. `api-key: …`), als sensibel markiert.
    Header(HeaderName, HeaderValue),
    /// Bearer-Token aus einer [`TokenSource`] (Azure Entra ID).
    Token(Arc<dyn TokenSource>),
}

/// Adapter für OpenAI, Azure OpenAI und OpenAI-kompatible Endpunkte.
#[derive(Debug, Clone)]
pub struct OpenAiAdapter {
    kind: ProviderKind,
    uri: Uri,
    provider_model: String,
    auth: Auth,
    headers: Vec<(HeaderName, HeaderValue)>,
    caps: Capabilities,
    /// `settings.defaultMaxTokens` (gesetzt, wenn die Anfrage keine Grenze nennt).
    default_max_tokens: Option<u64>,
    /// Mistral: `settings.safePrompt`.
    safe_prompt: Option<bool>,
    /// Mistral: Abbildung von `reasoning_effort` (`settings.reasoningMode`, F61).
    mistral_reasoning: crate::mistral::MistralReasoning,
    /// URLs der weiteren Endpunkte (AIGW-21); `None` = nicht unterstützt.
    ops: Ops,
}

/// URLs der weiteren Endpunkte eines Deployments (einmal beim Aufbau berechnet).
#[derive(Debug, Clone, Default)]
struct Ops {
    embeddings: Option<Uri>,
    images: Option<Uri>,
    transcriptions: Option<Uri>,
    translations: Option<Uri>,
    speech: Option<Uri>,
}

impl Ops {
    /// Baut die URLs; `url(op)` liefert die vollständige URL einer Operation.
    fn build(
        s: &Settings<'_>,
        caps: &Capabilities,
        url: impl Fn(&str) -> Result<String, ConfigError>,
    ) -> Result<Self, ConfigError> {
        let mut o = Self::default();
        let get = |enabled: bool, op: &str| -> Result<Option<Uri>, ConfigError> {
            if enabled {
                parse_uri(s, &url(op)?).map(Some)
            } else {
                Ok(None)
            }
        };
        o.embeddings = get(caps.embeddings, "/embeddings")?;
        o.images = get(caps.images, "/images/generations")?;
        o.transcriptions = get(caps.audio_transcription, "/audio/transcriptions")?;
        o.translations = get(caps.audio_transcription, "/audio/translations")?;
        o.speech = get(caps.audio_speech, "/audio/speech")?;
        Ok(o)
    }
}

/// Weitere Endpunkte je Anbieterart (Embeddings, Bilder, Audio).
fn set_op_caps(kind: ProviderKind, c: &mut Capabilities) {
    let (emb, img, stt, tts) = match kind {
        ProviderKind::OpenAi | ProviderKind::AzureOpenAi => (true, true, true, true),
        // Generisch kompatibel (vLLM-, LocalAI-, LiteLLM-Server …).
        ProviderKind::OpenAiCompatible => (true, false, true, true),
        ProviderKind::Vllm => (true, false, true, false),
        ProviderKind::Ollama | ProviderKind::Mistral => (true, false, false, false),
        _ => (false, false, false, false),
    };
    c.embeddings = emb;
    // `n > 1` nativ (F12); generisch kompatible Server und Ollama ignorieren `n` teils still –
    // dort schickt der Proxy n parallele Anfragen.
    c.n_choices = matches!(
        kind,
        ProviderKind::OpenAi
            | ProviderKind::AzureOpenAi
            | ProviderKind::Vllm
            | ProviderKind::Mistral
    );
    c.images = img;
    c.audio_transcription = stt;
    c.audio_speech = tts;
}

fn caps(max_tokens_field: MaxTokensField, reasoning: bool) -> Capabilities {
    Capabilities {
        chat: true,
        streaming: true,
        tools: true,
        vision: true,
        json_schema: true,
        reasoning,
        embeddings: false,
        stream_usage: true,
        max_tokens_field,
        images: false,
        audio_transcription: false,
        audio_speech: false,
        n_choices: false,
    }
}

pub(crate) fn sensitive(value: &str) -> Option<HeaderValue> {
    let mut v = HeaderValue::from_str(value).ok()?;
    v.set_sensitive(true);
    Some(v)
}

pub(crate) fn bearer(token: &str) -> Option<HeaderValue> {
    sensitive(&format!("Bearer {token}"))
}

pub(crate) fn parse_uri(s: &Settings<'_>, url: &str) -> Result<Uri, ConfigError> {
    let uri: Uri = url
        .parse()
        .map_err(|e| s.invalid(format!("ungültige URL {url:?}: {e}")))?;
    match (uri.scheme_str(), uri.authority()) {
        (Some("http" | "https"), Some(_)) => Ok(uri),
        _ => Err(s.invalid(format!("URL {url:?} braucht http(s)://host"))),
    }
}

pub(crate) fn join(base: &str, path: &str) -> String {
    format!("{}{path}", base.trim_end_matches('/'))
}

/// Azure-Deployment-Namen landen im Pfad; nur unbedenkliche Zeichen zulassen.
fn valid_path_segment(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

impl OpenAiAdapter {
    /// Fabrik für die [`Registry`](crate::Registry).
    pub fn factory(dep: &DeploymentCtx) -> Result<Arc<dyn Provider>, ConfigError> {
        Ok(Arc::new(Self::from_deployment(dep)?))
    }

    /// Baut den Adapter aus einem Deployment (`openai`, `azure-openai`, `openai-compatible`).
    pub fn from_deployment(dep: &DeploymentCtx) -> Result<Self, ConfigError> {
        let s = Settings::of(dep);
        let spec = &dep.deployment.spec;
        let api_key = dep
            .credential
            .as_ref()
            .and_then(|c| c.get("apiKey").or_else(|| c.get("api_key")))
            .map(SecretString::expose);
        let header_err = || s.invalid("Zugangsdaten enthalten unzulässige Zeichen");

        let mut adapter = match spec.provider {
            ProviderKind::OpenAi => {
                let base = spec.endpoint.as_deref().unwrap_or(OPENAI_DEFAULT_ENDPOINT);
                let key = api_key.ok_or_else(|| s.missing("Zugangsdaten (apiKey)"))?;
                let mut headers = Vec::new();
                for (setting, header) in [
                    ("organization", "openai-organization"),
                    ("project", "openai-project"),
                ] {
                    if let Some(v) = s.str(setting)? {
                        let v = HeaderValue::from_str(v)
                            .map_err(|_| s.invalid(format!("settings.{setting} ungültig")))?;
                        headers.push((HeaderName::from_static(header), v));
                    }
                }
                let mut c = caps(MaxTokensField::MaxCompletionTokens, true);
                set_op_caps(ProviderKind::OpenAi, &mut c);
                let ops = Ops::build(&s, &c, |op| Ok(join(base, op)))?;
                Self {
                    kind: ProviderKind::OpenAi,
                    uri: parse_uri(&s, &join(base, "/chat/completions"))?,
                    provider_model: spec.provider_model.clone(),
                    auth: Auth::Header(AUTHORIZATION, bearer(key).ok_or_else(header_err)?),
                    headers,
                    caps: c,
                    default_max_tokens: None,
                    safe_prompt: None,
                    mistral_reasoning: crate::mistral::MistralReasoning::default(),
                    ops,
                }
            }
            ProviderKind::AzureOpenAi => {
                let endpoint = spec
                    .endpoint
                    .as_deref()
                    .ok_or_else(|| s.missing("endpoint (https://<ressource>.openai.azure.com)"))?;
                let auth = match s.str("auth")?.unwrap_or("apiKey") {
                    "apiKey" => {
                        let key = api_key.ok_or_else(|| s.missing("Zugangsdaten (apiKey)"))?;
                        Auth::Header(
                            HeaderName::from_static("api-key"),
                            sensitive(key).ok_or_else(header_err)?,
                        )
                    }
                    "entraId" => {
                        return Err(s.invalid(
                            "settings.auth = entraId ist noch nicht umgesetzt (TokenSource fehlt)",
                        ));
                    }
                    other => {
                        return Err(s.invalid(format!(
                            "settings.auth = {other:?} unbekannt (erlaubt: apiKey, entraId)"
                        )));
                    }
                };
                let (url, field) = azure_url(&s, endpoint, &spec.provider_model)?;
                let mut c = caps(field, true);
                set_op_caps(ProviderKind::AzureOpenAi, &mut c);
                let ops = Ops::build(&s, &c, |op| {
                    azure_op_url(&s, endpoint, &spec.provider_model, op).map(|(u, _)| u)
                })?;
                Self {
                    kind: ProviderKind::AzureOpenAi,
                    uri: parse_uri(&s, &url)?,
                    provider_model: spec.provider_model.clone(),
                    auth,
                    headers: Vec::new(),
                    caps: c,
                    default_max_tokens: None,
                    safe_prompt: None,
                    mistral_reasoning: crate::mistral::MistralReasoning::default(),
                    ops,
                }
            }
            other => {
                // OpenAI-kompatibel: generisch, Ollama, vLLM (Profile in `local`) und
                // Mistral (Profil in `mistral`).
                let default_endpoint = match other {
                    ProviderKind::Ollama => Some(crate::local::OLLAMA_DEFAULT_ENDPOINT),
                    ProviderKind::Mistral => Some(crate::mistral::MISTRAL_DEFAULT_ENDPOINT),
                    _ => None,
                };
                let base = spec
                    .endpoint
                    .as_deref()
                    .or(default_endpoint)
                    .ok_or_else(|| s.missing("endpoint (Basis-URL, z. B. http://host:8000/v1)"))?;
                if other == ProviderKind::Mistral && api_key.is_none() {
                    return Err(s.missing("Zugangsdaten (apiKey)"));
                }
                let auth = match api_key {
                    Some(k) => Auth::Header(AUTHORIZATION, bearer(k).ok_or_else(header_err)?),
                    None => Auth::None,
                };
                let mut c = caps(MaxTokensField::MaxTokens, false);
                match other {
                    // Reasoning-Modelle (Qwen3, DeepSeek-R1, gpt-oss …) liefern
                    // `reasoning`/`reasoning_content`; `reasoning_effort` kennen neuere
                    // Versionen.
                    ProviderKind::Ollama | ProviderKind::Vllm => c.reasoning = true,
                    // Mistral schickt die Usage immer im letzten Chunk und lehnt
                    // `stream_options` ab.
                    ProviderKind::Mistral => c.stream_usage = false,
                    _ => {}
                }
                set_op_caps(other, &mut c);
                let ops = Ops::build(&s, &c, |op| Ok(join(base, op)))?;
                Self {
                    kind: other,
                    uri: parse_uri(&s, &join(base, "/chat/completions"))?,
                    provider_model: spec.provider_model.clone(),
                    auth,
                    headers: Vec::new(),
                    caps: c,
                    default_max_tokens: None,
                    safe_prompt: None,
                    mistral_reasoning: crate::mistral::MistralReasoning::default(),
                    ops,
                }
            }
        };

        if let Some(f) = s.str("maxTokensField")? {
            adapter.caps.max_tokens_field = match f {
                "max_tokens" => MaxTokensField::MaxTokens,
                "max_completion_tokens" => MaxTokensField::MaxCompletionTokens,
                other => {
                    return Err(s.invalid(format!(
                        "settings.maxTokensField = {other:?} unbekannt (max_tokens, max_completion_tokens)"
                    )));
                }
            };
        }
        if let Some(b) = s.bool("streamUsage")? {
            adapter.caps.stream_usage = b;
        }
        adapter.default_max_tokens = s.positive_u64("defaultMaxTokens")?;
        if adapter.kind == ProviderKind::Mistral {
            adapter.safe_prompt = s.bool("safePrompt")?;
            if let Some(m) = s.str("reasoningMode")? {
                adapter.mistral_reasoning =
                    crate::mistral::MistralReasoning::parse(m).ok_or_else(|| {
                        s.invalid(format!(
                            "settings.reasoningMode = {m:?} unbekannt (prompt-mode, effort, off)"
                        ))
                    })?;
            }
        }
        Ok(adapter)
    }

    /// Azure OpenAI mit Bearer-Token aus einer [`TokenSource`] (Entra ID) statt `api-key`.
    pub fn azure_with_token_source(
        dep: &DeploymentCtx,
        tokens: Arc<dyn TokenSource>,
    ) -> Result<Self, ConfigError> {
        let s = Settings::of(dep);
        let spec = &dep.deployment.spec;
        let endpoint = spec
            .endpoint
            .as_deref()
            .ok_or_else(|| s.missing("endpoint"))?;
        let (url, field) = azure_url(&s, endpoint, &spec.provider_model)?;
        let mut c = caps(field, true);
        set_op_caps(ProviderKind::AzureOpenAi, &mut c);
        let ops = Ops::build(&s, &c, |op| {
            azure_op_url(&s, endpoint, &spec.provider_model, op).map(|(u, _)| u)
        })?;
        Ok(Self {
            kind: ProviderKind::AzureOpenAi,
            uri: parse_uri(&s, &url)?,
            provider_model: spec.provider_model.clone(),
            auth: Auth::Token(tokens),
            headers: Vec::new(),
            caps: c,
            default_max_tokens: Settings::of(dep).positive_u64("defaultMaxTokens")?,
            safe_prompt: None,
            mistral_reasoning: crate::mistral::MistralReasoning::default(),
            ops,
        })
    }

    /// Ziel-URL (für Tests und Diagnose).
    pub fn uri(&self) -> &Uri {
        &self.uri
    }
}

/// Azure-URL (Chat) und Ausgabegrenzen-Feld je nach `apiStyle`/`apiVersion`.
fn azure_url(
    s: &Settings<'_>,
    endpoint: &str,
    deployment: &str,
) -> Result<(String, MaxTokensField), ConfigError> {
    azure_op_url(s, endpoint, deployment, "/chat/completions")
}

/// Azure-URL einer Operation (`/chat/completions`, `/embeddings`, `/audio/speech` …).
fn azure_op_url(
    s: &Settings<'_>,
    endpoint: &str,
    deployment: &str,
    op: &str,
) -> Result<(String, MaxTokensField), ConfigError> {
    let version = s.str("apiVersion")?;
    if let Some(v) = version
        && !v
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
    {
        return Err(s.invalid(format!("settings.apiVersion {v:?} ungültig")));
    }
    match s.str("apiStyle")?.unwrap_or("deployment") {
        "deployment" => {
            if !valid_path_segment(deployment) {
                return Err(s.invalid(format!(
                    "providerModel {deployment:?} ist kein gültiger Azure-Deployment-Name"
                )));
            }
            let version = version.unwrap_or(AZURE_DEFAULT_API_VERSION);
            let field = if version.get(..10).unwrap_or(version) >= AZURE_MAX_COMPLETION_TOKENS_SINCE
            {
                MaxTokensField::MaxCompletionTokens
            } else {
                MaxTokensField::MaxTokens
            };
            Ok((
                join(
                    endpoint,
                    &format!("/openai/deployments/{deployment}{op}?api-version={version}"),
                ),
                field,
            ))
        }
        "v1" => {
            let mut url = join(endpoint, &format!("/openai/v1{op}"));
            if let Some(v) = version {
                url.push_str("?api-version=");
                url.push_str(v);
            }
            Ok((url, MaxTokensField::MaxCompletionTokens))
        }
        other => Err(s.invalid(format!(
            "settings.apiStyle = {other:?} unbekannt (erlaubt: deployment, v1)"
        ))),
    }
}

pub(crate) fn internal(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(ErrorKind::Internal, e.to_string())
}

pub(crate) const USER_AGENT_VALUE: &str = concat!("ai-gw/", env!("CARGO_PKG_VERSION"));

impl Provider for OpenAiAdapter {
    fn kind(&self) -> ProviderKind {
        self.kind
    }

    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    fn native_format(&self) -> WireFormat {
        WireFormat::OpenAiChat
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
        if stream {
            if self.caps.stream_usage {
                // Usage serverseitig erzwingen (§7); der Encoder entfernt den Usage-Chunk,
                // wenn der Client ihn nicht angefordert hat.
                req.stream_options
                    .get_or_insert_with(Default::default)
                    .include_usage = Some(true);
            }
        } else {
            // OpenAI lehnt `stream_options` ohne `stream: true` ab.
            req.stream_options = None;
        }
        if req.max_tokens.is_none() {
            req.max_tokens = self.default_max_tokens;
        }
        // Profilspezifische Felder vor der Übersetzung herausnehmen (sonst als Verlust notiert).
        let top_k = match self.kind {
            ProviderKind::Vllm => req.sampling.top_k.take(),
            _ => None,
        };
        let seed = match self.kind {
            ProviderKind::Mistral => req.sampling.seed.take(),
            _ => None,
        };
        let mut wire = openai::request_to_wire(req, losses)?;
        let max = wire.max_completion_tokens.take().or(wire.max_tokens.take());
        match self.caps.max_tokens_field {
            MaxTokensField::MaxCompletionTokens => wire.max_completion_tokens = max,
            MaxTokensField::MaxTokens => wire.max_tokens = max,
        }
        if let Some(k) = top_k {
            wire.extra.insert("top_k".into(), Value::from(k));
        }
        if self.kind == ProviderKind::Mistral {
            crate::mistral::adjust_request(
                &mut wire,
                seed,
                self.safe_prompt,
                self.mistral_reasoning,
                losses,
            );
        }
        let body = serde_json::to_vec(&wire).map_err(internal)?;
        self.post(
            &self.uri,
            HeaderValue::from_static("application/json"),
            if stream {
                "text/event-stream"
            } else {
                "application/json"
            },
            Bytes::from(body),
        )
    }

    fn parse_response(&self, body: &[u8]) -> Result<ChatResponse, ApiError> {
        if self.kind == ProviderKind::Mistral {
            // Magistral: `content` als Liste mit `thinking`-Blöcken (F61).
            let normalized = crate::mistral::normalize_response(body);
            let mut resp = openai::decode_response(normalized.as_deref().unwrap_or(body))?;
            crate::local::lift_reasoning(&mut resp);
            crate::mistral::adjust_response(&mut resp);
            return Ok(resp);
        }
        let mut resp = openai::decode_response(body)?;
        if matches!(self.kind, ProviderKind::Ollama | ProviderKind::Vllm) {
            crate::local::lift_reasoning(&mut resp);
        }
        Ok(resp)
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        let mut d = OpenAiSseDecoder::new();
        d.mistral = self.kind == ProviderKind::Mistral;
        Box::new(d)
    }

    fn classify_error(
        &self,
        status: StatusCode,
        headers: &HeaderMap,
        body: &[u8],
    ) -> UpstreamError {
        match self.kind {
            ProviderKind::Mistral => crate::mistral::classify_mistral_error(status, headers, body),
            _ => classify_openai_error(status, headers, body),
        }
    }

    fn build_embeddings(
        &self,
        mut req: EmbeddingsRequest,
        losses: &mut Losses,
    ) -> Result<Vec<UpstreamRequest>, ApiError> {
        let uri = self.op(self.ops.embeddings.as_ref(), Capability::Embeddings)?;
        req.model.clone_from(&self.provider_model);
        // OpenAI/Azure: Base64 (kleiner auf der Leitung); kompatible Server liefern Zahlen
        // (nicht jeder kennt `encoding_format`).
        let upstream_format = match self.kind {
            ProviderKind::OpenAi | ProviderKind::AzureOpenAi => Some(EncodingFormat::Base64),
            _ => None,
        };
        let mut wire = embeddings::embeddings_request_to_wire(req, upstream_format, losses);
        if self.kind == ProviderKind::Mistral {
            // Mistral: `output_dimension`, lehnt unbekannte Felder (`user`) ab.
            if let Some(d) = wire.dimensions.take() {
                wire.extra.insert("output_dimension".into(), Value::from(d));
            }
            if wire.user.take().is_some() {
                losses.note("mistral: user verworfen");
            }
        }
        let body = serde_json::to_vec(&wire).map_err(internal)?;
        Ok(vec![self.post(
            uri,
            HeaderValue::from_static("application/json"),
            "application/json",
            Bytes::from(body),
        )?])
    }

    fn parse_embeddings(
        &self,
        _headers: &HeaderMap,
        body: &[u8],
    ) -> Result<EmbeddingsResponse, ApiError> {
        let mut resp = embeddings::decode_embeddings_response(body)?;
        if resp.model.is_empty() {
            resp.model.clone_from(&self.provider_model);
        }
        Ok(resp)
    }

    fn build_images(
        &self,
        mut req: ImageRequest,
        losses: &mut Losses,
    ) -> Result<UpstreamRequest, ApiError> {
        let uri = self.op(self.ops.images.as_ref(), Capability::Images)?;
        req.model.clone_from(&self.provider_model);
        let wire = images::image_request_to_wire(req, losses);
        let body = serde_json::to_vec(&wire).map_err(internal)?;
        self.post(
            uri,
            HeaderValue::from_static("application/json"),
            "application/json",
            Bytes::from(body),
        )
    }

    fn parse_images(&self, body: &[u8]) -> Result<ImageResponse, ApiError> {
        images::decode_image_response(body)
    }

    fn build_transcription(
        &self,
        mut req: TranscriptionRequest,
        losses: &mut Losses,
    ) -> Result<UpstreamRequest, ApiError> {
        let ops = match req.task {
            AudioTask::Transcribe => self.ops.transcriptions.as_ref(),
            AudioTask::Translate => self.ops.translations.as_ref(),
        };
        let uri = self.op(ops, Capability::AudioTranscription)?;
        req.model.clone_from(&self.provider_model);
        let (content_type, body) = audio::transcription_form(&req, losses);
        let ct = HeaderValue::from_str(&content_type).map_err(internal)?;
        let accept = if req.format().is_plain_text() {
            "text/plain"
        } else {
            "application/json"
        };
        self.post(uri, ct, accept, body)
    }

    fn parse_transcription(
        &self,
        format: TranscriptFormat,
        body: &[u8],
    ) -> Result<TranscriptionResponse, ApiError> {
        audio::decode_transcription_response(body, format)
    }

    fn build_speech(
        &self,
        mut req: SpeechRequest,
        losses: &mut Losses,
    ) -> Result<UpstreamRequest, ApiError> {
        let uri = self.op(self.ops.speech.as_ref(), Capability::AudioSpeech)?;
        req.model.clone_from(&self.provider_model);
        let accept = req.media_type();
        let wire = audio::speech_request_to_wire(req, losses);
        let body = serde_json::to_vec(&wire).map_err(internal)?;
        self.post(
            uri,
            HeaderValue::from_static("application/json"),
            accept,
            Bytes::from(body),
        )
    }
}

impl OpenAiAdapter {
    /// URL einer weiteren Operation oder „nicht unterstützt“.
    fn op<'a>(&self, uri: Option<&'a Uri>, cap: Capability) -> Result<&'a Uri, ApiError> {
        uri.ok_or_else(|| crate::unsupported(self.kind, cap))
    }

    /// POST mit Auth- und Zusatz-Headern.
    fn post(
        &self,
        uri: &Uri,
        content_type: HeaderValue,
        accept: &'static str,
        body: Bytes,
    ) -> Result<UpstreamRequest, ApiError> {
        let mut builder = http::Request::builder()
            .method(Method::POST)
            .uri(uri.clone())
            .header(CONTENT_TYPE, content_type)
            .header(ACCEPT, HeaderValue::from_static(accept))
            .header(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
        match &self.auth {
            Auth::None => {}
            Auth::Header(name, value) => builder = builder.header(name, value.clone()),
            Auth::Token(src) => {
                let token = src.bearer_token()?;
                let value = bearer(token.expose())
                    .ok_or_else(|| internal("Token der TokenSource enthält unzulässige Zeichen"))?;
                builder = builder.header(AUTHORIZATION, value);
            }
        }
        for (name, value) in &self.headers {
            builder = builder.header(name, value.clone());
        }
        builder.body(body).map_err(internal)
    }
}

// ---------------------------------------------------------------------------------------------
// Fehlerklassifikation
// ---------------------------------------------------------------------------------------------

/// Felder eines OpenAI-/Azure-Fehlerobjekts.
#[derive(Debug, Default)]
struct WireError {
    message: String,
    kind: Option<String>,
    code: Option<String>,
    param: Option<String>,
    inner_code: Option<String>,
}

fn str_or_num(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

pub(crate) fn truncate(mut s: String) -> String {
    if s.chars().count() > MAX_ERROR_MESSAGE {
        s = s.chars().take(MAX_ERROR_MESSAGE).collect();
        s.push('…');
    }
    s
}

fn parse_wire_error(body: &[u8]) -> Option<WireError> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let e = v.get("error")?;
    if let Value::String(m) = e {
        return Some(WireError {
            message: m.clone(),
            ..WireError::default()
        });
    }
    let e = e.as_object()?;
    Some(WireError {
        message: e
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        kind: str_or_num(e.get("type")),
        code: str_or_num(e.get("code")),
        param: str_or_num(e.get("param")),
        inner_code: e.get("innererror").and_then(|i| str_or_num(i.get("code"))),
    })
}

/// `Retry-After` (Sekunden) bzw. `retry-after-ms`.
pub(crate) fn retry_after(headers: &HeaderMap) -> Option<std::time::Duration> {
    let num = |name: &str| {
        headers
            .get(name)?
            .to_str()
            .ok()?
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite() && *v >= 0.0)
    };
    if let Some(ms) = num("retry-after-ms") {
        return Some(std::time::Duration::from_secs_f64(ms / 1000.0));
    }
    num("retry-after").map(std::time::Duration::from_secs_f64)
}

fn is_context_length(e: &WireError) -> bool {
    if e.code.as_deref() == Some("context_length_exceeded") {
        return true;
    }
    let m = e.message.to_ascii_lowercase();
    m.contains("maximum context length")
        || m.contains("context length")
        || m.contains("context_length_exceeded")
        || m.contains("too many tokens")
}

fn is_content_filter(e: &WireError) -> bool {
    matches!(
        e.code.as_deref(),
        Some("content_filter" | "content_policy_violation")
    ) || e.inner_code.as_deref() == Some("ResponsibleAIPolicyViolation")
}

/// Klassifiziert eine Fehlerantwort der OpenAI-Familie.
///
/// Zugangsfehler des Gateways gegenüber dem Anbieter (401/403) werden **nicht** als 401 an
/// den Client gereicht (dessen Schlüssel ist gültig), sondern als 502 ohne die Meldung des
/// Anbieters (sie kann Teile des Anbieter-Schlüssels enthalten).
pub fn classify_openai_error(
    status: StatusCode,
    headers: &HeaderMap,
    body: &[u8],
) -> UpstreamError {
    let s = status.as_u16();
    let wire = parse_wire_error(body).unwrap_or_else(|| WireError {
        message: String::from_utf8_lossy(body).trim().to_owned(),
        ..WireError::default()
    });
    let quota = wire.code.as_deref() == Some("insufficient_quota")
        || wire.kind.as_deref() == Some("insufficient_quota");
    let class = match s {
        429 if quota => ErrorClass::Quota,
        429 => ErrorClass::RateLimit,
        400 | 404 | 413 | 422 if is_context_length(&wire) => ErrorClass::ContextLength,
        400..=499 if is_content_filter(&wire) => ErrorClass::ContentFilter,
        401 | 403 => ErrorClass::Auth,
        404 => ErrorClass::NotFound,
        408 => ErrorClass::Timeout,
        409 => ErrorClass::ServerError,
        400..=499 => ErrorClass::InvalidRequest,
        503 | 529 => ErrorClass::Overloaded,
        504 => ErrorClass::Timeout,
        500..=599 => ErrorClass::ServerError,
        _ => ErrorClass::Protocol,
    };
    let message = if wire.message.is_empty() {
        format!("Anbieter meldet HTTP {s}")
    } else {
        truncate(wire.message)
    };
    let mut api = ApiError {
        kind: ErrorKind::from_status(s),
        message,
        param: wire.param,
        code: wire.code,
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
            api.message = format!("Anbieter kennt Modell/Deployment nicht: {}", api.message);
            api.code = Some("upstream_not_found".to_owned());
        }
        ErrorClass::ContextLength => {
            api.kind = ErrorKind::InvalidRequest;
            api.code
                .get_or_insert_with(|| "context_length_exceeded".to_owned());
        }
        ErrorClass::ContentFilter => {
            api.kind = ErrorKind::InvalidRequest;
            api.code.get_or_insert_with(|| "content_filter".to_owned());
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

/// SSE (`data: {chunk}` … `data: [DONE]`) → IR-Ereignisse.
#[derive(Debug, Default)]
pub struct OpenAiSseDecoder {
    sse: SseParser,
    inner: openai::StreamDecoder,
    events: Vec<SseEvent>,
    stopped: bool,
    done: bool,
    /// Mistral-Endgründe (`model_length`) abbilden.
    mistral: bool,
}

impl OpenAiSseDecoder {
    /// Neuer Decoder.
    pub fn new() -> Self {
        Self::default()
    }

    fn data(&mut self, data: &str, out: &mut Vec<StreamEvent>) {
        if self.done {
            return;
        }
        if data.trim() == "[DONE]" {
            self.done = true;
            return;
        }
        if data.contains("\"error\"")
            && let Some(err) = stream_error(data)
        {
            out.push(StreamEvent::Error(err));
            self.done = true;
            return;
        }
        // Magistral: `delta.content` als Liste mit `thinking`-Blöcken (F61).
        let normalized = if self.mistral
            && (data.contains("\"content\":[") || data.contains("\"content\": ["))
        {
            crate::mistral::normalize_chunk(data)
        } else {
            None
        };
        match self.inner.decode(normalized.as_deref().unwrap_or(data)) {
            Ok(events) => {
                for mut ev in events {
                    if let StreamEvent::Stop { reason, .. } = &mut ev {
                        self.stopped = true;
                        if self.mistral {
                            crate::mistral::map_stop(reason);
                        }
                    }
                    out.push(ev);
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

/// Fehlerobjekt mitten im Stream (`data: {"error":{…}}`), sonst `None`.
fn stream_error(data: &str) -> Option<ApiError> {
    let v: Value = serde_json::from_str(data).ok()?;
    let obj = v.as_object()?;
    if obj.contains_key("choices") {
        return None;
    }
    let w = parse_wire_error(data.as_bytes())?;
    let kind = match w.kind.as_deref().or(w.code.as_deref()) {
        Some("rate_limit_error" | "rate_limit_exceeded") => ErrorKind::RateLimit,
        Some("invalid_request_error") => ErrorKind::InvalidRequest,
        Some("timeout" | "timeout_error") => ErrorKind::Timeout,
        Some("overloaded" | "overloaded_error") => ErrorKind::Overloaded,
        _ => ErrorKind::BadGateway,
    };
    let message = if w.message.is_empty() {
        "Anbieter meldet einen Fehler im Stream".to_owned()
    } else {
        truncate(w.message)
    };
    Some(ApiError {
        kind,
        message,
        param: w.param,
        code: w.code,
        exact_status: None,
    })
}

impl StreamDecoder for OpenAiSseDecoder {
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
        // Manche kompatiblen Server lassen `[DONE]` weg; ein Endgrund genügt dann.
        if !self.stopped {
            out.push(StreamEvent::Error(
                ApiError::new(
                    ErrorKind::BadGateway,
                    "Anbieter-Stream endete unerwartet (ohne Abschluss)",
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

    use aigw_core::ir::{Message, Role, StopReason, StreamOptions};
    use aigw_core::resource::{
        CredentialData, Deployment, DeploymentSpec, JsonObject, ObjectMeta, new_id,
    };
    use serde_json::json;

    use super::*;
    use crate::auth::StaticToken;

    fn dep(
        provider: ProviderKind,
        endpoint: Option<&str>,
        model: &str,
        key: Option<&str>,
        settings: Value,
    ) -> DeploymentCtx {
        let settings: JsonObject = match settings {
            Value::Object(m) => m,
            _ => JsonObject::new(),
        };
        DeploymentCtx {
            deployment: Deployment {
                id: new_id(),
                metadata: ObjectMeta::named("dep-1"),
                spec: DeploymentSpec {
                    model_group: "g".into(),
                    provider,
                    endpoint: endpoint.map(str::to_owned),
                    provider_model: model.into(),
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

    fn request(stream: bool) -> ChatRequest {
        ChatRequest {
            model: "alias".into(),
            messages: vec![Message::text(Role::User, "hi")],
            max_tokens: Some(42),
            stream: Some(stream),
            source: Some(WireFormat::OpenAiChat),
            ..ChatRequest::default()
        }
    }

    fn body_json(req: &UpstreamRequest) -> Value {
        serde_json::from_slice(req.body()).unwrap()
    }

    #[test]
    fn openai_request_uses_bearer_and_max_completion_tokens() {
        let a = OpenAiAdapter::from_deployment(&dep(
            ProviderKind::OpenAi,
            None,
            "gpt-4o",
            Some("sk-test"),
            json!({"organization": "org-1"}),
        ))
        .unwrap();
        let mut req = request(true);
        req.hints.legacy_max_tokens = true;
        let r = a.build_request(req, &mut Losses::new()).unwrap();
        assert_eq!(r.uri(), "https://api.openai.com/v1/chat/completions");
        assert_eq!(r.headers()["authorization"], "Bearer sk-test");
        assert!(r.headers()["authorization"].is_sensitive());
        assert_eq!(r.headers()["openai-organization"], "org-1");
        assert_eq!(r.headers()["accept"], "text/event-stream");
        let b = body_json(&r);
        assert_eq!(b["model"], "gpt-4o");
        assert_eq!(b["max_completion_tokens"], 42);
        assert!(b.get("max_tokens").is_none());
        assert_eq!(b["stream_options"]["include_usage"], true);
    }

    #[test]
    fn non_stream_drops_stream_options() {
        let a = OpenAiAdapter::from_deployment(&dep(
            ProviderKind::OpenAi,
            Some("http://127.0.0.1:9/v1/"),
            "m",
            Some("k"),
            json!({}),
        ))
        .unwrap();
        let mut req = request(false);
        req.stream_options = Some(StreamOptions {
            include_usage: Some(true),
            ..StreamOptions::default()
        });
        let r = a.build_request(req, &mut Losses::new()).unwrap();
        assert_eq!(r.uri(), "http://127.0.0.1:9/v1/chat/completions");
        assert!(body_json(&r).get("stream_options").is_none());
    }

    #[test]
    fn azure_deployment_url_api_key_and_token_field() {
        let a = OpenAiAdapter::from_deployment(&dep(
            ProviderKind::AzureOpenAi,
            Some("https://res.openai.azure.com/"),
            "gpt4o-prod",
            Some("az-key"),
            json!({}),
        ))
        .unwrap();
        let r = a.build_request(request(false), &mut Losses::new()).unwrap();
        assert_eq!(
            r.uri(),
            "https://res.openai.azure.com/openai/deployments/gpt4o-prod/chat/completions?api-version=2024-10-21"
        );
        assert_eq!(r.headers()["api-key"], "az-key");
        assert!(r.headers().get("authorization").is_none());
        assert_eq!(body_json(&r)["max_completion_tokens"], 42);

        // Alte API-Version → max_tokens.
        let a = OpenAiAdapter::from_deployment(&dep(
            ProviderKind::AzureOpenAi,
            Some("https://res.openai.azure.com"),
            "d",
            Some("k"),
            json!({"apiVersion": "2024-06-01"}),
        ))
        .unwrap();
        let r = a.build_request(request(false), &mut Losses::new()).unwrap();
        let b = body_json(&r);
        assert_eq!(b["max_tokens"], 42);
        assert!(b.get("max_completion_tokens").is_none());

        // v1-API.
        let a = OpenAiAdapter::from_deployment(&dep(
            ProviderKind::AzureOpenAi,
            Some("https://res.openai.azure.com"),
            "gpt-4o",
            Some("k"),
            json!({"apiStyle": "v1"}),
        ))
        .unwrap();
        assert_eq!(
            a.uri(),
            "https://res.openai.azure.com/openai/v1/chat/completions"
        );
    }

    #[test]
    fn azure_entra_token_source() {
        let d = dep(
            ProviderKind::AzureOpenAi,
            Some("https://res.openai.azure.com"),
            "d",
            None,
            json!({"auth": "entraId"}),
        );
        assert!(OpenAiAdapter::from_deployment(&d).is_err());
        let a = OpenAiAdapter::azure_with_token_source(
            &d,
            Arc::new(StaticToken(SecretString::new("eyJ"))),
        )
        .unwrap();
        let r = a.build_request(request(false), &mut Losses::new()).unwrap();
        assert_eq!(r.headers()["authorization"], "Bearer eyJ");
    }

    #[test]
    fn azure_rejects_bad_deployment_names() {
        let d = dep(
            ProviderKind::AzureOpenAi,
            Some("https://res.openai.azure.com"),
            "../x",
            Some("k"),
            json!({}),
        );
        assert!(OpenAiAdapter::from_deployment(&d).is_err());
    }

    #[test]
    fn compatible_without_key_uses_max_tokens_and_overrides() {
        let a = OpenAiAdapter::from_deployment(&dep(
            ProviderKind::OpenAiCompatible,
            Some("http://vllm:8000/v1"),
            "llama",
            None,
            json!({"streamUsage": false}),
        ))
        .unwrap();
        let r = a.build_request(request(true), &mut Losses::new()).unwrap();
        assert!(r.headers().get("authorization").is_none());
        let b = body_json(&r);
        assert_eq!(b["max_tokens"], 42);
        assert!(b.get("stream_options").is_none());

        let a = OpenAiAdapter::from_deployment(&dep(
            ProviderKind::OpenAiCompatible,
            Some("http://vllm:8000/v1"),
            "llama",
            Some("tok"),
            json!({"maxTokensField": "max_completion_tokens"}),
        ))
        .unwrap();
        let r = a.build_request(request(false), &mut Losses::new()).unwrap();
        assert_eq!(r.headers()["authorization"], "Bearer tok");
        assert_eq!(body_json(&r)["max_completion_tokens"], 42);

        assert!(
            OpenAiAdapter::from_deployment(&dep(
                ProviderKind::OpenAiCompatible,
                None,
                "x",
                None,
                json!({})
            ))
            .is_err()
        );
        assert!(
            OpenAiAdapter::from_deployment(&dep(
                ProviderKind::OpenAiCompatible,
                Some("ftp://x"),
                "x",
                None,
                json!({})
            ))
            .is_err()
        );
    }

    #[test]
    fn ollama_vllm_and_mistral_profiles() {
        // Ollama: Standard-Endpunkt, ohne Schlüssel, Reasoning-Fähigkeit.
        let a = OpenAiAdapter::from_deployment(&dep(
            ProviderKind::Ollama,
            None,
            "qwen3",
            None,
            json!({"defaultMaxTokens": 256}),
        ))
        .unwrap();
        assert_eq!(a.uri(), "http://localhost:11434/v1/chat/completions");
        assert!(a.capabilities().reasoning && a.capabilities().stream_usage);
        let mut req = request(true);
        req.max_tokens = None;
        req.sampling.top_k = Some(5);
        let b = body_json(&a.build_request(req, &mut Losses::new()).unwrap());
        assert_eq!(b["max_tokens"], 256);
        assert!(b.get("top_k").is_none());
        assert_eq!(b["stream_options"]["include_usage"], true);

        // vLLM: Endpunkt Pflicht, top_k wird weitergegeben.
        assert!(
            OpenAiAdapter::from_deployment(&dep(ProviderKind::Vllm, None, "m", None, json!({})))
                .is_err()
        );
        let a = OpenAiAdapter::from_deployment(&dep(
            ProviderKind::Vllm,
            Some("http://vllm:8000/v1"),
            "m",
            None,
            json!({}),
        ))
        .unwrap();
        let mut req = request(false);
        req.sampling.top_k = Some(5);
        let mut losses = Losses::new();
        let b = body_json(&a.build_request(req, &mut losses).unwrap());
        assert_eq!(b["top_k"], 5);
        assert_eq!(b["max_tokens"], 42);
        assert!(losses.is_empty(), "{losses:?}");

        // Mistral: Schlüssel Pflicht, random_seed, any, keine stream_options, Tool-IDs.
        assert!(
            OpenAiAdapter::from_deployment(&dep(ProviderKind::Mistral, None, "m", None, json!({})))
                .is_err()
        );
        let a = OpenAiAdapter::from_deployment(&dep(
            ProviderKind::Mistral,
            None,
            "mistral-large-latest",
            Some("mk"),
            json!({"safePrompt": true}),
        ))
        .unwrap();
        assert_eq!(a.uri(), "https://api.mistral.ai/v1/chat/completions");
        let mut req = request(true);
        req.sampling.seed = Some(7);
        req.user = Some("u1".into());
        req.tool_choice = Some(aigw_core::ir::ToolChoice::Required);
        req.messages.push(Message {
            role: Role::Assistant,
            content: aigw_core::ir::Content::Parts(vec![aigw_core::ir::Part::new(
                aigw_core::ir::PartKind::ToolCall {
                    id: "toolu_01XYZ".into(),
                    name: "f".into(),
                    arguments: "{}".into(),
                },
            )]),
            name: None,
            extra: Default::default(),
        });
        req.messages.push(Message {
            role: Role::Tool,
            content: aigw_core::ir::Content::Parts(vec![aigw_core::ir::Part::new(
                aigw_core::ir::PartKind::ToolResult {
                    tool_call_id: "toolu_01XYZ".into(),
                    content: aigw_core::ir::Content::Text("ok".into()),
                    is_error: None,
                },
            )]),
            name: None,
            extra: Default::default(),
        });
        let r = a.build_request(req, &mut Losses::new()).unwrap();
        assert_eq!(r.headers()["authorization"], "Bearer mk");
        let b = body_json(&r);
        assert_eq!(b["random_seed"], 7);
        assert!(b.get("seed").is_none() && b.get("user").is_none());
        assert!(b.get("stream_options").is_none());
        assert_eq!(b["tool_choice"], "any");
        assert_eq!(b["safe_prompt"], true);
        assert_eq!(b["max_tokens"], 42);
        let id = b["messages"][1]["tool_calls"][0]["id"].as_str().unwrap();
        assert_eq!(id.len(), 9);
        assert_eq!(b["messages"][2]["tool_call_id"], id);

        // Endgrund model_length → max_tokens (Antwort und Stream).
        let resp = a
            .parse_response(
                json!({"id": "x", "object": "chat.completion", "created": 1, "model": "m",
                       "choices": [{"index": 0, "message": {"role": "assistant", "content": "a"},
                                    "finish_reason": "model_length"}]})
                .to_string()
                .as_bytes(),
            )
            .unwrap();
        assert_eq!(resp.choices[0].stop_reason, Some(StopReason::MaxTokens));
        let mut d = a.stream_decoder();
        let mut out = Vec::new();
        d.push(
            b"data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"model_length\"}]}\n\n",
            &mut out,
        );
        assert!(out.iter().any(|e| matches!(
            e,
            StreamEvent::Stop {
                reason: StopReason::MaxTokens,
                ..
            }
        )));
    }

    fn classify(status: u16, headers: &[(&str, &str)], body: Value) -> UpstreamError {
        let mut h = HeaderMap::new();
        for (k, v) in headers {
            h.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        classify_openai_error(
            StatusCode::from_u16(status).unwrap(),
            &h,
            body.to_string().as_bytes(),
        )
    }

    #[test]
    fn classifies_errors() {
        let e = classify(
            429,
            &[("retry-after", "2")],
            json!({"error": {"message": "slow down", "type": "requests", "code": "rate_limit_exceeded"}}),
        );
        assert_eq!(e.class, ErrorClass::RateLimit);
        assert!(e.retryable);
        assert_eq!(e.retry_after, Some(std::time::Duration::from_secs(2)));
        assert_eq!(e.error.kind, ErrorKind::RateLimit);

        let e = classify(
            429,
            &[("retry-after-ms", "1500")],
            json!({"error": {"message": "quota", "type": "insufficient_quota", "code": "insufficient_quota"}}),
        );
        assert_eq!(e.class, ErrorClass::Quota);
        assert!(!e.retryable);
        assert_eq!(e.retry_after, Some(std::time::Duration::from_millis(1500)));

        let e = classify(
            400,
            &[],
            json!({"error": {"message": "This model's maximum context length is 8192 tokens", "code": "context_length_exceeded", "param": "messages"}}),
        );
        assert_eq!(e.class, ErrorClass::ContextLength);
        assert!(!e.retryable);
        assert_eq!(e.error.code.as_deref(), Some("context_length_exceeded"));
        assert_eq!(e.error.param.as_deref(), Some("messages"));

        let e = classify(
            400,
            &[],
            json!({"error": {"message": "filtered", "code": "content_filter", "innererror": {"code": "ResponsibleAIPolicyViolation"}}}),
        );
        assert_eq!(e.class, ErrorClass::ContentFilter);

        let e = classify(
            401,
            &[],
            json!({"error": {"message": "Incorrect API key provided: sk-abc***xyz"}}),
        );
        assert_eq!(e.class, ErrorClass::Auth);
        assert_eq!(e.error.kind, ErrorKind::BadGateway);
        assert!(!e.error.message.contains("sk-abc"));

        let e = classify(500, &[], json!({"error": {"message": "boom"}}));
        assert_eq!(e.class, ErrorClass::ServerError);
        assert!(e.retryable);
        assert_eq!(e.upstream_status, Some(500));

        let e = classify(503, &[], json!("x"));
        assert_eq!(e.class, ErrorClass::Overloaded);
        let e = classify(
            404,
            &[],
            json!({"error": {"message": "DeploymentNotFound"}}),
        );
        assert_eq!(e.class, ErrorClass::NotFound);
        let e = classify(400, &[], json!({"error": {"message": "bad"}}));
        assert_eq!(e.class, ErrorClass::InvalidRequest);
        assert_eq!(e.error.message, "bad");
    }

    #[test]
    fn decoder_handles_split_chunks_usage_and_done() {
        let mut d = OpenAiSseDecoder::new();
        let input = concat!(
            "data: {\"id\":\"\",\"choices\":[],\"created\":0,\"model\":\"\",\"object\":\"\",\"prompt_filter_results\":[]}\n\n",
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hä\"}}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1,\"total_tokens\":4}}\n\n",
            "data: [DONE]\n\n"
        );
        let mut out = Vec::new();
        for b in input.as_bytes().chunks(5) {
            d.push(b, &mut out);
        }
        d.finish(&mut out);
        assert!(d.is_done());
        assert!(matches!(&out[0], StreamEvent::Start { id, .. } if id == "c1"));
        assert!(matches!(&out[1], StreamEvent::TextDelta { text, .. } if text == "Hä"));
        assert!(matches!(
            &out[2],
            StreamEvent::Stop {
                reason: StopReason::EndTurn,
                ..
            }
        ));
        assert!(
            matches!(&out[3], StreamEvent::Usage(u) if u.input_tokens == 3 && u.output_tokens == 1)
        );
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn decoder_reports_errors_and_truncation() {
        let mut d = OpenAiSseDecoder::new();
        let mut out = Vec::new();
        d.push(
            b"data: {\"error\":{\"message\":\"overloaded\",\"type\":\"server_error\"}}\n\n",
            &mut out,
        );
        assert!(
            matches!(&out[0], StreamEvent::Error(e) if e.kind == ErrorKind::BadGateway && e.message == "overloaded")
        );
        assert!(d.is_done());

        // Inhalt mit dem Wort "error" ist kein Fehler.
        let mut d = OpenAiSseDecoder::new();
        let mut out = Vec::new();
        d.push(
            b"data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"error\"}}]}\n\n",
            &mut out,
        );
        assert!(matches!(&out[1], StreamEvent::TextDelta { text, .. } if text == "error"));
        d.finish(&mut out);
        assert!(
            matches!(out.last(), Some(StreamEvent::Error(e)) if e.code.as_deref() == Some("upstream_stream_truncated"))
        );

        let mut d = OpenAiSseDecoder::new();
        let mut out = Vec::new();
        d.push(b"data: {nope\n\n", &mut out);
        assert!(
            matches!(&out[0], StreamEvent::Error(e) if e.code.as_deref() == Some("upstream_protocol_error"))
        );
    }

    #[test]
    fn mistral_stream_thinking_blocks() {
        let mut d = OpenAiSseDecoder::new();
        d.mistral = true;
        let mut out = Vec::new();
        let chunks = [
            r#"{"id":"c","model":"magistral","created":1,"choices":[{"index":0,"delta":{"role":"assistant","content":[{"type":"thinking","thinking":[{"type":"text","text":"erst denken"}]}]}}]}"#,
            r#"{"id":"c","model":"magistral","created":1,"choices":[{"index":0,"delta":{"content":"42"},"finish_reason":"stop"}]}"#,
        ];
        for c in chunks {
            d.push(format!("data: {c}\n\n").as_bytes(), &mut out);
        }
        assert!(
            matches!(&out[1], StreamEvent::ReasoningDelta { text, .. } if text == "erst denken")
        );
        assert!(matches!(&out[2], StreamEvent::TextDelta { text, .. } if text == "42"));
    }
}
