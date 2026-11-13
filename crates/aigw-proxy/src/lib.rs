// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Hot Path (Datenebene) von ai-gw.
//!
//! Routen:
//! - `POST /v1/chat/completions` – OpenAI Chat Completions (JSON oder SSE)
//! - `POST /v1/messages` – Anthropic Messages (JSON oder SSE)
//! - `GET /v1/models` – Modellliste (Anthropic-Format, wenn `anthropic-version` gesetzt ist)
//! - `POST /v1/responses` – OpenAI Responses API (zustandslos, über die Chat-IR; AIGW-21)
//! - `POST /v1/embeddings`, `POST /v1/images/generations`, `POST /v1/audio/transcriptions`,
//!   `POST /v1/audio/translations`, `POST /v1/audio/speech` (AIGW-21)
//! - `GET /healthz` – Liveness
//!
//! Jede Anfrage wird in die IR übersetzt ([`aigw_core::ir`]), über [`ModelRoutes`] einem
//! Deployment ([`ChatProvider`], in der Regel ein [`aigw_providers::Upstream`]) zugeordnet
//! und im Format des Eingangs beantwortet – auch Fehler. Streams laufen Upstream-Bytes →
//! Decoder → IR-Ereignisse → Encoder des Eingangsformats, ohne Puffern und mit Backpressure.
//! Jede Antwort trägt den Header `x-aigw-request-id` (UUIDv7); am Ende jeder Anfrage geht ein
//! [`RequestRecord`](aigw_core::observe::RequestRecord) an den
//! [`RequestObserver`](aigw_core::observe::RequestObserver).
//!
//! Die Schlüsselprüfung übernimmt [`SnapshotAuthenticator`] (Keys, Ablauf, Modellfreigaben
//! aus dem Snapshot); [`auth::AllowAll`] lässt alles zu (Tests, Entwicklung). Das Routing
//! ([`routing`]: Strategien, Prioritätsgruppen, Retries, Fallbacks, Circuit Breaker) kommt im
//! Betrieb aus dem Snapshot ([`SnapshotRoutes`]); [`StaticRoutes`] und
//! [`mock::MockProvider`] dienen Tests und Benchmarks. Limits, Budgets und Kosten je Anfrage
//! übernimmt [`Limiter`] ([`limits`]) an einer Stelle zwischen Modellfreigabe und Routing.
//!
//! Die Plugin-Kette ([`PluginChains`], AIGW-36) hängt nach Auth/Modellfreigabe (und den
//! Limits) vor dem Routing (Pre-Plugins), vor dem Encoder (Post-Plugins) und im Stream-Pfad
//! (Stream-Haken je IR-Ereignis); siehe `docs/konzept/12-plugins.md`.

pub mod auth;
mod endpoints;
mod error;
mod handlers;
pub mod limits;
pub mod mock;
mod observe;
mod pipeline;
mod plugins;
pub mod provider;
mod request_id;
pub mod routes;
pub mod routing;
mod stream;
mod trace;

use std::fmt;
use std::sync::Arc;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{any, get, post};

pub use aigw_core::observe::{NoopObserver, PayloadMode, RequestObserver, RequestRecord};
pub use auth::{
    AllowAll, ApiKey, AuthContext, Authenticator, KeySource, SnapshotAuthenticator, extract_api_key,
};
pub use error::ErrorFormat;
pub use limits::{Limiter, TokenEstimate};
pub use provider::{BoxFuture, ChatProvider, Describe, EventStream};
pub use request_id::{REQUEST_ID_HEADER, RequestId, RequestStart};
pub use routes::{ModelRoutes, StaticRoutes};
pub use routing::{GroupRoute, RoutesTask, SnapshotRoutes};

pub use aigw_core::config::{DEFAULT_MAX_AUDIO_BYTES, DEFAULT_MAX_BODY_BYTES};
pub use aigw_plugins::PluginChains;

/// Antwort-Header: gewähltes Deployment (Name).
pub const DEPLOYMENT_HEADER: &str = "x-aigw-deployment";
/// Antwort-Header: Anzahl der Versuche (inkl. Retries und Fallbacks).
pub const ATTEMPTS_HEADER: &str = "x-aigw-attempts";
/// Antwort-Header: Modellgruppe, falls die Antwort über eine Fallback-Kette kam.
pub const FALLBACK_HEADER: &str = "x-aigw-fallback";

/// Einstellungen der Datenebene.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Maximale Größe eines Request-Bodys in Bytes (`server.data.max_body_bytes`).
    pub max_body_bytes: usize,
    /// Antwort-Header `x-aigw-deployment`, `x-aigw-attempts`, `x-aigw-fallback`
    /// (`server.data.routing_headers`).
    pub routing_headers: bool,
    /// Maximale Größe eines Audio-Uploads (`/v1/audio/transcriptions|translations`) in Bytes
    /// (`server.data.max_audio_bytes`, Standard 25 MiB; höchstens `max_body_bytes`).
    pub max_audio_bytes: usize,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            routing_headers: true,
            max_audio_bytes: DEFAULT_MAX_AUDIO_BYTES,
        }
    }
}

impl From<&aigw_core::config::DataListenerConfig> for ProxyConfig {
    fn from(c: &aigw_core::config::DataListenerConfig) -> Self {
        Self {
            max_body_bytes: c.max_body_bytes,
            routing_headers: c.routing_headers,
            max_audio_bytes: c.max_audio_bytes,
        }
    }
}

/// Gemeinsamer Zustand der Datenebene.
#[derive(Clone)]
pub struct ProxyState {
    /// Modellname → Route der Modellgruppe.
    pub routes: Arc<dyn ModelRoutes>,
    /// Schlüsselprüfung.
    pub auth: Arc<dyn Authenticator>,
    /// Empfänger der Kennzahlen je Anfrage (Request-Log, Abrechnung).
    pub observer: Arc<dyn RequestObserver>,
    /// Limits, Budgets und Zähler-Backend (auch für die Deployment-Kapazität).
    pub limits: Arc<Limiter>,
    /// Plugin-Ketten je (Org, Team, Key, Modellgruppe) (AIGW-36).
    pub plugins: Arc<PluginChains>,
    /// Einstellungen.
    pub config: ProxyConfig,
}

impl fmt::Debug for ProxyState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyState")
            .field("config", &self.config)
            .field("observer", &self.observer)
            .finish_non_exhaustive()
    }
}

impl ProxyState {
    /// Zustand mit eingebautem Mock-Anbieter und ohne Schlüsselprüfung ([`AllowAll`]).
    pub fn mock() -> Self {
        Self::with_routes(Arc::new(mock::mock_routes()))
    }

    /// Zustand mit den angegebenen Routen, ohne Schlüsselprüfung, ohne Beobachter, mit
    /// lokalen Zählern ohne Budgets.
    pub fn with_routes(routes: Arc<dyn ModelRoutes>) -> Self {
        Self {
            routes,
            auth: Arc::new(AllowAll),
            observer: Arc::new(NoopObserver),
            limits: Arc::new(Limiter::local()),
            plugins: Arc::new(PluginChains::default()),
            config: ProxyConfig::default(),
        }
    }
}

/// Router der Datenebene (`/v1/*`, `/healthz`).
pub fn router(state: ProxyState) -> Router {
    let max_body = state.config.max_body_bytes;
    let state = Arc::new(state);
    Router::new()
        .route(
            "/v1/chat/completions",
            post(handlers::chat_completions).fallback(handlers::method_not_allowed),
        )
        .route(
            "/v1/messages",
            post(handlers::messages).fallback(handlers::method_not_allowed),
        )
        .route(
            "/v1/models",
            get(handlers::models).fallback(handlers::method_not_allowed),
        )
        .route(
            "/v1/responses",
            post(endpoints::responses).fallback(handlers::method_not_allowed),
        )
        .route(
            "/v1/embeddings",
            post(endpoints::embeddings).fallback(handlers::method_not_allowed),
        )
        .route(
            "/v1/images/generations",
            post(endpoints::images_generations).fallback(handlers::method_not_allowed),
        )
        .route(
            "/v1/audio/transcriptions",
            post(endpoints::audio_transcriptions).fallback(handlers::method_not_allowed),
        )
        .route(
            "/v1/audio/translations",
            post(endpoints::audio_translations).fallback(handlers::method_not_allowed),
        )
        .route(
            "/v1/audio/speech",
            post(endpoints::audio_speech).fallback(handlers::method_not_allowed),
        )
        .route("/v1/{*rest}", any(handlers::not_found))
        .route("/healthz", get(healthz))
        .layer(DefaultBodyLimit::max(max_body))
        .layer(axum::middleware::from_fn(request_id::middleware))
        .with_state(state)
}

/// Liveness: antwortet ohne Abhängigkeiten mit `200 ok`.
pub async fn healthz() -> &'static str {
    "ok"
}
