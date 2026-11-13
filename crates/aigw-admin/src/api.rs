// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Admin-API `v1`.

use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi, ToSchema};

use crate::problem::Problem;
use crate::resources as r;

/// OpenAPI-Dokument der Admin-API.
#[derive(Debug, OpenApi)]
#[openapi(
    info(title = "ai-gw Admin-API", description = "Verwaltungs-API von ai-gw"),
    paths(
        version,
        r::list_organizations, r::create_organization, r::get_organization,
        r::put_organization, r::delete_organization,
        r::list_teams, r::create_team, r::get_team, r::put_team, r::delete_team,
        r::list_users, r::create_user, r::get_user, r::put_user, r::delete_user,
        r::list_api_keys, r::create_api_key, r::get_api_key, r::put_api_key,
        r::delete_api_key, r::revoke_api_key, r::reactivate_api_key, r::rotate_api_key,
        r::list_model_groups, r::get_model_group,
        crate::gitops::export_config, crate::gitops::apply_config,
    ),
    components(schemas(VersionInfo, Problem)),
    modifiers(&BearerAuth),
    tags(
        (name = "system", description = "Systeminformationen"),
        (name = "organizations", description = "Organisationen (oberste Mandantenebene)"),
        (name = "teams", description = "Teams mit Modellfreigaben, Limits und Budgets"),
        (name = "users", description = "Nutzer (Login per OIDC folgt mit AIGW-32)"),
        (name = "api-keys", description = "Virtuelle API-Keys der Datenebene"),
        (name = "model-groups", description = "Modellgruppen (lesend, für Freigaben)"),
        (name = "gitops", description = "YAML-Export und -Apply aller Ressourcen (aigw/v1alpha1)"),
    )
)]
pub struct ApiDoc;

/// Sicherheitsschema `bearer` (vorläufig Bootstrap-Admin-Token, später OIDC/Admin-Keys).
#[derive(Debug)]
struct BearerAuth;

impl Modify for BearerAuth {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "bearer",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some(
                        "Admin-Token (`Authorization: Bearer …`); vorläufig das Bootstrap-Token",
                    ))
                    .build(),
            ),
        );
    }
}

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
        for p in [
            "/admin/api/v1/organizations",
            "/admin/api/v1/teams/{name}",
            "/admin/api/v1/users",
            "/admin/api/v1/api-keys/{name}/rotate",
            "/admin/api/v1/model-groups",
            "/admin/api/v1/config:export",
            "/admin/api/v1/config:apply",
        ] {
            assert!(json["paths"][p].is_object(), "Pfad {p} fehlt");
        }
        for s in [
            "Problem",
            "Organization",
            "TeamInput",
            "ApiKeyList",
            "CreatedApiKey",
            "RotatedApiKey",
        ] {
            assert!(
                json["components"]["schemas"][s].is_object(),
                "Schema {s} fehlt"
            );
        }
        assert!(json["components"]["securitySchemes"]["bearer"].is_object());
    }
}
