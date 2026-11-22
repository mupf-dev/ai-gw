// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

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
use crate::mcp as m;
use crate::plugin_bindings as pb;
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
        r::delete_api_key, r::revoke_api_key, r::suspend_api_key, r::resume_api_key, r::rotate_api_key,
        r::list_model_groups, r::create_model_group, r::get_model_group,
        r::put_model_group, r::delete_model_group,
        d::list_deployments, d::create_deployment, d::get_deployment, d::put_deployment,
        d::delete_deployment,
        c::list_credentials, c::create_credential, c::get_credential, c::put_credential,
        c::patch_credential, c::delete_credential,
        crate::secrets::list_secrets, crate::secrets::create_secret,
        crate::secrets::get_secret, crate::secrets::put_secret, crate::secrets::patch_secret,
        crate::secrets::delete_secret,
        pr::list_prices, pr::create_price, pr::get_price, pr::put_price, pr::delete_price,
        crate::price_suggestions::list_suggestions, crate::price_suggestions::accept_suggestions,
        m::list_mcp_servers, m::create_mcp_server, m::get_mcp_server, m::put_mcp_server,
        m::delete_mcp_server,
        m::list_mcp_grants, m::create_mcp_grant, m::get_mcp_grant, m::put_mcp_grant,
        m::delete_mcp_grant,
        pb::list_plugin_bindings, pb::create_plugin_binding, pb::get_plugin_binding,
        pb::put_plugin_binding, pb::delete_plugin_binding,
        crate::tools::list_tool_policies, crate::tools::create_tool_policy,
        crate::tools::get_tool_policy, crate::tools::put_tool_policy,
        crate::tools::delete_tool_policy,
        crate::tools::list_tools, crate::tools::get_tool, crate::tools::approve_tool,
        crate::tools::block_tool, crate::tools::acknowledge_tool, crate::tools::delete_tool,
        crate::plugins::plugin_catalog, crate::plugins::test_plugin,
        crate::wasm_modules::list_wasm_modules, crate::wasm_modules::get_wasm_module,
        crate::wasm_modules::get_wasm_module_content, crate::wasm_modules::put_wasm_module,
        crate::wasm_modules::delete_wasm_module, crate::wasm_modules::approve_wasm_module,
        crate::wasm_modules::reject_wasm_module, crate::wasm_modules::get_pending_content,
        crate::guardrail_profiles::list_guardrail_profiles,
        crate::guardrail_profiles::apply_guardrail_profile,
        crate::gitops::export_config, crate::gitops::apply_config,
        s::login, s::callback, s::logout, s::session_info, s::auth_info,
        s::create_invitation, s::get_invitation, s::delete_invitation,
        ak::list_admin_api_keys, ak::create_admin_api_key, ak::get_admin_api_key,
        ak::put_admin_api_key, ak::delete_admin_api_key,
        crate::usage::list_budgets, crate::usage::limit_warnings_list,
        crate::audit::audit_log,
        crate::audit::verify_audit_log,
        crate::mcp_status::mcp_status,
        crate::providers::provider_overview,
        crate::analytics::timeseries, crate::analytics::summary, crate::analytics::top,
        crate::analytics::list_request_logs, crate::analytics::get_request_log,
        crate::analytics::get_request_payload,
        crate::costs::summary, crate::costs::breakdown, crate::costs::budgets,
        crate::costs::export,
        crate::playground::models, crate::playground::chat,
    ),
    components(schemas(
        VersionInfo, SnapshotInfo, Problem, crate::bounds::Cascade,
        crate::costs::ExportFormat, crate::costs::Granularity, crate::costs::ForecastMethod,
        crate::costs::CostExport, crate::costs::CostExportRow,
    )),
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
        (name = "providers", description = "Anbieter-Übersicht je Anbieter und Endpunkt: Anmeldeart, Deployments, Laufzeitzustand"),
        (name = "secrets", description = "Allgemeine Geheimnisse für Webhooks, Guardrail-Dienste und MCP-Server (Werte nur schreibbar)"),
        (name = "prices", description = "Preise je Anbieter-Modell, versioniert über validFrom; Preisvorschläge aus Azure Retail Prices und Preiskatalog"),
        (name = "mcp-servers", description = "Upstream-MCP-Server des MCP-Gateways"),
        (name = "mcp-grants", description = "Freigaben von MCP-Servern und -Tools für Teams und API-Keys"),
        (name = "plugin-bindings", description = "Plugin-Kette: Bindungen je Ebene (Guardrails, Webhooks, Header …) mit wirksamem Fehlermodus"),
        (name = "wasm-modules", description = "WASM-Module (WebAssembly-Komponenten) für Plugins der Art wasm: hochladen mit SHA-256, Signaturen (Ed25519, Sigstore), Vier-Augen-Freigabe, auflisten, löschen (AIGW-63, AIGW-101, AIGW-106)"),
        (name = "guardrail-profiles", description = "Standard-Guardrail-Profile (Basis, Streng, Offen) als Vorschläge: Inhalt ansehen, Diff berechnen, auf einen Scope anwenden"),
        (name = "tool-policies", description = "Tool-Richtlinien: Freigeben, Sperren, Beobachten von Tools, die Anwendungen mitschicken (AIGW-87)"),
        (name = "tools", description = "Tool-Inventar: von Anwendungen mitgeschickte Tools mit Herkunft, Nutzung, Hash-Verlauf und Status"),
        (name = "gitops", description = "YAML-Export und -Apply aller Ressourcen (aigw/v1alpha1)"),
        (name = "auth", description = "Anmeldung: OIDC-Login, Session, Abmeldung"),
        (name = "admin-api-keys", description = "Admin-API-Keys für Automatisierung"),
        (name = "usage", description = "Verbrauch und Budgetstände"),
        (name = "audit-log", description = "Audit-Log: Änderungen und Anmeldeereignisse, Export als JSON Lines"),
        (name = "analytics", description = "Dashboards: Zeitreihen, KPIs, Ranglisten aus den Rollups"),
        (name = "costs", description = "Kostenanalyse (FinOps): Kennzahlen, Kostentreiber, Budget-Prognosen, Chargeback-Export (CSV/JSON)"),
        (name = "request-logs", description = "Log-Explorer: einzelne Anfragen, Versuche, Payloads"),
        (name = "playground", description = "Playground der Admin-UI: Chat im Namen eines virtuellen Keys über die Pipeline der Datenebene (auditiert)"),
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
    /// Zustand des Konfigurations-Snapshots (F15, für das UI-Banner); fehlt ohne laufenden
    /// Gateway-Zustand (z. B. `aigw openapi`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SnapshotInfo>,
}

/// Zustand des aktiven Konfigurations-Snapshots (F15).
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotInfo {
    /// `config_version` des aktiven Snapshots.
    pub version: i64,
    /// Sekunden seit dem letzten erfolgreichen Abgleich mit der Datenbank (bei `source =
    /// cache`: seit dem Schreiben des Caches).
    pub age_seconds: u64,
    /// Herkunft: `database` oder `cache` (Kaltstart ohne Datenbank, F14).
    #[schema(example = "database")]
    pub source: String,
    /// Veraltet (Readiness `degraded`): aus dem Cache oder länger als
    /// `snapshot.stale_after_secs` nicht mit der Datenbank abgleichbar.
    pub stale: bool,
    /// Schlägt der Abgleich mit der Datenbank gerade fehl?
    pub failing: bool,
}

impl SnapshotInfo {
    /// Aus dem Halter des Snapshots.
    pub fn of(handle: &aigw_core::snapshot::SnapshotHandle) -> Self {
        let st = handle.status();
        Self {
            version: st.version,
            age_seconds: st.age.as_secs(),
            source: st.source.as_str().to_owned(),
            stale: st.stale,
            failing: st.failing,
        }
    }
}

impl VersionInfo {
    /// Version dieses Builds.
    pub fn current() -> Self {
        Self {
            name: aigw_core::NAME.to_owned(),
            version: aigw_core::VERSION.to_owned(),
            ui: crate::ui::KIND.to_owned(),
            snapshot: None,
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
async fn version(
    snapshot: Option<axum::Extension<aigw_core::snapshot::SnapshotHandle>>,
) -> Json<VersionInfo> {
    Json(VersionInfo {
        snapshot: snapshot.map(|axum::Extension(h)| SnapshotInfo::of(&h)),
        ..VersionInfo::current()
    })
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
            "/admin/api/v1/secrets/{name}",
            "/admin/api/v1/audit-log/verify",
            "/admin/api/v1/mcp-servers:status",
            "/admin/api/v1/providers",
            "/admin/api/v1/prices",
            "/admin/api/v1/mcp-servers/{name}",
            "/admin/api/v1/mcp-grants",
            "/admin/api/v1/plugin-bindings/{name}",
            "/admin/api/v1/playground/chat",
            "/admin/api/v1/playground/models",
            "/admin/api/v1/guardrail-profiles",
            "/admin/api/v1/guardrail-profiles/{name}:apply",
            "/admin/api/v1/tool-policies/{name}",
            "/admin/api/v1/tools",
            "/admin/api/v1/tools/{id}/approve",
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
            "ProviderOverview",
            "EntraAuth",
            "Secret",
            "SecretPatch",
            "AuditChainReport",
            "PriceList",
            "McpServerInput",
            "McpGrantList",
            "PluginBindingList",
            "EffectivePluginControl",
            "GuardrailProfileList",
            "GuardrailProfileApplyResult",
            "ToolPolicyList",
            "ToolDetail",
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
            "/admin/api/v1/audit-log",
            "/admin/api/v1/costs/summary",
            "/admin/api/v1/costs/breakdown",
            "/admin/api/v1/costs/budgets",
            "/admin/api/v1/costs/export",
        ] {
            assert!(json["paths"][p].is_object(), "Pfad {p} fehlt");
        }
        assert!(json["components"]["schemas"]["AuditEntry"].is_object());
        assert!(json["components"]["schemas"]["AuditLogPage"].is_object());
        let content =
            &json["paths"]["/admin/api/v1/audit-log"]["get"]["responses"]["200"]["content"];
        assert!(content["application/x-ndjson"].is_object(), "{content}");
        let content =
            &json["paths"]["/admin/api/v1/costs/export"]["get"]["responses"]["200"]["content"];
        assert!(content["text/csv"].is_object(), "{content}");
        // Ressourcen akzeptieren Bearer und Session.
        let sec = json["paths"]["/admin/api/v1/teams"]["post"]["security"].to_string();
        assert!(sec.contains("bearer") && sec.contains("session"), "{sec}");
    }
}
