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
//! - `/admin/api/v1/{mcp-servers,mcp-grants}` – MCP-Gateway ([`mcp`], AIGW-41)
//! - `/admin/api/v1/plugin-bindings` – Plugin-Kette, geprüft mit der Plugin-Registry
//!   ([`plugin_bindings`], AIGW-69); `/admin/api/v1/plugins` und
//!   `/admin/api/v1/plugin-bindings:test` – Katalog und Testlauf für die Editoren ([`plugins`], AIGW-52)
//! - `/admin/api/v1/guardrail-profiles` – Standard-Guardrail-Profile anwenden
//!   ([`guardrail_profiles`], AIGW-86)
//! - `/admin/api/v1/admin-api-keys` – Admin-API-Keys ([`admin_keys`]),
//!   `/admin/api/v1/session` – angemeldeter Principal
//! - `/admin/api/v1/audit-log` – Audit-Log lesen und exportieren ([`audit`])
//! - `/admin/auth/{login,callback,logout}` – OIDC-Login und Abmeldung ([`session`], [`oidc`])
//! - `GET /admin/api/v1/usage/budgets` – Budgetstände (siehe [`usage`])
//! - `/admin/api/v1/analytics/{timeseries,summary,top}`, `/admin/api/v1/request-logs` –
//!   Dashboards und Log-Explorer (siehe [`analytics`])
//! - `GET /admin/…` – eingebettete Svelte-SPA (siehe [`ui`])
//!
//! Fehler kommen einheitlich als Problem Details (RFC 9457, [`problem::Problem`]).

pub mod admin_keys;
pub mod analytics;
pub mod api;
pub mod audit;
pub mod auth;
pub mod bounds;
pub mod credentials;
pub mod deployments;
pub mod gitops;
pub mod guardrail_profiles;
pub mod mcp;
pub mod mcp_status;
pub mod oidc;
pub mod playground;
pub mod plugin_bindings;
pub mod plugins;
pub mod price_suggestions;
pub mod prices;
pub mod problem;
pub mod providers;
pub mod rbac;
pub mod read_only;
pub mod resources;
pub mod secrets;
pub mod session;
pub mod tools;
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
    /// Einstellungen der Anmeldung, die die Routen brauchen (F87, F90, F113).
    pub(crate) auth_opts: AuthOptions,
    /// Modus „nur YAML“ (`gitops.read_only_admin`, für `GET /auth/info`, F56).
    pub(crate) read_only: bool,
    /// Bootstrap-Token (für `GET /auth/info`: aktiv?, F56).
    pub(crate) bootstrap: Option<Arc<BootstrapGate>>,
    /// Mock-Modus der UI anbieten (`admin.ui.mock`, für `GET /auth/info`, F135).
    pub(crate) ui_mock: bool,
    /// Link-Vorlage zum Tracing-System (`admin.ui.trace_url`, für `GET /auth/info`).
    pub(crate) trace_url: Option<String>,
    /// Preisvorschläge (AIGW-85; ohne: `404` auf `/price-suggestions`).
    pub(crate) price_suggester: Option<Arc<aigw_billing::suggest::Suggester>>,
}

/// Einstellungen der Anmeldung (aus `admin.auth`).
#[derive(Debug, Clone)]
pub(crate) struct AuthOptions {
    /// Login/Logout nur mit Audit-Eintrag in derselben Transaktion (F113).
    pub(crate) audit_required: bool,
    /// Höchstlaufzeit von Admin-API-Keys in Sekunden (F87).
    pub(crate) max_admin_key_lifetime: Option<u64>,
    /// Gültigkeit von Einladungslinks in Sekunden (F90).
    pub(crate) invite_ttl_secs: u64,
}

impl Default for AuthOptions {
    fn default() -> Self {
        let d = AdminAuthConfig::default();
        Self {
            audit_required: d.audit_required,
            max_admin_key_lifetime: None,
            invite_ttl_secs: d.oidc.invite_ttl_secs,
        }
    }
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
            auth_opts: AuthOptions::default(),
            read_only: false,
            bootstrap: None,
            ui_mock: true,
            trace_url: None,
            price_suggester: None,
        }
    }

    /// Modus „nur YAML“ (`gitops.read_only_admin`) – nur für die Anzeige in
    /// `GET /admin/api/v1/auth/info` (die Sperre selbst setzt [`read_only::apply`]).
    #[must_use]
    pub fn with_read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// Mock-Modus der Admin-UI anbieten (`admin.ui.mock`, F135) – nur für die Anzeige in
    /// `GET /admin/api/v1/auth/info`.
    #[must_use]
    pub fn with_ui_mock(mut self, ui_mock: bool) -> Self {
        self.ui_mock = ui_mock;
        self
    }

    /// Link-Vorlage zum Tracing-System für den Log-Explorer (`admin.ui.trace_url`, `{traceId}`).
    #[must_use]
    pub fn with_trace_url(mut self, trace_url: Option<String>) -> Self {
        self.trace_url = trace_url.filter(|u| !u.trim().is_empty());
        self
    }

    /// Preisvorschläge (`billing.price_suggestions`, AIGW-85).
    #[must_use]
    pub fn with_price_suggester(
        mut self,
        suggester: Option<Arc<aigw_billing::suggest::Suggester>>,
    ) -> Self {
        self.price_suggester = suggester;
        self
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
        let auth_opts = AuthOptions {
            audit_required: cfg.audit_required,
            max_admin_key_lifetime: cfg.max_admin_key_lifetime_secs()?,
            invite_ttl_secs: cfg.oidc.invite_ttl_secs.max(60),
        };
        let mut gate = None;
        if let Some(token) = bootstrap
            && cfg.bootstrap != aigw_core::config::BootstrapMode::Off
        {
            let g = Arc::new(BootstrapGate::new(token, cfg.bootstrap, store.clone()));
            chain = chain.bearer(Arc::clone(&g) as Arc<dyn AdminAuthenticator>);
            gate = Some(g);
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
            auth_opts,
            read_only: false,
            bootstrap: gate,
            ui_mock: true,
            trace_url: None,
            price_suggester: None,
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
    Router::new()
        .merge(api::router())
        .merge(ui::router())
        .layer(axum::middleware::map_response(security_headers))
}

/// AIGW-58: Sicherheits-Header für alle Antworten unter `/admin` (die UI setzt eigene CSP
/// und Cache-Regeln; hier nur, was fehlt): kein Einbetten in Frames, kein MIME-Sniffing,
/// kein Referrer, API-Antworten (JSON/Problem) nicht cachen und ohne aktive Inhalte.
async fn security_headers(mut res: axum::response::Response) -> axum::response::Response {
    use axum::http::{HeaderValue, header};
    let h = res.headers_mut();
    let defaults = [
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::CACHE_CONTROL, "no-store"),
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; frame-ancestors 'none'",
        ),
    ];
    for (name, value) in defaults {
        if !h.contains_key(&name) {
            h.insert(name, HeaderValue::from_static(value));
        }
    }
    res
}

/// Router für alles unter `/admin` einschließlich der Ressourcen-API.
pub fn router_with(state: AdminState) -> Router {
    let protected = resources::router()
        .merge(deployments::router())
        .merge(credentials::router())
        .merge(secrets::router())
        .merge(prices::router())
        .merge(price_suggestions::router())
        .merge(mcp::router())
        .merge(plugin_bindings::router())
        .merge(tools::router())
        .merge(plugins::router())
        .merge(guardrail_profiles::router())
        .merge(admin_keys::router())
        .merge(audit::router())
        .merge(session::protected_router())
        .route_layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state.auth),
            auth::require_principal,
        ));
    let api = protected
        .merge(session::public_router())
        .with_state(state)
        .layer(axum::middleware::map_response(security_headers));
    router().merge(api)
}

/// OpenAPI-Spezifikation der Admin-API als formatiertes JSON.
pub fn openapi_json() -> Result<String, serde_json::Error> {
    use utoipa::OpenApi as _;
    serde_json::to_string_pretty(&ApiDoc::openapi())
}

#[cfg(test)]
mod security_header_tests {
    #![allow(clippy::unwrap_used)]

    use axum::body::Body;
    use axum::http::{Request, header};
    use tower::ServiceExt as _;

    /// AIGW-58: Sicherheits-Header auf API-Antworten; die UI behält ihre eigene CSP.
    #[tokio::test]
    async fn admin_responses_carry_security_headers() {
        let get = |uri: &'static str| async move {
            super::router()
                .oneshot(Request::get(uri).body(Body::empty()).unwrap())
                .await
                .unwrap()
        };
        let api = get("/admin/api/v1/version").await;
        let h = api.headers();
        assert_eq!(h[header::X_FRAME_OPTIONS], "DENY");
        assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(h[header::CACHE_CONTROL], "no-store");
        assert!(
            h[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap()
                .contains("frame-ancestors 'none'")
        );
        let ui = get("/admin/").await;
        let csp = ui.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap();
        assert!(csp.contains("script-src 'self'"), "{csp}");
        assert_eq!(ui.headers()[header::X_FRAME_OPTIONS], "DENY");
    }
}
