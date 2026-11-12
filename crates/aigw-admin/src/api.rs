// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Admin-API `v1`.

use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use utoipa::{OpenApi, ToSchema};

/// OpenAPI-Dokument der Admin-API.
#[derive(Debug, OpenApi)]
#[openapi(
    info(title = "ai-gw Admin-API", description = "Verwaltungs-API von ai-gw"),
    paths(version),
    components(schemas(VersionInfo)),
    tags((name = "system", description = "Systeminformationen"))
)]
pub struct ApiDoc;

/// Versionsinformationen des laufenden Gateways.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct VersionInfo {
    /// Produktname.
    pub name: String,
    /// Version (SemVer).
    pub version: String,
    /// Art der eingebetteten UI: `ui` (gebaut) oder `fallback` (Platzhalter).
    pub ui: String,
}

impl VersionInfo {
    /// Version dieses Builds.
    pub fn current() -> Self {
        Self {
            name: aigw_core::NAME.to_owned(),
            version: aigw_core::VERSION.to_owned(),
            ui: crate::ui::KIND.to_owned(),
        }
    }
}

/// Liefert die Version des Gateways.
#[utoipa::path(
    get,
    path = "/admin/api/v1/version",
    tag = "system",
    responses((status = 200, description = "Version des Gateways", body = VersionInfo))
)]
async fn version() -> Json<VersionInfo> {
    Json(VersionInfo::current())
}

async fn openapi() -> impl IntoResponse {
    Json(ApiDoc::openapi())
}

pub(crate) fn router() -> Router {
    Router::new()
        .route("/admin/api/v1/version", get(version))
        .route("/admin/api/openapi.json", get(openapi))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openapi_contains_version_path() {
        let json = serde_json::to_value(ApiDoc::openapi()).unwrap();
        assert!(json["paths"]["/admin/api/v1/version"]["get"].is_object());
        assert!(json["components"]["schemas"]["VersionInfo"].is_object());
    }
}
