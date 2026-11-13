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
//! - `/admin/api/v1/admin-api-keys` – Admin-API-Keys ([`admin_keys`]),
//!   `/admin/api/v1/session` – angemeldeter Principal
//! - `/admin/auth/{login,callback,logout}` – OIDC-Login und Abmeldung ([`session`], [`oidc`])
//! - `GET /admin/api/v1/usage/budgets` – Budgetstände (siehe [`usage`])
//! - `GET /admin/…` – eingebettete Svelte-SPA (siehe [`ui`])
//!
//! Fehler kommen einheitlich als Problem Details (RFC 9457, [`problem::Problem`]).

pub mod admin_keys;
pub mod api;
pub mod auth;
pub mod credentials;
pub mod deployments;
pub mod gitops;
pub mod oidc;
pub mod prices;
pub mod problem;
pub mod rbac;
pub mod read_only;
pub mod resources;
pub mod session;
pub mod ui;
pub mod usage;
pub use aigw_core::upstream;

use std::fmt;
use std::sync::Arc;

use aigw_store::{Keyring, Store};
use axum::Router;
use tokio::sync::Mutex;

use aigw_core::config::AdminAuthConfig;
pub use api::ApiDoc;
pub use auth::{
    AdminAuthenticator, AuthChain, BootstrapGate, BootstrapToken, Permission, Principal,
};
pub use problem::Problem;
pub use rbac::Target;
pub use upstream::UpstreamPolicy;

/// Zustand der Admin-API.
#[derive(Clone)]
pub struct AdminState {
    /// Datenbank (jede Änderung erhöht `config_version`, der Reloader baut den Snapshot neu).
    pub store: Store,
    /// Anmeldung (austauschbar: Session, Admin-API-Key, Bootstrap-Token; siehe [`AuthChain`]).
    pub auth: Arc<dyn AdminAuthenticator>,
    /// OIDC-Login (falls konfiguriert).
    pub(crate) login: Option<session::SharedLogin>,
    /// Erlaubter `Origin` schreibender Session-Anfragen (aus `redirect_url`); sonst `Host`.
    pub(crate) allowed_origin: Option<String>,
    /// Serialisiert Lesen-Prüfen-Schreiben bei `If-Match` innerhalb dieser Instanz.
    pub(crate) write_lock: Arc<Mutex<()>>,
    /// Master-Keys zum Verschlüsseln von Anbieter-Zugangsdaten (ohne: Zugangsdaten nur lesbar).
    pub(crate) keyring: Option<Arc<Keyring>>,
    /// Regeln für Upstream-Endpunkte (SSRF-Schutz).
    pub(crate) upstreams: Arc<UpstreamPolicy>,
}

impl AdminState {
    /// Neuer Zustand mit genau diesem Anmeldeverfahren (ohne OIDC-Login).
    pub fn new(store: Store, auth: Arc<dyn AdminAuthenticator>) -> Self {
        Self {
            store,
            auth,
            login: None,
            allowed_origin: None,
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

    /// Zustand laut Konfiguration `admin.auth`: Admin-API-Keys, Bootstrap-Token (falls
    /// übergeben und nicht `off`), OIDC-Login mit Sessions (falls `oidc.issuer` gesetzt).
    ///
    /// `client_secret`: Client-Secret des IdP (aus [`session::client_secret`]).
    pub fn from_config(
        store: Store,
        cfg: &AdminAuthConfig,
        bootstrap: Option<BootstrapToken>,
        client_secret: Option<String>,
    ) -> Result<Self, String> {
        let mut chain = AuthChain::new().bearer(Arc::new(admin_keys::AdminKeyAuthenticator::new(
            store.clone(),
        )));
        if let Some(token) = bootstrap
            && cfg.bootstrap != aigw_core::config::BootstrapMode::Off
        {
            chain = chain.bearer(Arc::new(BootstrapGate::new(
                token,
                cfg.bootstrap,
                store.clone(),
            )));
        }
        let mut login = None;
        let mut allowed_origin = None;
        if cfg.oidc.enabled() {
            let svc = session::LoginService::new(&cfg.oidc, &cfg.session, client_secret)?;
            allowed_origin = session::origin_of(svc.oidc.redirect_url());
            chain = chain.session(Arc::new(session::SessionAuthenticator::new(
                store.clone(),
                cfg.session.clone(),
                cfg.oidc.group_roles.clone(),
            )));
            login = Some(Arc::new(svc));
        }
        let chain = chain.allowed_origin(allowed_origin.clone());
        Ok(Self {
            store,
            auth: Arc::new(chain),
            login,
            allowed_origin,
            write_lock: Arc::new(Mutex::new(())),
            keyring: None,
            upstreams: Arc::new(UpstreamPolicy::strict()),
        })
    }

    /// `true`, wenn OIDC-Login konfiguriert ist.
    pub fn oidc_enabled(&self) -> bool {
        self.login.is_some()
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
    let protected = resources::router()
        .merge(deployments::router())
        .merge(credentials::router())
        .merge(prices::router())
        .merge(admin_keys::router())
        .merge(session::protected_router())
        .route_layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state.auth),
            auth::require_principal,
        ));
    let api = protected.merge(session::public_router()).with_state(state);
    router().merge(api)
}

/// OpenAPI-Spezifikation der Admin-API als formatiertes JSON.
pub fn openapi_json() -> Result<String, serde_json::Error> {
    use utoipa::OpenApi as _;
    serde_json::to_string_pretty(&ApiDoc::openapi())
}
