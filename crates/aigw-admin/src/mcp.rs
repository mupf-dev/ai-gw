// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! MCP-Gateway (AIGW-41): Upstream-MCP-Server und Freigaben.
//!
//! - `/admin/api/v1/mcp-servers` – Katalog (lesen: jede wirksame Rolle, schreiben: globaler
//!   `admin`). Prüfung wie in GitOps über [`aigw_core::mcp::validate_server`]: URL gegen die
//!   SSRF-Regeln ([`aigw_core::upstream`]), Name ohne `__`, Einstellungen; Zugangsdaten bzw.
//!   Secret müssen existieren und das Token-Feld enthalten (`422` mit `issues`, AIGW-90).
//! - `/admin/api/v1/mcp-grants` – Freigaben für ein Team (Geltungsbereich: das Team, schreiben
//!   also Org-/globaler Admin) oder einen API-Key (Geltungsbereich: im Team des Keys, schreiben
//!   also auch Team-Admins; die Team-Freigabe bleibt Obergrenze, siehe
//!   `docs/konzept/15-mcp-gateway.md`). Keys ohne Team: global.

// `#[utoipa::path(params(…))]` erzeugt für IntoParams-Typen qualifizierte Pfade.
#![allow(unused_qualifications)]

use std::collections::HashMap;

use aigw_core::resource::{
    Id, McpGrantSpec, McpServerSpec, McpTransport, Object, ObjectMeta, ResourceKind, ScopeLevel,
    TeamSpec,
};
use aigw_store::{ListFilter, StoreError};
use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse as _;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::AdminState;
use crate::auth::{Permission, Principal, Target};
use crate::problem::Problem;
use crate::resources::{
    AdminResource, ApiResult, BASE, ListArgs, St, check_if_match, create_impl, created_at,
    delete_impl, etag_of, get_impl, json_with, list_impl, list_query, page_parts, plain_resource,
    put_impl, read_scope, same_name,
};

// ---------------------------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------------------------

/// Filter für MCP-Server.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query, rename_all = "camelCase")]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct McpServerQuery {
    /// Seitengröße (1–500, Standard 50).
    pub limit: Option<u32>,
    /// Fortsetzung (`nextCursor` der vorigen Seite).
    pub cursor: Option<String>,
    /// Teilstring im Namen oder in der URL.
    pub q: Option<String>,
    /// Label-Selektor `k=v,k2`.
    pub label: Option<String>,
    /// Nur dieser Transport.
    pub transport: Option<McpTransport>,
    /// Nur Server mit diesen Zugangsdaten (Name).
    pub credential: Option<String>,
    /// Nur aktive (`true`) bzw. deaktivierte (`false`).
    pub enabled: Option<bool>,
}

plain_resource!(
    spec = McpServerSpec,
    res = McpServer,
    input = McpServerInput,
    list = McpServerList,
    query = McpServerQuery,
    tag = "mcp-servers",
    path = "/admin/api/v1/mcp-servers",
    item_path = "/admin/api/v1/mcp-servers/{name}",
    what = "MCP-Server",
    handlers = [
        list_mcp_servers,
        create_mcp_server,
        get_mcp_server,
        put_mcp_server,
        delete_mcp_server
    ]
);

impl AdminResource for McpServerSpec {
    type Res = McpServer;
    type Query = McpServerQuery;
    const COLLECTION: &'static str = "mcp-servers";

    fn target(_name: &str, _spec: &Self) -> Target {
        Target::Catalog
    }

    fn list_args(q: &Self::Query) -> ListArgs<'_> {
        ListArgs {
            limit: q.limit,
            cursor: q.cursor.as_deref(),
            q: q.q.as_deref(), // auch die URL (Suchspalte im Store)
            label: q.label.as_deref(),
        }
    }

    fn list_filters(q: &Self::Query) -> Result<Vec<ListFilter>, Problem> {
        let mut f = Vec::new();
        if let Some(t) = q.transport {
            match serde_json::to_value(t) {
                Ok(serde_json::Value::String(s)) => f.push(ListFilter::Transport(s)),
                _ => return Err(Problem::bad_request("transport: unbekannter Wert")),
            }
        }
        f.extend(
            q.credential
                .iter()
                .map(|c| ListFilter::Credential(c.clone())),
        );
        f.extend(q.enabled.map(ListFilter::Enabled));
        Ok(f)
    }

    async fn check(
        st: &AdminState,
        name: &str,
        spec: &Self,
        _current: Option<&Object<Self>>,
    ) -> Result<(), Problem> {
        aigw_core::mcp::validate_server(&st.upstreams, name, spec).map_err(Problem::invalid)?;
        // AIGW-90: Verweis muss existieren und das Token-Feld liefern → sonst 422 mit `issues`.
        let invalid = |message: String| {
            Problem::invalid(message.clone()).with_issues(vec![aigw_store::gitops::Issue {
                document: None,
                kind: Some(ResourceKind::McpServer),
                name: Some(name.to_owned()),
                message,
                line: None,
                column: None,
            }])
        };
        let missing = |e: StoreError, what: String| match e {
            StoreError::NotFound { .. } => invalid(format!("{what} existiert nicht")),
            other => other.into(),
        };
        let fields = if let Some(cred) = &spec.credential {
            st.store
                .get_credential(cred)
                .await
                .map_err(|e| missing(e, format!("spec.credential: Zugangsdaten {cred:?}")))?
                .spec
                .fields
        } else if let Some(secret) = &spec.secret {
            st.store
                .get_secret(secret)
                .await
                .map_err(|e| missing(e, format!("spec.secret: Secret {secret:?}")))?
                .spec
                .fields
        } else {
            return Ok(());
        };
        aigw_core::mcp::check_reference_fields(spec, fields.iter().map(String::as_str))
            .map_err(invalid)
    }
}

// ---------------------------------------------------------------------------------------------
// Freigaben
// ---------------------------------------------------------------------------------------------

/// MCP-Freigabe (gelesen).
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpGrant {
    /// ID (UUIDv7).
    #[schema(value_type = String, format = Uuid)]
    pub id: Id,
    /// Metadaten.
    pub metadata: ObjectMeta,
    /// Spezifikation.
    pub spec: McpGrantSpec,
}

impl From<Object<McpGrantSpec>> for McpGrant {
    fn from(o: Object<McpGrantSpec>) -> Self {
        Self {
            id: o.id,
            metadata: o.metadata,
            spec: o.spec,
        }
    }
}

/// MCP-Freigabe (Eingabe für Anlegen und Ersetzen).
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpGrantInput {
    /// Metadaten (`name`, `labels`; vom Server gepflegte Felder werden ignoriert).
    pub metadata: ObjectMeta,
    /// Spezifikation.
    pub spec: McpGrantSpec,
}

/// Seite einer Liste: MCP-Freigaben.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpGrantList {
    /// Einträge (nach Name sortiert).
    pub items: Vec<McpGrant>,
    /// Cursor der nächsten Seite (`?cursor=`), fehlt auf der letzten Seite.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Filter für MCP-Freigaben.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query, rename_all = "camelCase")]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct McpGrantQuery {
    /// Seitengröße (1–500, Standard 50).
    pub limit: Option<u32>,
    /// Fortsetzung (`nextCursor` der vorigen Seite).
    pub cursor: Option<String>,
    /// Teilstring im Namen.
    pub q: Option<String>,
    /// Label-Selektor `k=v,k2`.
    pub label: Option<String>,
    /// Nur Freigaben für diesen Server.
    pub server: Option<String>,
    /// Nur Freigaben für dieses Team.
    pub team: Option<String>,
    /// Nur Freigaben für diesen API-Key.
    pub api_key: Option<String>,
}

/// Team → Organisation und API-Key → Team (für die Rollenprüfung).
struct Scopes {
    team_orgs: HashMap<String, String>,
    /// API-Key → (Team, Organisation).
    key_teams: HashMap<String, (Option<String>, String)>,
}

impl Scopes {
    async fn load(st: &AdminState) -> Result<Self, Problem> {
        let team_orgs = st
            .store
            .list::<TeamSpec>()
            .await?
            .into_iter()
            .map(|t| (t.metadata.name, t.spec.organization))
            .collect();
        let key_teams = st
            .store
            .list_api_keys()
            .await?
            .into_iter()
            .map(|k| (k.metadata.name, (k.spec.team, k.spec.organization)))
            .collect();
        Ok(Self {
            team_orgs,
            key_teams,
        })
    }

    /// Geltungsbereich einer Freigabe; unbekanntes Team/Key → `422`.
    fn target(&self, spec: &McpGrantSpec) -> Result<Target, Problem> {
        let name = spec.subject.target.clone().unwrap_or_default();
        let in_team = |team: &str| {
            Target::in_team(
                self.team_orgs.get(team).cloned().unwrap_or_default(),
                team.to_owned(),
            )
        };
        match spec.subject.level {
            ScopeLevel::Team => match self.team_orgs.get(&name) {
                Some(org) => Ok(Target::team(org.clone(), name)),
                None => Err(Problem::invalid(format!(
                    "subject.target: unbekanntes Team {name:?}"
                ))),
            },
            ScopeLevel::ApiKey => match self.key_teams.get(&name) {
                Some((Some(team), _)) => Ok(in_team(team)),
                // F193: Keys ohne Team liegen direkt in ihrer Organisation.
                Some((None, org)) => Ok(Target::InOrganization(org.clone())),
                None => Err(Problem::invalid(format!(
                    "subject.target: unbekannter API-Key {name:?}"
                ))),
            },
            _ => Err(Problem::invalid(
                "subject.level: nur `team` oder `api-key` erlaubt",
            )),
        }
    }
}

fn grant_etag(g: &McpGrant) -> String {
    etag_of(g)
}

async fn check_grant(st: &AdminState, spec: &McpGrantSpec) -> Result<(), Problem> {
    aigw_core::mcp::validate_grant(spec).map_err(Problem::invalid)?;
    // Unbekannter Server → 422.
    st.store
        .get::<McpServerSpec>(&spec.server)
        .await
        .map_err(|e| match e {
            StoreError::NotFound { kind, name } => StoreError::InvalidReference { kind, name },
            other => other,
        })?;
    Ok(())
}

/// Listet MCP-Freigaben (nur im lesbaren Geltungsbereich).
#[utoipa::path(
    get, path = "/admin/api/v1/mcp-grants", tag = "mcp-grants", params(McpGrantQuery),
    responses(
        (status = 200, description = "Seite", body = McpGrantList),
        (status = 400, description = "Ungültige Parameter", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn list_mcp_grants(
    State(st): St,
    Extension(p): Extension<Principal>,
    q: Result<Query<McpGrantQuery>, QueryRejection>,
) -> ApiResult {
    let Query(q) = q?;
    // Filter und Geltungsbereich (Team bzw. Team des Keys) laufen in SQL (F42).
    let mut filters = Vec::new();
    filters.extend(q.server.iter().map(|s| ListFilter::Server(s.clone())));
    filters.extend(
        q.team
            .iter()
            .map(|t| ListFilter::Subject(ScopeLevel::Team, t.clone())),
    );
    filters.extend(
        q.api_key
            .iter()
            .map(|k| ListFilter::Subject(ScopeLevel::ApiKey, k.clone())),
    );
    let args = ListArgs {
        limit: q.limit,
        cursor: q.cursor.as_deref(),
        q: q.q.as_deref(),
        label: q.label.as_deref(),
    };
    let lq = list_query(&args, filters, read_scope(&p))?;
    let (items, next_cursor) =
        page_parts::<_, McpGrant>(st.store.list_page::<McpGrantSpec>(&lq).await?);
    Ok(json_with(
        StatusCode::OK,
        None,
        &McpGrantList { items, next_cursor },
    ))
}

/// Legt eine MCP-Freigabe an.
#[utoipa::path(
    post, path = "/admin/api/v1/mcp-grants", tag = "mcp-grants", request_body = McpGrantInput,
    responses(
        (status = 201, description = "Angelegt", body = McpGrant,
            headers(("ETag" = String, description = "Version"), ("Location" = String, description = "Pfad"))),
        (status = 400, description = "Ungültige Anfrage", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Name bereits vergeben", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Ungültige Eingabe oder Verweis", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn create_mcp_grant(
    State(st): St,
    Extension(p): Extension<Principal>,
    body: Result<Json<McpGrantInput>, JsonRejection>,
) -> ApiResult {
    let Json(input) = body?;
    aigw_core::mcp::validate_grant(&input.spec).map_err(Problem::invalid)?;
    let target = Scopes::load(&st).await?.target(&input.spec)?;
    p.require_on(Permission::Write, &target)?;
    let _guard = st.write_lock.lock().await;
    check_grant(&st, &input.spec).await?;
    let obj = st
        .store
        .create(p.actor(), input.metadata, input.spec)
        .await?;
    let g = McpGrant::from(obj);
    let tag = grant_etag(&g);
    let location = format!("{BASE}/mcp-grants/{}", g.metadata.name);
    Ok(created_at(
        json_with(StatusCode::CREATED, Some(tag), &g),
        &location,
    ))
}

/// Liest eine MCP-Freigabe.
#[utoipa::path(
    get, path = "/admin/api/v1/mcp-grants/{name}", tag = "mcp-grants",
    params(("name" = String, Path, description = "Name")),
    responses(
        (status = 200, description = "Gefunden", body = McpGrant,
            headers(("ETag" = String, description = "Version für If-Match"))),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn get_mcp_grant(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
) -> ApiResult {
    let g = McpGrant::from(st.store.get::<McpGrantSpec>(&name).await?);
    let target = Scopes::load(&st).await?.target(&g.spec)?;
    p.require_on(Permission::Read, &target)?;
    Ok(json_with(StatusCode::OK, Some(grant_etag(&g)), &g))
}

/// Ersetzt Labels und Spezifikation einer MCP-Freigabe.
#[utoipa::path(
    put, path = "/admin/api/v1/mcp-grants/{name}", tag = "mcp-grants", request_body = McpGrantInput,
    params(
        ("name" = String, Path, description = "Name"),
        ("If-Match" = String, Header, description = "ETag aus dem letzten Lesen oder `*` (Pflicht, F38; fehlt er: 428)"),
    ),
    responses(
        (status = 200, description = "Geändert", body = McpGrant,
            headers(("ETag" = String, description = "Neue Version"))),
        (status = 400, description = "Ungültige Anfrage", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "ETag passt nicht", body = Problem, content_type = "application/problem+json"),
        (status = 428, description = "If-Match fehlt (F38)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Ungültige Eingabe oder Verweis", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn put_mcp_grant(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Result<Json<McpGrantInput>, JsonRejection>,
) -> ApiResult {
    let Json(input) = body?;
    let meta = same_name(&name, input.metadata)?;
    aigw_core::mcp::validate_grant(&input.spec).map_err(Problem::invalid)?;
    let scopes = Scopes::load(&st).await?;
    // Neuer und bisheriger Geltungsbereich (Freigabe nicht in fremde Teams verschieben).
    p.require_on(Permission::Write, &scopes.target(&input.spec)?)?;
    let _guard = st.write_lock.lock().await;
    let current = McpGrant::from(st.store.get::<McpGrantSpec>(&name).await?);
    p.require_on(Permission::Write, &scopes.target(&current.spec)?)?;
    let expect = check_if_match(&headers, &grant_etag(&current), current.metadata.updated_at)?;
    check_grant(&st, &input.spec).await?;
    let g = McpGrant::from(
        st.store
            .update_with(
                p.actor(),
                meta,
                input.spec,
                &aigw_store::WriteExtras::default().expecting(expect),
            )
            .await?,
    );
    Ok(json_with(StatusCode::OK, Some(grant_etag(&g)), &g))
}

/// Löscht eine MCP-Freigabe.
#[utoipa::path(
    delete, path = "/admin/api/v1/mcp-grants/{name}", tag = "mcp-grants",
    params(
        ("name" = String, Path, description = "Name"),
        ("If-Match" = String, Header, description = "ETag aus dem letzten Lesen oder `*` (Pflicht, F38; fehlt er: 428)"),
    ),
    responses(
        (status = 204, description = "Gelöscht"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "ETag passt nicht", body = Problem, content_type = "application/problem+json"),
        (status = 428, description = "If-Match fehlt (F38)", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn delete_mcp_grant(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> ApiResult {
    let _guard = st.write_lock.lock().await;
    let current = McpGrant::from(st.store.get::<McpGrantSpec>(&name).await?);
    // Verwaiste Freigaben (Team/Key gelöscht) darf ein globaler Admin entfernen.
    let target = Scopes::load(&st)
        .await?
        .target(&current.spec)
        .unwrap_or(Target::Global);
    p.require_on(Permission::Write, &target)?;
    let expect = check_if_match(&headers, &grant_etag(&current), current.metadata.updated_at)?;
    st.store
        .delete_expect(
            p.actor(),
            aigw_core::resource::ResourceKind::McpGrant,
            &name,
            expect,
        )
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Routen der MCP-Ressourcen (ohne Authentifizierungsschicht).
pub(crate) fn router() -> axum::Router<AdminState> {
    use axum::routing::get;
    axum::Router::new()
        .route(
            "/admin/api/v1/mcp-servers",
            get(list_mcp_servers).post(create_mcp_server),
        )
        .route(
            "/admin/api/v1/mcp-servers/{name}",
            get(get_mcp_server)
                .put(put_mcp_server)
                .delete(delete_mcp_server),
        )
        .route(
            "/admin/api/v1/mcp-grants",
            get(list_mcp_grants).post(create_mcp_grant),
        )
        .route(
            "/admin/api/v1/mcp-grants/{name}",
            get(get_mcp_grant)
                .put(put_mcp_grant)
                .delete(delete_mcp_grant),
        )
}
