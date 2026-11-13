// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Admin-API und Auslieferung der Admin-UI.
//!
//! - `GET /admin/api/v1/version` – Version des Gateways (ohne Anmeldung)
//! - `GET /admin/api/openapi.json` – OpenAPI-Spezifikation (Quelle der TS-Typen der UI, E3)
//! - `/admin/api/v1/{organizations,teams,users,api-keys,model-groups}` – Ressourcen
//!   (siehe [`resources`]), nur mit Anmeldung ([`auth`])
//! - `GET /admin/…` – eingebettete Svelte-SPA (siehe [`ui`])
//!
//! Fehler kommen einheitlich als Problem Details (RFC 9457, [`problem::Problem`]).

pub mod api;
pub mod auth;
pub mod problem;
pub mod resources;
pub mod ui;

use std::fmt;
use std::sync::Arc;

use aigw_store::Store;
use axum::Router;
use tokio::sync::Mutex;

pub use api::ApiDoc;
pub use auth::{AdminAuthenticator, BootstrapToken, Permission, Principal};
pub use problem::Problem;

/// Zustand der Admin-API.
#[derive(Clone)]
pub struct AdminState {
    /// Datenbank (jede Änderung erhöht `config_version`, der Reloader baut den Snapshot neu).
    pub store: Store,
    /// Anmeldung (austauschbar, AIGW-32: OIDC/Admin-Keys).
    pub auth: Arc<dyn AdminAuthenticator>,
    /// Serialisiert Lesen-Prüfen-Schreiben bei `If-Match` innerhalb dieser Instanz.
    pub(crate) write_lock: Arc<Mutex<()>>,
}

impl AdminState {
    /// Neuer Zustand.
    pub fn new(store: Store, auth: Arc<dyn AdminAuthenticator>) -> Self {
        Self {
            store,
            auth,
            write_lock: Arc::new(Mutex::new(())),
        }
    }
}

impl fmt::Debug for AdminState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdminState")
            .field("store", &self.store)
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
