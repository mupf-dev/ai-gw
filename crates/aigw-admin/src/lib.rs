// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Admin-API und Auslieferung der Admin-UI.
//!
//! - `GET /admin/api/v1/version` – Version des Gateways (ohne Anmeldung)
//! - `GET /admin/api/openapi.json` – OpenAPI-Spezifikation (Quelle der TS-Typen der UI, E3)
//! - `/admin/api/v1/{organizations,teams,users,api-keys,model-groups}` – Ressourcen
//!   (siehe [`resources`]), nur mit Anmeldung ([`auth`])
//! - `/admin/api/v1/config:{export,apply}` – GitOps (siehe [`gitops`]), Modus „nur YAML“
//!   siehe [`read_only`]
//! - `/admin/api/v1/{deployments,provider-credentials,prices}` – Routing-Ressourcen
//!   (siehe [`deployments`], [`credentials`], [`prices`]; Endpunktprüfung in [`upstream`])
//! - `GET /admin/…` – eingebettete Svelte-SPA (siehe [`ui`])
//!
//! Fehler kommen einheitlich als Problem Details (RFC 9457, [`problem::Problem`]).

pub mod api;
pub mod auth;
pub mod credentials;
pub mod deployments;
pub mod gitops;
pub mod prices;
pub mod problem;
pub mod read_only;
pub mod resources;
pub mod ui;
pub use aigw_core::upstream;

use std::fmt;
use std::sync::Arc;

use aigw_store::{Keyring, Store};
use axum::Router;
use tokio::sync::Mutex;

pub use api::ApiDoc;
pub use auth::{AdminAuthenticator, BootstrapToken, Permission, Principal};
pub use problem::Problem;
pub use upstream::UpstreamPolicy;

/// Zustand der Admin-API.
#[derive(Clone)]
pub struct AdminState {
    /// Datenbank (jede Änderung erhöht `config_version`, der Reloader baut den Snapshot neu).
    pub store: Store,
    /// Anmeldung (austauschbar, AIGW-32: OIDC/Admin-Keys).
    pub auth: Arc<dyn AdminAuthenticator>,
    /// Serialisiert Lesen-Prüfen-Schreiben bei `If-Match` innerhalb dieser Instanz.
    pub(crate) write_lock: Arc<Mutex<()>>,
    /// Master-Keys zum Verschlüsseln von Anbieter-Zugangsdaten (ohne: Zugangsdaten nur lesbar).
    pub(crate) keyring: Option<Arc<Keyring>>,
    /// Regeln für Upstream-Endpunkte (SSRF-Schutz).
    pub(crate) upstreams: Arc<UpstreamPolicy>,
}

impl AdminState {
    /// Neuer Zustand.
    pub fn new(store: Store, auth: Arc<dyn AdminAuthenticator>) -> Self {
        Self {
            store,
            auth,
            write_lock: Arc::new(Mutex::new(())),
            keyring: None,
            upstreams: Arc::new(UpstreamPolicy::strict()),
        }
    }

    /// Master-Keys zum Verschlüsseln von Anbieter-Zugangsdaten.
    #[must_use]
    pub fn with_keyring(mut self, keyring: Arc<Keyring>) -> Self {
        self.keyring = Some(keyring);
        self
    }

    /// Regeln für Upstream-Endpunkte (Standard: [`UpstreamPolicy::strict`]).
    #[must_use]
    pub fn with_upstream_policy(mut self, policy: UpstreamPolicy) -> Self {
        self.upstreams = Arc::new(policy);
        self
    }
}

impl fmt::Debug for AdminState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdminState")
            .field("store", &self.store)
            .field("keyring", &self.keyring.is_some())
            .field("upstreams", &self.upstreams)
            .finish_non_exhaustive()
    }
}

/// Router für alles unter `/admin` ohne Ressourcen-API (nur Version, OpenAPI, UI).
pub fn router() -> Router {
    Router::new().merge(api::router()).merge(ui::router())
}

/// Router für alles unter `/admin` einschließlich der Ressourcen-API.
pub fn router_with(state: AdminState) -> Router {
    let resources = resources::router()
        .merge(deployments::router())
        .merge(credentials::router())
        .merge(prices::router())
        .route_layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state.auth),
            auth::require_principal,
        ))
        .with_state(state);
    router().merge(resources)
}

/// OpenAPI-Spezifikation der Admin-API als formatiertes JSON.
pub fn openapi_json() -> Result<String, serde_json::Error> {
    use utoipa::OpenApi as _;
    serde_json::to_string_pretty(&ApiDoc::openapi())
}
