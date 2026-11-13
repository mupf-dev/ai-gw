// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Admin-API `v1`.

use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use utoipa::openapi::security::{
    ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityRequirement, SecurityScheme,
};
use utoipa::{Modify, OpenApi, ToSchema};

use crate::credentials as c;
use crate::deployments as d;
use crate::prices as pr;
use crate::problem::Problem;
use crate::resources as r;
use crate::{admin_keys as ak, session as s};

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
        r::list_model_groups, r::create_model_group, r::get_model_group,
        r::put_model_group, r::delete_model_group,
        d::list_deployments, d::create_deployment, d::get_deployment, d::put_deployment,
        d::delete_deployment,
        c::list_credentials, c::create_credential, c::get_credential, c::put_credential,
        c::patch_credential, c::delete_credential,
        pr::list_prices, pr::create_price, pr::get_price, pr::put_price, pr::delete_price,
        crate::gitops::export_config, crate::gitops::apply_config,
        s::login, s::callback, s::logout, s::session_info, s::auth_info,
        ak::list_admin_api_keys, ak::create_admin_api_key, ak::get_admin_api_key,
        ak::put_admin_api_key, ak::delete_admin_api_key,
    ),
    components(schemas(VersionInfo, Problem)),
    modifiers(&SecuritySchemes),
    tags(
        (name = "system", description = "Systeminformationen"),
        (name = "organizations", description = "Organisationen (oberste Mandantenebene)"),
        (name = "teams", description = "Teams mit Modellfreigaben, Limits und Budgets"),
        (name = "users", description = "Nutzer (Login per OIDC, Rollen)"),
        (name = "api-keys", description = "Virtuelle API-Keys der Datenebene"),
        (name = "model-groups", description = "Modellgruppen (öffentliche Modellnamen, Aliasse, Routing, Fallbacks)"),
        (name = "deployments", description = "Deployments: Anbieter-Modelle hinter Modellgruppen"),
        (name = "provider-credentials", description = "Zugangsdaten der Anbieter (Werte nur schreibbar)"),
        (name = "prices", description = "Preise je Anbieter-Modell, versioniert über validFrom"),
        (name = "gitops", description = "YAML-Export und -Apply aller Ressourcen (aigw/v1alpha1)"),
        (name = "auth", description = "Anmeldung: OIDC-Login, Session, Abmeldung"),
        (name = "admin-api-keys", description = "Admin-API-Keys für Automatisierung"),
    )
)]
pub struct ApiDoc;

/// Sicherheitsschemata: `bearer` (Admin-API-Key oder Bootstrap-Token) und `session`
/// (Cookie aus dem OIDC-Login; schreibende Anfragen zusätzlich mit `X-CSRF-Token`).
/// Jede Operation, die `bearer` verlangt, akzeptiert alternativ `session`.
#[derive(Debug)]
struct SecuritySchemes;

impl Modify for SecuritySchemes {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "bearer",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some(
                        "`Authorization: Bearer …` mit einem Admin-API-Key (`sk-aigw-admin-…`) \
                         oder – nur bei der Erstinstallation – dem Bootstrap-Token",
                    ))
                    .build(),
            ),
        );
        components.add_security_scheme(
            "session",
            SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::with_description(
                crate::session::SESSION_COOKIE,
                "Session-Cookie nach dem OIDC-Login (`/admin/auth/login`). Schreibende Anfragen \
                 (POST/PUT/PATCH/DELETE) brauchen zusätzlich den Header `X-CSRF-Token` mit dem \
                 Wert `csrfToken` aus `GET /admin/api/v1/session`.",
            ))),
        );
        let session = SecurityRequirement::new("session", Vec::<String>::new());
        for item in openapi.paths.paths.values_mut() {
            for op in [
                &mut item.get,
                &mut item.put,
                &mut item.post,
                &mut item.delete,
                &mut item.patch,
            ]
            .into_iter()
            .flatten()
            {
                if let Some(sec) = op.security.as_mut() {
                    let has_bearer = serde_json::to_value(&*sec)
                        .ok()
                        .is_some_and(|v| v.to_string().contains("\"bearer\""));
                    let has_session = serde_json::to_value(&*sec)
                        .ok()
                        .is_some_and(|v| v.to_string().contains("\"session\""));
                    if has_bearer && !has_session {
                        sec.push(session.clone());
                    }
                }
            }
        }
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
            "/admin/api/v1/deployments/{name}",
            "/admin/api/v1/provider-credentials/{name}",
            "/admin/api/v1/prices",
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
            "ModelGroupInput",
            "DeploymentInput",
            "ProviderCredential",
            "ProviderCredentialPatch",
            "PriceList",
        ] {
            assert!(
                json["components"]["schemas"][s].is_object(),
                "Schema {s} fehlt"
            );
        }
        assert!(json["components"]["securitySchemes"]["bearer"].is_object());
        assert_eq!(
            json["components"]["securitySchemes"]["session"]["in"],
            "cookie"
        );
        for p in [
            "/admin/auth/login",
            "/admin/auth/callback",
            "/admin/auth/logout",
            "/admin/api/v1/session",
            "/admin/api/v1/auth/info",
            "/admin/api/v1/admin-api-keys",
            "/admin/api/v1/admin-api-keys/{name}",
        ] {
            assert!(json["paths"][p].is_object(), "Pfad {p} fehlt");
        }
        // Ressourcen akzeptieren Bearer und Session.
        let sec = json["paths"]["/admin/api/v1/teams"]["post"]["security"].to_string();
        assert!(sec.contains("bearer") && sec.contains("session"), "{sec}");
    }
}
