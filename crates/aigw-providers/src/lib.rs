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
//! [`anthropic::AnthropicAdapter`] für die Anthropic Messages API. Bedrock und Vertex folgen.

pub mod anthropic;
pub mod auth;
mod client;
pub mod local;
pub mod mistral;
pub mod openai;
mod settings;
mod upstream;

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use aigw_core::ir::{ApiError, ChatRequest, ChatResponse, StreamEvent, UpstreamError};
use aigw_core::snapshot::DeploymentCtx;
use aigw_core::types::ProviderKind;
use aigw_core::wire::{Losses, WireFormat};
use bytes::Bytes;
use http::{HeaderMap, StatusCode};

pub use client::{ClientConfig, UpstreamClient};
pub use settings::{GROUP_INHERITED_SETTINGS, with_group_defaults};
pub use upstream::{EventStream, Timeouts, Upstream, UpstreamInfo};

/// Fertig gebaute Upstream-Anfrage (Body vollständig im Speicher).
pub type UpstreamRequest = http::Request<Bytes>;

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
    /// Embeddings (noch nicht umgesetzt).
    pub embeddings: bool,
    /// Usage im Stream anforderbar (`stream_options.include_usage`).
    pub stream_usage: bool,
    /// Feld für die Obergrenze der Ausgabe-Tokens.
    pub max_tokens_field: MaxTokensField,
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
    /// `openai-compatible`, `ollama`, `vllm`, `mistral`, `anthropic`).
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
    }
}
