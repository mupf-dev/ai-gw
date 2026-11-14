// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Anbieter-Adapter von ai-gw (Architektur §5, AIGW-15) und Upstream-Ausführung (§7, AIGW-14).
//!
//! - [`Provider`] – ein Adapter übersetzt IR ↔ Anbieterformat: Request bauen, Antwort parsen,
//!   Stream inkrementell dekodieren, Fehler klassifizieren. Eine Adapter-Instanz ist an **ein**
//!   Deployment gebunden (URL, Zugangsdaten, Einstellungen sind beim Snapshot-Aufbau
//!   vorberechnet, nicht pro Anfrage).
//! - [`Registry`] – Fabriken je [`ProviderKind`], baut aus einem
//!   [`DeploymentCtx`](aigw_core::snapshot::DeploymentCtx) den passenden Adapter.
//! - [`UpstreamClient`] – gemeinsamer HTTP-Client (hyper, rustls, HTTP/2 + Keep-Alive,
//!   Verbindungspool je Endpunkt).
//! - [`Upstream`] – Deployment = Adapter + Client + [`Timeouts`]; führt Anfragen aus und
//!   liefert IR-Antworten bzw. einen IR-Ereignisstrom mit Backpressure.
//!
//! Adapter: [`openai::OpenAiAdapter`] für OpenAI, Azure OpenAI, generisch OpenAI-kompatible
//! Endpunkte und die Profile Ollama, vLLM ([`local`]) und Mistral ([`mistral`]);
//! [`anthropic::AnthropicAdapter`] für die Anthropic Messages API;
//! [`bedrock::BedrockAdapter`] für AWS Bedrock (Converse, SigV4) und
//! [`vertex::VertexAdapter`] für Google Gemini (Gemini API und Vertex AI).

pub mod anthropic;
pub mod auth;
pub mod bedrock;
mod client;
mod codec;
pub mod entra;
mod errmap;
pub mod local;
pub mod mistral;
pub mod openai;
mod settings;
mod upstream;
pub mod vertex;

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use aigw_core::ir::audio::{
    SpeechRequest, TranscriptFormat, TranscriptionRequest, TranscriptionResponse,
};
use aigw_core::ir::embeddings::{EmbeddingsRequest, EmbeddingsResponse};
use aigw_core::ir::images::{ImageRequest, ImageResponse};
use aigw_core::ir::{ApiError, ChatRequest, ChatResponse, StreamEvent, UpstreamError};
use aigw_core::snapshot::DeploymentCtx;
use aigw_core::types::ProviderKind;
use aigw_core::wire::{Losses, WireFormat};
use bytes::Bytes;
use http::{HeaderMap, StatusCode};

pub use client::{ClientConfig, UpstreamClient};
pub use settings::{GROUP_INHERITED_SETTINGS, with_credential_auth, with_group_defaults};
pub use upstream::{
    ByteStream, EventStream, SpeechStream, Timeouts, Upstream, UpstreamInfo, default_propagation,
};

/// Fertig gebaute Upstream-Anfrage (Body vollständig im Speicher).
pub type UpstreamRequest = http::Request<Bytes>;

/// Asynchrone Vorbereitung eines Adapters vor [`Provider::build_request`] (siehe
/// [`Provider::prepare`]).
pub type PrepareFuture<'a> = Pin<Box<dyn Future<Output = Result<(), UpstreamError>> + Send + 'a>>;

/// Welches Feld die Obergrenze der Ausgabe-Tokens trägt (OpenAI-Familie).
///
/// OpenAI verlangt für Reasoning-Modelle `max_completion_tokens` und lehnt dort `max_tokens`
/// ab; viele kompatible Server (ältere vLLM-/Ollama-Versionen, ältere Azure-API-Versionen)
/// kennen nur `max_tokens`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxTokensField {
    /// `max_completion_tokens`.
    MaxCompletionTokens,
    /// `max_tokens` (veraltet bei OpenAI).
    MaxTokens,
}

/// Fähigkeiten eines Adapters bzw. Deployments.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Capabilities {
    /// Chat-Anfragen.
    pub chat: bool,
    /// Gestreamte Antworten.
    pub streaming: bool,
    /// Tools/Funktionsaufrufe.
    pub tools: bool,
    /// Bildeingabe.
    pub vision: bool,
    /// Strukturierte Ausgabe per JSON-Schema.
    pub json_schema: bool,
    /// Reasoning-Steuerung (`reasoning_effort`).
    pub reasoning: bool,
    /// Embeddings (`/v1/embeddings`, AIGW-21).
    pub embeddings: bool,
    /// Usage im Stream anforderbar (`stream_options.include_usage`).
    pub stream_usage: bool,
    /// Feld für die Obergrenze der Ausgabe-Tokens.
    pub max_tokens_field: MaxTokensField,
    /// Bilderzeugung (`/v1/images/generations`, AIGW-21).
    pub images: bool,
    /// Transkription/Übersetzung (`/v1/audio/transcriptions|translations`, AIGW-21).
    pub audio_transcription: bool,
    /// Sprachausgabe (`/v1/audio/speech`, AIGW-21).
    pub audio_speech: bool,
    /// Mehrere Antwortvarianten (`n > 1`) in einer Anfrage (F12). Sonst schickt der Proxy
    /// `n` parallele Anfragen und führt sie zusammen.
    pub n_choices: bool,
}

impl Capabilities {
    /// Kann der Adapter Anfragen dieser Art ausführen?
    pub fn supports(&self, cap: Capability) -> bool {
        match cap {
            Capability::Chat => self.chat,
            Capability::Embeddings => self.embeddings,
            Capability::Images => self.images,
            Capability::AudioTranscription => self.audio_transcription,
            Capability::AudioSpeech => self.audio_speech,
        }
    }
}

/// Art einer Anfrage bzw. Fähigkeit eines Deployments/einer Modellgruppe (AIGW-21).
///
/// Die Responses API (`/v1/responses`) läuft über die Chat-IR und braucht [`Capability::Chat`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    /// Chat Completions, Messages, Responses.
    Chat,
    /// Embeddings.
    Embeddings,
    /// Bilderzeugung.
    Images,
    /// Transkription und Übersetzung.
    AudioTranscription,
    /// Sprachausgabe.
    AudioSpeech,
}

impl Capability {
    /// Alle Fähigkeiten.
    pub const ALL: [Self; 5] = [
        Self::Chat,
        Self::Embeddings,
        Self::Images,
        Self::AudioTranscription,
        Self::AudioSpeech,
    ];

    /// Bezeichner in der Konfiguration (`settings.capabilities`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Embeddings => "embeddings",
            Self::Images => "images",
            Self::AudioTranscription => "audio-transcription",
            Self::AudioSpeech => "audio-speech",
        }
    }

    /// Aus dem Bezeichner.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == s)
    }

    fn bit(self) -> u8 {
        match self {
            Self::Chat => 1,
            Self::Embeddings => 2,
            Self::Images => 4,
            Self::AudioTranscription => 8,
            Self::AudioSpeech => 16,
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Menge von [`Capability`]s (Bitmaske, `Copy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CapabilitySet(u8);

impl CapabilitySet {
    /// Leer.
    pub const EMPTY: Self = Self(0);
    /// Nur Chat (Standard einer Modellgruppe ohne `settings.capabilities`).
    pub const CHAT: Self = Self(1);
    /// Alle Fähigkeiten.
    pub const ALL: Self = Self(31);

    /// Enthält `cap`?
    pub fn contains(self, cap: Capability) -> bool {
        self.0 & cap.bit() != 0
    }

    /// Fügt `cap` hinzu.
    #[must_use]
    pub fn with(self, cap: Capability) -> Self {
        Self(self.0 | cap.bit())
    }

    /// Enthaltene Fähigkeiten in fester Reihenfolge.
    pub fn iter(self) -> impl Iterator<Item = Capability> {
        Capability::ALL
            .into_iter()
            .filter(move |c| self.contains(*c))
    }
}

impl FromIterator<Capability> for CapabilitySet {
    fn from_iter<I: IntoIterator<Item = Capability>>(iter: I) -> Self {
        iter.into_iter().fold(Self::EMPTY, Self::with)
    }
}

/// Fehler „Anbieter/Deployment kann diese Art Anfrage nicht“ (400, nicht retrybar).
pub fn unsupported(kind: ProviderKind, cap: Capability) -> ApiError {
    ApiError::invalid_request(format!(
        "Anbieter {} unterstützt {cap} nicht",
        kind.as_str()
    ))
    .with_code("unsupported_capability")
}

/// Inkrementeller Decoder Upstream-Bytes → IR-Ereignisse.
///
/// Bytes kommen in beliebigen Stücken (auch mitten in UTF-8-Zeichen oder SSE-Zeilen).
/// Fehler des Anbieters im Stream und nicht lesbare Daten werden als
/// [`StreamEvent::Error`] gemeldet; danach liefert der Decoder nichts mehr.
pub trait StreamDecoder: Send {
    /// Verarbeitet weitere Bytes und hängt die vollständigen Ereignisse an `out` an.
    fn push(&mut self, chunk: &[u8], out: &mut Vec<StreamEvent>);
    /// Der Upstream-Body ist zu Ende. Endet der Stream ohne regulären Abschluss, hängt der
    /// Decoder ein Fehlerereignis an.
    fn finish(&mut self, out: &mut Vec<StreamEvent>);
    /// `true`, sobald der Stream regulär abgeschlossen oder mit Fehler beendet ist.
    fn is_done(&self) -> bool;
    /// Zwischengespeicherte, noch nicht dekodierte Bytes (AIGW-58: Obergrenze im
    /// Upstream-Stream gegen Speicher-DoS). Standard `0`.
    fn pending_bytes(&self) -> usize {
        0
    }
}

/// Anbieter-Adapter für ein Deployment (Architektur §5).
pub trait Provider: Send + Sync + fmt::Debug {
    /// Art des Anbieters.
    fn kind(&self) -> ProviderKind;
    /// Fähigkeiten.
    fn capabilities(&self) -> &Capabilities;
    /// Zielformat (entscheidet, ob `extra` der Anfrage weitergegeben wird).
    fn native_format(&self) -> WireFormat;
    /// Modellname beim Anbieter (bzw. Azure-Deployment-Name).
    fn provider_model(&self) -> &str;
    /// Baut die HTTP-Anfrage. Die Anfrage wird übernommen (keine Kopie); für Retries klont der
    /// Aufrufer vorher. Übersetzungsverluste landen in `losses`.
    fn build_request(
        &self,
        req: ChatRequest,
        losses: &mut Losses,
    ) -> Result<UpstreamRequest, ApiError>;
    /// Parst eine nicht gestreamte Erfolgsantwort.
    fn parse_response(&self, body: &[u8]) -> Result<ChatResponse, ApiError>;
    /// Neuer Stream-Decoder (einer je Anfrage).
    fn stream_decoder(&self) -> Box<dyn StreamDecoder>;
    /// Klassifiziert eine Fehlerantwort (Status ≥ 400) des Anbieters.
    fn classify_error(&self, status: StatusCode, headers: &HeaderMap, body: &[u8])
    -> UpstreamError;
    /// Optionale asynchrone Vorbereitung vor `build_request`, z. B. ein OAuth-Token holen,
    /// wenn (noch) keines im Cache liegt (Vertex). `None` = nichts zu tun (Standard, kein
    /// Overhead im Hot Path). Erneuerungen vor Ablauf laufen im Hintergrund; nur ohne gültiges
    /// Token wartet die Anfrage (innerhalb ihres Zeitlimits). `client` ist der HTTP-Client des
    /// Deployments (gleiche SSRF-Regeln und Vertrauensanker wie die Anbieter-Aufrufe).
    fn prepare(&self, client: &UpstreamClient) -> Option<PrepareFuture<'_>> {
        let _ = client;
        None
    }

    // --- Weitere Endpunkte (AIGW-21); Standard: nicht unterstützt -------------------------

    /// Baut die Embedding-Anfrage(n). Mehrere Anfragen, wenn der Anbieter je Aufruf nur eine
    /// bzw. wenige Eingaben annimmt (z. B. Bedrock Titan); die Antworten werden in
    /// Reihenfolge zusammengeführt.
    fn build_embeddings(
        &self,
        req: EmbeddingsRequest,
        losses: &mut Losses,
    ) -> Result<Vec<UpstreamRequest>, ApiError> {
        let _ = (req, losses);
        Err(unsupported(self.kind(), Capability::Embeddings))
    }

    /// Parst eine Embedding-Antwort (Header z. B. für Token-Zahlen von Bedrock).
    fn parse_embeddings(
        &self,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<EmbeddingsResponse, ApiError> {
        let _ = (headers, body);
        Err(unsupported(self.kind(), Capability::Embeddings))
    }

    /// Baut die Anfrage zur Bilderzeugung.
    fn build_images(
        &self,
        req: ImageRequest,
        losses: &mut Losses,
    ) -> Result<UpstreamRequest, ApiError> {
        let _ = (req, losses);
        Err(unsupported(self.kind(), Capability::Images))
    }

    /// Parst die Antwort der Bilderzeugung.
    fn parse_images(&self, body: &[u8]) -> Result<ImageResponse, ApiError> {
        let _ = body;
        Err(unsupported(self.kind(), Capability::Images))
    }

    /// Baut die Anfrage einer Transkription/Übersetzung.
    fn build_transcription(
        &self,
        req: TranscriptionRequest,
        losses: &mut Losses,
    ) -> Result<UpstreamRequest, ApiError> {
        let _ = (req, losses);
        Err(unsupported(self.kind(), Capability::AudioTranscription))
    }

    /// Parst die Antwort einer Transkription/Übersetzung (`format` = angefragtes Format).
    fn parse_transcription(
        &self,
        format: TranscriptFormat,
        body: &[u8],
    ) -> Result<TranscriptionResponse, ApiError> {
        let _ = (format, body);
        Err(unsupported(self.kind(), Capability::AudioTranscription))
    }

    /// Baut die Anfrage der Sprachausgabe (die Antwort sind Audio-Bytes, gestreamt).
    fn build_speech(
        &self,
        req: SpeechRequest,
        losses: &mut Losses,
    ) -> Result<UpstreamRequest, ApiError> {
        let _ = (req, losses);
        Err(unsupported(self.kind(), Capability::AudioSpeech))
    }
}

/// Fehler beim Aufbau eines Adapters aus der Konfiguration.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// Für die Anbieterart ist kein Adapter registriert.
    #[error("kein Adapter für Anbieter {0} registriert")]
    UnsupportedProvider(&'static str),
    /// Pflichtangabe fehlt.
    #[error("Deployment {deployment}: {what} fehlt")]
    Missing {
        /// Deployment.
        deployment: String,
        /// Was fehlt.
        what: &'static str,
    },
    /// Ungültige Angabe.
    #[error("Deployment {deployment}: {message}")]
    Invalid {
        /// Deployment.
        deployment: String,
        /// Beschreibung.
        message: String,
    },
}

/// Fabrik: Deployment (mit entschlüsselten Zugangsdaten) → Adapter.
pub type ProviderFactory = fn(&DeploymentCtx) -> Result<Arc<dyn Provider>, ConfigError>;

/// Verzeichnis der Adapter-Fabriken je Anbieterart.
#[derive(Clone)]
pub struct Registry {
    factories: HashMap<ProviderKind, ProviderFactory>,
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut kinds: Vec<&str> = self.factories.keys().map(|k| k.as_str()).collect();
        kinds.sort_unstable();
        f.debug_struct("Registry").field("kinds", &kinds).finish()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::with_builtin()
    }
}

impl Registry {
    /// Leeres Verzeichnis.
    pub fn empty() -> Self {
        Self {
            factories: HashMap::new(),
        }
    }

    /// Verzeichnis mit allen eingebauten Adaptern (`openai`, `azure-openai`,
    /// `openai-compatible`, `ollama`, `vllm`, `mistral`, `anthropic`, `bedrock`, `vertex`).
    pub fn with_builtin() -> Self {
        let mut r = Self::empty();
        r.register(ProviderKind::OpenAi, openai::OpenAiAdapter::factory);
        r.register(ProviderKind::AzureOpenAi, openai::OpenAiAdapter::factory);
        r.register(
            ProviderKind::OpenAiCompatible,
            openai::OpenAiAdapter::factory,
        );
        r.register(ProviderKind::Ollama, openai::OpenAiAdapter::factory);
        r.register(ProviderKind::Vllm, openai::OpenAiAdapter::factory);
        r.register(ProviderKind::Mistral, openai::OpenAiAdapter::factory);
        r.register(
            ProviderKind::Anthropic,
            anthropic::AnthropicAdapter::factory,
        );
        r.register(ProviderKind::Bedrock, bedrock::BedrockAdapter::factory);
        r.register(ProviderKind::Vertex, vertex::VertexAdapter::factory);
        r
    }

    /// Registriert (oder ersetzt) eine Fabrik.
    pub fn register(&mut self, kind: ProviderKind, factory: ProviderFactory) {
        self.factories.insert(kind, factory);
    }

    /// `true`, wenn für `kind` ein Adapter registriert ist.
    pub fn supports(&self, kind: ProviderKind) -> bool {
        self.factories.contains_key(&kind)
    }

    /// Baut den Adapter für ein Deployment.
    pub fn build(&self, dep: &DeploymentCtx) -> Result<Arc<dyn Provider>, ConfigError> {
        let kind = dep.deployment.spec.provider;
        let factory = self
            .factories
            .get(&kind)
            .ok_or(ConfigError::UnsupportedProvider(kind.as_str()))?;
        factory(dep)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_knows_openai_family() {
        let r = Registry::with_builtin();
        assert!(r.supports(ProviderKind::OpenAi));
        assert!(r.supports(ProviderKind::AzureOpenAi));
        assert!(r.supports(ProviderKind::OpenAiCompatible));
        for k in [
            ProviderKind::Anthropic,
            ProviderKind::Ollama,
            ProviderKind::Vllm,
            ProviderKind::Mistral,
        ] {
            assert!(r.supports(k), "{k:?}");
        }
        assert!(format!("{r:?}").contains("azure-openai"));
        assert!(r.supports(ProviderKind::Bedrock));
        assert!(r.supports(ProviderKind::Vertex));
    }
}
