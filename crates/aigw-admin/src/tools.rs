// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Tool-Inventar und Tool-Richtlinien (AIGW-87, `docs/konzept/16-tools.md`).
//!
//! - `/admin/api/v1/tool-policies` – Ressource `ToolPolicy` (CRUD mit `If-Match`, Audit je
//!   Objekt, GitOps). Rechte wie Plugin-Bindungen (F183): lesen jede wirksame Rolle; schreiben
//!   je Ebene – `global`/`model-group` nur globaler `admin`, `organization` O auch `org-admin`
//!   von O, `team` T auch `team-admin` von T, `api-key` wie der Key.
//! - `/admin/api/v1/tools` – Inventar (von Anwendungen mitgeschickte Tools) mit Filtern und
//!   wirksamem Status (`new`, `approved`, `blocked`, `pending`), Markierungen `changed` und
//!   `suspicious`, Kennzeichen `gatewayMcp` (Herkunft ist ein MCP-Server des Gateways).
//!   Sichtbar ist, was im Bereich der Rolle liegt (Team bzw. teamlose Keys der Organisation).
//! - `/admin/api/v1/tools/{id}` – Detail mit Verlauf der Hashes; `…/approve`, `…/block`
//!   schreiben die Entscheidungs-Richtlinie der gewählten Ebene (`tools-allow-…`,
//!   `tools-block-…`); `…/acknowledge` bestätigt eine Änderung; `DELETE` entfernt den
//!   Eintrag.

// `#[utoipa::path(params(…))]` erzeugt für IntoParams-Typen qualifizierte Pfade.
#![allow(unused_qualifications)]

use std::collections::{HashMap, HashSet};

use aigw_core::resource::{Id, Object, ObjectMeta, OrganizationSpec, Scope, ScopeLevel, TeamSpec};
use aigw_core::tools::{
    Effect, Reason, ToolAction, ToolCtx, ToolPolicyIndex, ToolPolicySpec,
};
use aigw_store::ListFilter;
use aigw_store::tools::{ToolHistoryRow, ToolQuery, ToolRow, ToolVisibility};
use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use utoipa::{IntoParams, ToSchema};

use crate::AdminState;
use crate::auth::{Permission, Principal, Target};
use crate::problem::Problem;
use crate::resources::{
    AdminResource, ApiResult, ListArgs, St, create_impl, delete_impl, get_impl, json_with,
    list_impl, plain_resource, put_impl,
};

// ---------------------------------------------------------------------------------------------
// Richtlinien (Ressource)
// ---------------------------------------------------------------------------------------------

/// Filter für Tool-Richtlinien.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct ToolPolicyQuery {
    /// Seitengröße (1–500, Standard 50).
    pub limit: Option<u32>,
    /// Fortsetzung (`nextCursor` der vorigen Seite).
    pub cursor: Option<String>,
    /// Teilstring im Namen.
    pub q: Option<String>,
    /// Label-Selektor `k=v,k2`.
    pub label: Option<String>,
    /// Nur diese Ebene (`global`, `organization`, `team`, `api-key`, `model-group`).
    pub level: Option<String>,
    /// Nur dieses Ziel der Ebene (Name).
    pub target: Option<String>,
    /// Nur diese Aktion (`allow`, `block-strip`, `block-reject`, `observe`,
    /// `require-approval`).
    pub action: Option<String>,
}

plain_resource!(
    spec = ToolPolicySpec,
    res = ToolPolicy,
    input = ToolPolicyInput,
    list = ToolPolicyList,
    query = ToolPolicyQuery,
    tag = "tool-policies",
    path = "/admin/api/v1/tool-policies",
    item_path = "/admin/api/v1/tool-policies/{name}",
    what = "Tool-Richtlinie",
    handlers = [
        list_tool_policies,
        create_tool_policy,
        get_tool_policy,
        put_tool_policy,
        delete_tool_policy
    ]
);

impl AdminResource for ToolPolicySpec {
    type Res = ToolPolicy;
    type Query = ToolPolicyQuery;
    const COLLECTION: &'static str = "tool-policies";

    fn target(_name: &str, _spec: &Self) -> Target {
        Target::Catalog
    }

    async fn target_for(st: &AdminState, _name: &str, spec: &Self) -> Result<Target, Problem> {
        scope_target(st, &spec.scope).await
    }

    fn list_args(q: &Self::Query) -> ListArgs<'_> {
        ListArgs {
            limit: q.limit,
            cursor: q.cursor.as_deref(),
            q: q.q.as_deref(),
            label: q.label.as_deref(),
        }
    }

    fn list_filters(q: &Self::Query) -> Result<Vec<ListFilter>, Problem> {
        let mut f = Vec::new();
        f.extend(q.level.iter().map(|l| ListFilter::Level(l.clone())));
        f.extend(q.target.iter().map(|t| ListFilter::Target(t.clone())));
        f.extend(q.action.iter().map(|a| ListFilter::Action(a.clone())));
        Ok(f)
    }

    async fn check(
        _st: &AdminState,
        _name: &str,
        spec: &Self,
        _current: Option<&Object<Self>>,
    ) -> Result<(), Problem> {
        spec.check().map_err(Problem::invalid)
    }
}

/// RBAC-Ziel einer Ebene (wie Plugin-Bindungen, F183). Unbekanntes Ziel → `422`.
async fn scope_target(st: &AdminState, scope: &Scope) -> Result<Target, Problem> {
    let name = || {
        scope
            .target
            .clone()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| Problem::invalid("scope.target: Pflicht auf dieser Ebene"))
    };
    match scope.level {
        ScopeLevel::Global | ScopeLevel::ModelGroup => Ok(Target::Catalog),
        ScopeLevel::Organization => Ok(Target::InOrganization(name()?)),
        ScopeLevel::Team => {
            let t = name()?;
            match st.store.get::<TeamSpec>(&t).await {
                Ok(team) => Ok(Target::in_team(team.spec.organization, t)),
                Err(aigw_store::StoreError::NotFound { .. }) => Err(Problem::invalid(format!(
                    "scope.target: unbekanntes Team {t:?}"
                ))),
                Err(e) => Err(e.into()),
            }
        }
        ScopeLevel::ApiKey => {
            let k = name()?;
            match st.store.get_api_key(&k).await {
                Ok(key) => crate::resources::key_target(&st.store, &key.spec).await,
                Err(aigw_store::StoreError::NotFound { .. }) => Err(Problem::invalid(format!(
                    "scope.target: unbekannter API-Key {k:?}"
                ))),
                Err(e) => Err(e.into()),
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Inventar
// ---------------------------------------------------------------------------------------------

/// Wirksamer Status eines Tools im Bereich seines Keys (Richtlinien auf Modellgruppen-Ebene
/// bleiben unberücksichtigt).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ToolStatus {
    /// Keine Richtlinie greift (durchgelassen).
    New,
    /// Ausdrücklich erlaubt (`allow`).
    Approved,
    /// Gesperrt (`block-strip`/`block-reject`).
    Blocked,
    /// Wartet auf Freigabe (`require-approval`, wird entfernt bzw. abgelehnt).
    Pending,
}

impl ToolStatus {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "new" => Self::New,
            "approved" => Self::Approved,
            "blocked" => Self::Blocked,
            "pending" => Self::Pending,
            _ => return None,
        })
    }
}

/// Ein Tool im Inventar.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolItem {
    /// ID.
    #[schema(value_type = String, format = Uuid)]
    pub id: Id,
    /// Tool-Name (wie vom Client geschickt).
    pub name: String,
    /// Art: `function`, `builtin`, `server-tool`.
    pub kind: String,
    /// Herkunft: MCP-Server (aus `mcp__<server>__<tool>` bzw. `<server>__<tool>`) oder
    /// `client`.
    pub origin: String,
    /// Herkunft ist ein MCP-Server des Gateways (`/mcp`, Ressource `McpServer`).
    pub gateway_mcp: bool,
    /// Letzter Client (User-Agent bzw. Anwendungs-Header, gekürzt).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// Organisation (Name).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    /// Team (Name).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    /// API-Key (Name).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Erste Sichtung.
    #[schema(value_type = String, format = DateTime)]
    pub first_seen: String,
    /// Letzte Sichtung.
    #[schema(value_type = String, format = DateTime)]
    pub last_seen: String,
    /// Anfragen mit diesem Tool.
    pub requests: i64,
    /// Tool-Aufrufe des Modells.
    pub calls: i64,
    /// Blockiert (entfernt, abgelehnt oder Aufruf unterbunden).
    pub blocked: i64,
    /// Hash der Beschreibung (BLAKE3, 128 Bit, hex).
    pub description_hash: String,
    /// Hash des Schemas (BLAKE3, 128 Bit, hex).
    pub schema_hash: String,
    /// Beschreibung gespeichert (nur bei `payloadMode` `masked`/`full`).
    pub has_description: bool,
    /// Beschreibung/Schema seit der letzten Bestätigung geändert.
    pub changed: bool,
    /// Zeitpunkt der letzten Änderung.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub changed_at: Option<String>,
    /// Treffer des Prompt-Injection-Erkenners in der Beschreibung (leer = keine).
    pub suspicious: Vec<String>,
    /// Wirksamer Status.
    pub status: ToolStatus,
    /// Entscheidende Richtlinie.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
    /// Passende `observe`-Richtlinie.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_by: Option<String>,
}

/// Seite des Inventars.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolList {
    /// Einträge (nach Name sortiert).
    pub items: Vec<ToolItem>,
    /// Cursor der nächsten Seite.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Eintrag im Verlauf.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolHistoryItem {
    /// Zeitpunkt.
    #[schema(value_type = String, format = DateTime)]
    pub seen_at: String,
    /// `discovered` oder `changed`.
    pub event: String,
    /// Hash der Beschreibung.
    pub description_hash: String,
    /// Hash des Schemas.
    pub schema_hash: String,
    /// Beschreibung (falls gespeichert).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Client.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
}

/// Detail eines Tools.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolDetail {
    /// Tool.
    pub tool: ToolItem,
    /// Beschreibung (falls gespeichert).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON-Schema der Parameter (falls gespeichert).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub parameters: Option<serde_json::Value>,
    /// Verlauf (neueste zuerst, höchstens 50).
    pub history: Vec<ToolHistoryItem>,
    /// Richtlinien, deren Muster auf das Tool passen (Name, Ebene, Aktion).
    pub policies: Vec<MatchingPolicy>,
}

/// Passende Richtlinie.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MatchingPolicy {
    /// Name.
    pub name: String,
    /// Ebene und Ziel.
    pub scope: Scope,
    /// Aktion.
    pub action: ToolAction,
}

/// Freigeben bzw. Sperren eines Tools.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolDecisionInput {
    /// Ebene (Standard `global`). Mit `api-key`/`team`/`organization` ohne Ziel gilt das
    /// Ziel des Inventar-Eintrags.
    #[serde(default)]
    pub scope: Option<Scope>,
    /// Nur Sperren: `block-strip` (Standard) oder `block-reject`.
    #[serde(default)]
    pub action: Option<ToolAction>,
}

/// Ergebnis einer Entscheidung.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolDecision {
    /// Geschriebene Richtlinie.
    pub policy: String,
    /// Tool nach der Entscheidung.
    pub tool: ToolItem,
}

/// Filter des Inventars.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ToolInventoryQuery {
    /// Seitengröße (1–500, Standard 50).
    pub limit: Option<u32>,
    /// Fortsetzung (`nextCursor` der vorigen Seite).
    pub cursor: Option<String>,
    /// Teilstring im Tool-Namen.
    pub q: Option<String>,
    /// Herkunft (`client` oder MCP-Server).
    pub origin: Option<String>,
    /// Teilstring im Client.
    pub client: Option<String>,
    /// Art (`function`, `builtin`, `server-tool`).
    pub kind: Option<String>,
    /// Organisation (Name).
    pub organization: Option<String>,
    /// Team (Name).
    pub team: Option<String>,
    /// API-Key (Name).
    pub api_key: Option<String>,
    /// Status: `new`, `approved`, `blocked`, `pending`, `changed`, `suspicious`.
    pub status: Option<String>,
}

/// Namen und Beziehungen (je Anfrage aus dem Store).
struct Directory {
    orgs: HashMap<Id, String>,
    teams: HashMap<Id, (String, String)>,
    keys: HashMap<Id, String>,
    mcp_servers: HashSet<String>,
    index: ToolPolicyIndex,
    policies: Vec<Object<ToolPolicySpec>>,
}

impl Directory {
    async fn load(st: &AdminState) -> Result<Self, Problem> {
        let orgs = st
            .store
            .list::<OrganizationSpec>()
            .await?
            .into_iter()
            .map(|o| (o.id, o.metadata.name))
            .collect();
        let teams = st
            .store
            .list::<TeamSpec>()
            .await?
            .into_iter()
            .map(|t| (t.id, (t.metadata.name, t.spec.organization)))
            .collect();
        let keys = st
            .store
            .list_api_keys()
            .await?
            .into_iter()
            .map(|k| (k.id, k.metadata.name))
            .collect();
        let mcp_servers = st
            .store
            .list::<aigw_core::resource::McpServerSpec>()
            .await?
            .into_iter()
            .map(|s| s.metadata.name)
            .collect();
        let policies = st.store.list::<ToolPolicySpec>().await?;
        let index = ToolPolicyIndex::new(policies.iter());
        Ok(Self {
            orgs,
            teams,
            keys,
            mcp_servers,
            index,
            policies,
        })
    }

    fn target_of(&self, row: &ToolRow) -> Target {
        if let Some((team, org)) = row.team_id.and_then(|id| self.teams.get(&id)) {
            return Target::in_team(org.clone(), team.clone());
        }
        match row.organization_id.and_then(|id| self.orgs.get(&id)) {
            Some(o) if row.team_id.is_none() => Target::InOrganization(o.clone()),
            _ => Target::Global,
        }
    }

    fn visibility(&self, p: &Principal) -> Option<ToolVisibility> {
        if p.can(Permission::Read, &Target::Global) {
            return None;
        }
        Some(ToolVisibility {
            teams: self
                .teams
                .iter()
                .filter(|(_, (t, o))| p.can(Permission::Read, &Target::in_team(o.clone(), t.clone())))
                .map(|(id, _)| *id)
                .collect(),
            teamless_orgs: self
                .orgs
                .iter()
                .filter(|(_, o)| p.can(Permission::Read, &Target::InOrganization((*o).clone())))
                .map(|(id, _)| *id)
                .collect(),
        })
    }

    fn id_by<'a>(map: impl Iterator<Item = (&'a Id, &'a str)>, name: &str, what: &str) -> Result<Id, Problem> {
        let mut map = map;
        map.find(|(_, n)| *n == name)
            .map(|(id, _)| *id)
            .ok_or_else(|| Problem::invalid(format!("unbekannte(r) {what} {name:?}")))
    }

    fn item(&self, r: &ToolRow) -> ToolItem {
        let organization = r.organization_id.and_then(|i| self.orgs.get(&i)).cloned();
        let team = r.team_id.and_then(|i| self.teams.get(&i)).map(|(t, _)| t.clone());
        let api_key = r.api_key_id.and_then(|i| self.keys.get(&i)).cloned();
        let ctx = ToolCtx {
            organization: organization.as_deref(),
            team: team.as_deref(),
            api_key: api_key.as_deref(),
            model_group: None,
        };
        let v = self.index.decide(&ctx, &r.name);
        let status = match (v.effect, v.reason) {
            (Effect::Allow, Reason::Default) => ToolStatus::New,
            (Effect::Allow, _) => ToolStatus::Approved,
            (_, Reason::Unapproved) => ToolStatus::Pending,
            _ => ToolStatus::Blocked,
        };
        let gateway_mcp = self.mcp_servers.contains(&r.origin)
            || self.mcp_servers.iter().any(|s| r.name.contains(&format!("__{s}__")));
        ToolItem {
            id: r.id,
            name: r.name.clone(),
            kind: r.kind.clone(),
            origin: r.origin.clone(),
            gateway_mcp,
            client: r.client.clone(),
            organization,
            team,
            api_key,
            first_seen: rfc3339(r.first_seen_ms),
            last_seen: rfc3339(r.last_seen_ms),
            requests: r.requests,
            calls: r.calls,
            blocked: r.blocked,
            description_hash: r.description_hash.clone(),
            schema_hash: r.schema_hash.clone(),
            has_description: r.description.is_some(),
            changed: r.changed,
            changed_at: r.changed_at_ms.map(rfc3339),
            suspicious: r.suspicious.clone(),
            status,
            policy: v.policy.map(|p| p.to_string()),
            observed_by: v.observed.map(|p| p.to_string()),
        }
    }

    fn matching(&self, item: &ToolItem) -> Vec<MatchingPolicy> {
        self.policies
            .iter()
            .filter(|p| {
                let one = ToolPolicyIndex::new([*p].into_iter().map(|p| {
                    // Nur Muster prüfen: Ebene neutralisieren.
                    p
                }));
                let mut probe = (*p).clone();
                probe.spec.scope = Scope::global();
                probe.spec.enabled = true;
                drop(one);
                let idx = ToolPolicyIndex::new([&probe]);
                let v = idx.decide(&ToolCtx::default(), &item.name);
                v.policy.is_some() || v.observed.is_some()
            })
            .map(|p| MatchingPolicy {
                name: p.metadata.name.clone(),
                scope: p.spec.scope.clone(),
                action: p.spec.action,
            })
            .collect()
    }
}

fn rfc3339(ms: i64) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .ok()
        .and_then(|t| t.format(&Rfc3339).ok())
        .unwrap_or_default()
}

fn decode_cursor(c: &str) -> Result<(String, Id), Problem> {
    let (name, id) = c
        .rsplit_once('|')
        .ok_or_else(|| Problem::bad_request("cursor: ungültig"))?;
    let id = id
        .parse::<Id>()
        .map_err(|_| Problem::bad_request("cursor: ungültig"))?;
    Ok((name.to_owned(), id))
}

/// Listet das Tool-Inventar.
#[utoipa::path(
    get, path = "/admin/api/v1/tools", tag = "tools", params(ToolInventoryQuery),
    responses(
        (status = 200, description = "Seite", body = ToolList),
        (status = 400, description = "Ungültige Parameter", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Unbekannte Organisation, Team oder Key", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn list_tools(
    State(st): St,
    Extension(p): Extension<Principal>,
    q: Result<Query<ToolInventoryQuery>, QueryRejection>,
) -> ApiResult {
    let Query(q) = q?;
    let dir = Directory::load(&st).await?;
    let limit = q.limit.unwrap_or(crate::resources::DEFAULT_LIMIT);
    if limit == 0 || limit > crate::resources::MAX_LIMIT {
        return Err(Problem::bad_request(format!(
            "limit: 1–{}",
            crate::resources::MAX_LIMIT
        )));
    }
    let mut status = None;
    let mut sq = ToolQuery {
        limit,
        after: q.cursor.as_deref().map(decode_cursor).transpose()?,
        search: q.q.clone(),
        origin: q.origin.clone(),
        client: q.client.clone(),
        kind: q.kind.clone(),
        visibility: dir.visibility(&p),
        ..ToolQuery::default()
    };
    match q.status.as_deref() {
        None => {}
        Some("changed") => sq.changed = Some(true),
        Some("suspicious") => sq.suspicious = Some(true),
        Some(s) => {
            status = Some(ToolStatus::parse(s).ok_or_else(|| {
                Problem::bad_request(
                    "status: new, approved, blocked, pending, changed oder suspicious",
                )
            })?);
        }
    }
    if let Some(o) = &q.organization {
        sq.organization = Some(Directory::id_by(
            dir.orgs.iter().map(|(i, n)| (i, n.as_str())),
            o,
            "Organisation",
        )?);
    }
    if let Some(t) = &q.team {
        sq.team = Some(Directory::id_by(
            dir.teams.iter().map(|(i, (n, _))| (i, n.as_str())),
            t,
            "Team",
        )?);
    }
    if let Some(k) = &q.api_key {
        sq.api_key = Some(Directory::id_by(
            dir.keys.iter().map(|(i, n)| (i, n.as_str())),
            k,
            "API-Key",
        )?);
    }
    // Status-Filter (aus den Richtlinien berechnet) nach dem SQL: weiterlesen, bis die Seite
    // voll ist (höchstens 20 Runden).
    let mut items = Vec::new();
    let mut next = None;
    for _ in 0..20 {
        let rows = st.store.list_tools(&sq).await?;
        let more = rows.len() > limit as usize;
        let mut last = None;
        for r in rows.iter().take(limit as usize) {
            last = Some((r.name.clone(), r.id));
            let item = dir.item(r);
            if status.is_none_or(|s| s == item.status) {
                items.push(item);
                if items.len() == limit as usize {
                    break;
                }
            }
        }
        if items.len() == limit as usize {
            if more || last.as_ref() != rows.last().map(|r| (r.name.clone(), r.id)).as_ref() {
                next = items.last().map(|i| format!("{}|{}", i.name, i.id));
            }
            break;
        }
        if !more {
            break;
        }
        sq.after = last;
    }
    Ok(json_with(
        StatusCode::OK,
        None,
        &ToolList {
            items,
            next_cursor: next,
        },
    ))
}

async fn load_row(st: &AdminState, id: &str) -> Result<ToolRow, Problem> {
    let id: Id = id
        .parse()
        .map_err(|_| Problem::not_found(format!("Tool {id:?} nicht gefunden")))?;
    st.store
        .get_tool(id)
        .await?
        .ok_or_else(|| Problem::not_found(format!("Tool {id} nicht gefunden")))
}

fn history_item(h: ToolHistoryRow) -> ToolHistoryItem {
    ToolHistoryItem {
        seen_at: rfc3339(h.seen_at_ms),
        event: h.event,
        description_hash: h.description_hash,
        schema_hash: h.schema_hash,
        description: h.description,
        client: h.client,
    }
}

/// Liest ein Tool mit Verlauf der Hashes und passenden Richtlinien.
#[utoipa::path(
    get, path = "/admin/api/v1/tools/{id}", tag = "tools",
    params(("id" = String, Path, description = "ID des Inventar-Eintrags")),
    responses(
        (status = 200, description = "Gefunden", body = ToolDetail),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn get_tool(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult {
    let row = load_row(&st, &id).await?;
    let dir = Directory::load(&st).await?;
    p.require_on(Permission::Read, &dir.target_of(&row))?;
    let history = st
        .store
        .tool_history(row.id, 50)
        .await?
        .into_iter()
        .map(history_item)
        .collect();
    let tool = dir.item(&row);
    let policies = dir.matching(&tool);
    Ok(json_with(
        StatusCode::OK,
        None,
        &ToolDetail {
            tool,
            description: row.description.clone(),
            parameters: row
                .parameters
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok()),
            history,
            policies,
        },
    ))
}

/// Ebene einer Entscheidung: Standard global; ohne Ziel das des Eintrags.
fn decision_scope(input: &ToolDecisionInput, item: &ToolItem) -> Result<Scope, Problem> {
    let mut scope = input.scope.clone().unwrap_or_else(Scope::global);
    if scope.level != ScopeLevel::Global && scope.target.as_deref().is_none_or(str::is_empty) {
        scope.target = match scope.level {
            ScopeLevel::Organization => item.organization.clone(),
            ScopeLevel::Team => item.team.clone(),
            ScopeLevel::ApiKey => item.api_key.clone(),
            ScopeLevel::Global | ScopeLevel::ModelGroup => None,
        };
        if scope.target.is_none() {
            return Err(Problem::invalid(
                "scope.target: Pflicht (der Eintrag hat auf dieser Ebene kein Ziel)",
            ));
        }
    }
    Ok(scope)
}

async fn decide(
    st: &AdminState,
    p: &Principal,
    id: &str,
    input: ToolDecisionInput,
    approve: bool,
) -> ApiResult {
    let row = load_row(st, id).await?;
    let dir = Directory::load(st).await?;
    p.require_on(Permission::Read, &dir.target_of(&row))?;
    let item = dir.item(&row);
    let scope = decision_scope(&input, &item)?;
    p.require_on(Permission::Write, &scope_target(st, &scope).await?)?;
    let action = if approve {
        if input.action.is_some_and(|a| a != ToolAction::Allow) {
            return Err(Problem::invalid("action: beim Freigeben nur `allow`"));
        }
        ToolAction::Allow
    } else {
        match input.action.unwrap_or(ToolAction::BlockStrip) {
            a @ (ToolAction::BlockStrip | ToolAction::BlockReject) => a,
            _ => return Err(Problem::invalid("action: `block-strip` oder `block-reject`")),
        }
    };
    let policy = {
        let _guard = st.write_lock.lock().await;
        st.store
            .set_tool_decision(p.actor(), &scope, &row.name, action)
            .await?
    };
    if approve {
        st.store.acknowledge_tool(row.id).await?;
    }
    let row = load_row(st, id).await?;
    let dir = Directory::load(st).await?;
    Ok(json_with(
        StatusCode::OK,
        None,
        &ToolDecision {
            policy,
            tool: dir.item(&row),
        },
    ))
}

/// Gibt ein Tool frei (Richtlinie `tools-allow-<ebene>[-<ziel>]`) und bestätigt eine
/// Änderung.
#[utoipa::path(
    post, path = "/admin/api/v1/tools/{id}/approve", tag = "tools",
    params(("id" = String, Path, description = "ID des Inventar-Eintrags")),
    request_body = ToolDecisionInput,
    responses(
        (status = 200, description = "Freigegeben", body = ToolDecision),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Ungültige Ebene", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn approve_tool(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(id): Path<String>,
    body: Option<Json<ToolDecisionInput>>,
) -> ApiResult {
    decide(&st, &p, &id, body.map(|b| b.0).unwrap_or_default(), true).await
}

/// Sperrt ein Tool (Richtlinie `tools-block-<ebene>[-<ziel>]`, Standard `block-strip`).
#[utoipa::path(
    post, path = "/admin/api/v1/tools/{id}/block", tag = "tools",
    params(("id" = String, Path, description = "ID des Inventar-Eintrags")),
    request_body = ToolDecisionInput,
    responses(
        (status = 200, description = "Gesperrt", body = ToolDecision),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Ungültige Ebene oder Aktion", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn block_tool(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(id): Path<String>,
    body: Option<Json<ToolDecisionInput>>,
) -> ApiResult {
    decide(&st, &p, &id, body.map(|b| b.0).unwrap_or_default(), false).await
}

/// Bestätigt eine geänderte Beschreibung bzw. ein geändertes Schema (setzt `changed` zurück).
#[utoipa::path(
    post, path = "/admin/api/v1/tools/{id}/acknowledge", tag = "tools",
    params(("id" = String, Path, description = "ID des Inventar-Eintrags")),
    responses(
        (status = 200, description = "Bestätigt", body = ToolItem),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn acknowledge_tool(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult {
    let row = load_row(&st, &id).await?;
    let dir = Directory::load(&st).await?;
    p.require_on(Permission::Write, &dir.target_of(&row))?;
    st.store.acknowledge_tool(row.id).await?;
    let row = load_row(&st, &id).await?;
    Ok(json_with(StatusCode::OK, None, &dir.item(&row)))
}

/// Entfernt einen Eintrag samt Verlauf aus dem Inventar (taucht bei erneuter Nutzung wieder
/// als neu auf).
#[utoipa::path(
    delete, path = "/admin/api/v1/tools/{id}", tag = "tools",
    params(("id" = String, Path, description = "ID des Inventar-Eintrags")),
    responses(
        (status = 204, description = "Entfernt"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn delete_tool(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult {
    let row = load_row(&st, &id).await?;
    let dir = Directory::load(&st).await?;
    p.require_on(Permission::Write, &dir.target_of(&row))?;
    st.store.delete_tool(row.id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

use axum::response::IntoResponse as _;

/// Routen der Tool-Richtlinien und des Inventars (ohne Authentifizierungsschicht).
pub(crate) fn router() -> axum::Router<AdminState> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route(
            "/admin/api/v1/tool-policies",
            get(list_tool_policies).post(create_tool_policy),
        )
        .route(
            "/admin/api/v1/tool-policies/{name}",
            get(get_tool_policy)
                .put(put_tool_policy)
                .delete(delete_tool_policy),
        )
        .route("/admin/api/v1/tools", get(list_tools))
        .route("/admin/api/v1/tools/{id}", get(get_tool).delete(delete_tool))
        .route("/admin/api/v1/tools/{id}/approve", post(approve_tool))
        .route("/admin/api/v1/tools/{id}/block", post(block_tool))
        .route("/admin/api/v1/tools/{id}/acknowledge", post(acknowledge_tool))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_roundtrip() {
        let id = aigw_core::resource::new_id();
        let (n, i) = decode_cursor(&format!("mcp__a|b__c|{id}")).unwrap();
        assert_eq!((n.as_str(), i), ("mcp__a|b__c", id));
        assert!(decode_cursor("x").is_err());
    }
}
