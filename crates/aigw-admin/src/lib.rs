// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Admin-API und Auslieferung der Admin-UI.
//!
//! - `GET /admin/api/v1/version` – Version des Gateways
//! - `GET /admin/api/openapi.json` – OpenAPI-Spezifikation (Quelle der TS-Typen der UI, E3)
//! - `GET /admin/…` – eingebettete Svelte-SPA (siehe [`ui`])

pub mod api;
pub mod ui;

use axum::Router;

pub use api::ApiDoc;

/// Router für alles unter `/admin`.
pub fn router() -> Router {
    Router::new().merge(api::router()).merge(ui::router())
}

/// OpenAPI-Spezifikation der Admin-API als formatiertes JSON.
pub fn openapi_json() -> Result<String, serde_json::Error> {
    use utoipa::OpenApi as _;
    serde_json::to_string_pretty(&ApiDoc::openapi())
}
