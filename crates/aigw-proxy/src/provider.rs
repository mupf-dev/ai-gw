// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Ausführungsschnittstelle der Datenebene: „IR rein, IR raus“ für ein Deployment.
//!
//! Echte Anbieter laufen über [`aigw_providers::Upstream`] (Adapter + HTTP-Client), der
//! eingebaute [`MockProvider`](crate::mock::MockProvider) dient Tests und dem Betrieb ohne
//! konfigurierte Anbieter. Welches Deployment eine Anfrage bekommt, entscheidet
//! [`ModelRoutes`](crate::routes::ModelRoutes).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use aigw_core::ir::audio::{SpeechRequest, TranscriptionRequest, TranscriptionResponse};
use aigw_core::ir::embeddings::{EmbeddingsRequest, EmbeddingsResponse};
use aigw_core::ir::images::{ImageRequest, ImageResponse};
use aigw_core::ir::{ChatRequest, ChatResponse, UpstreamError};
use aigw_core::resource::Id;
use aigw_core::types::ProviderKind;
use aigw_providers::Upstream;

pub use aigw_providers::{ByteStream, Capability, EventStream, SpeechStream};

/// Geboxte Future (`Send`).
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Beschreibung eines Deployments für Log und Request-Log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Describe<'a> {
    /// Name des Deployments.
    pub deployment: &'a str,
    /// ID des Deployments (`None` beim Mock und bei programmatisch gebauten).
    pub deployment_id: Option<Id>,
    /// Modellgruppe.
    pub model_group: Option<&'a str>,
    /// Anbieter (`None` beim Mock).
    pub provider: Option<ProviderKind>,
    /// Modellname beim Anbieter.
    pub provider_model: &'a str,
}

/// Ein ausführbares Deployment.
pub trait ChatProvider: Send + Sync + 'static {
    /// Beschreibung.
    fn describe(&self) -> Describe<'_>;

    /// Nicht gestreamte Antwort.
    fn chat(self: Arc<Self>, req: ChatRequest) -> BoxFuture<Result<ChatResponse, UpstreamError>>;

    /// Gestreamte Antwort. Fehler vor dem ersten Ereignis kommen als `Err` (der Client erhält
    /// dann eine normale Fehlerantwort), spätere als
    /// [`StreamEvent::Error`](aigw_core::ir::StreamEvent::Error).
    fn chat_stream(
        self: Arc<Self>,
        req: ChatRequest,
    ) -> BoxFuture<Result<EventStream, UpstreamError>>;

    // --- Weitere Endpunkte (AIGW-21); Standard: nur Chat ----------------------------------

    /// Kann das Deployment Anfragen dieser Art ausführen?
    fn supports(&self, cap: Capability) -> bool {
        cap == Capability::Chat
    }

    /// Embeddings.
    fn embeddings(
        self: Arc<Self>,
        req: EmbeddingsRequest,
    ) -> BoxFuture<Result<EmbeddingsResponse, UpstreamError>> {
        let _ = req;
        unsupported(self.describe().provider, Capability::Embeddings)
    }

    /// Bilderzeugung.
    fn images(
        self: Arc<Self>,
        req: ImageRequest,
    ) -> BoxFuture<Result<ImageResponse, UpstreamError>> {
        let _ = req;
        unsupported(self.describe().provider, Capability::Images)
    }

    /// Transkription/Übersetzung.
    fn transcription(
        self: Arc<Self>,
        req: TranscriptionRequest,
    ) -> BoxFuture<Result<TranscriptionResponse, UpstreamError>> {
        let _ = req;
        unsupported(self.describe().provider, Capability::AudioTranscription)
    }

    /// Sprachausgabe (Audio-Bytes als Strom).
    fn speech(
        self: Arc<Self>,
        req: SpeechRequest,
    ) -> BoxFuture<Result<SpeechStream, UpstreamError>> {
        let _ = req;
        unsupported(self.describe().provider, Capability::AudioSpeech)
    }
}

fn unsupported<T: Send + 'static>(
    kind: Option<ProviderKind>,
    cap: Capability,
) -> BoxFuture<Result<T, UpstreamError>> {
    let err = aigw_providers::unsupported(kind.unwrap_or(ProviderKind::Mock), cap);
    Box::pin(async move { Err(UpstreamError::from(err)) })
}

impl ChatProvider for Upstream {
    fn describe(&self) -> Describe<'_> {
        let info = self.info();
        Describe {
            deployment: &info.deployment,
            deployment_id: info.deployment_id,
            model_group: info.model_group.as_deref(),
            provider: Some(info.provider),
            provider_model: &info.provider_model,
        }
    }

    fn chat(self: Arc<Self>, req: ChatRequest) -> BoxFuture<Result<ChatResponse, UpstreamError>> {
        Box::pin(async move { Upstream::chat(&self, req).await })
    }

    fn chat_stream(
        self: Arc<Self>,
        req: ChatRequest,
    ) -> BoxFuture<Result<EventStream, UpstreamError>> {
        Box::pin(async move { Upstream::chat_stream(&self, req).await })
    }

    fn supports(&self, cap: Capability) -> bool {
        self.provider().capabilities().supports(cap)
    }

    fn embeddings(
        self: Arc<Self>,
        req: EmbeddingsRequest,
    ) -> BoxFuture<Result<EmbeddingsResponse, UpstreamError>> {
        Box::pin(async move { Upstream::embeddings(&self, req).await })
    }

    fn images(
        self: Arc<Self>,
        req: ImageRequest,
    ) -> BoxFuture<Result<ImageResponse, UpstreamError>> {
        Box::pin(async move { Upstream::images(&self, req).await })
    }

    fn transcription(
        self: Arc<Self>,
        req: TranscriptionRequest,
    ) -> BoxFuture<Result<TranscriptionResponse, UpstreamError>> {
        Box::pin(async move { Upstream::transcription(&self, req).await })
    }

    fn speech(
        self: Arc<Self>,
        req: SpeechRequest,
    ) -> BoxFuture<Result<SpeechStream, UpstreamError>> {
        Box::pin(async move { Upstream::speech(&self, req).await })
    }
}
