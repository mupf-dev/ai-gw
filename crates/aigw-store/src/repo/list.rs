// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Listen mit echter SQL-Pagination (F42, AIGW-73).
//!
//! Sortiert wird nach Name, fortgesetzt per **Keyset-Cursor** (`name > $cursor`, Name des
//! letzten Eintrags der vorigen Seite). Suche (`?q=`), Label-Selektor, ressourcenspezifische
//! Filter ([`ListFilter`]) und der lesbare Geltungsbereich des Aufrufers (Organisationen/Teams,
//! wie im Audit-Log: [`AuditScope`]) stehen im `WHERE` – die Datenbank liefert höchstens
//! `limit + 1` Zeilen, unabhängig von der Gesamtzahl.
//!
//! Geltungsbereich je Art: `Organization` (eigener Name), `Team` (Organisation bzw. Team),
//! `ApiKey` (Team des Keys; Keys ohne Team nur mit [`AuditScope::All`]), `McpGrant` (Team bzw.
//! Team des Keys). Alle anderen Arten sind entweder Katalog (der Aufrufer prüft das Leserecht
//! vorher und übergibt `All`) oder global (`Within` liefert dann nichts).

use aigw_core::resource::{
    ApiKey, ApiKeySpec, Object, ProviderCredential, ResourceKind, ScopeLevel,
};
use time::OffsetDateTime;

use super::{Resource, table};
use crate::db::{Q, Val};
use crate::repo::audit::{AuditScope, ScopeRule};
use crate::{Backend, Store, StoreError};

/// Ressourcenspezifischer Listenfilter (gilt nur für Arten mit passender Spalte, sonst
/// `StoreError::Invalid`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ListFilter {
    /// Organisation (Name): Teams, API-Keys.
    Organization(String),
    /// Team (Name): API-Keys.
    Team(String),
    /// Nutzer (Name): API-Keys.
    User(String),
    /// Modellgruppe (Name): Deployments.
    ModelGroup(String),
    /// Zugangsdaten (Name): Deployments, MCP-Server.
    Credential(String),
    /// MCP-Server (Name): MCP-Freigaben.
    Server(String),
    /// Subjekt einer MCP-Freigabe (Ebene `team`/`api-key` + Name).
    Subject(ScopeLevel, String),
    /// Anbieter (Wert der Spalte, bei Preisen `""` = alle Anbieter): Deployments,
    /// Zugangsdaten, Preise.
    Provider(String),
    /// Modell (exakt): Preise.
    Model(String),
    /// Transport (`streamable-http` …): MCP-Server.
    Transport(String),
    /// Aktiv/inaktiv: Deployments, MCP-Server.
    Enabled(bool),
    /// Zustand (`active`, `revoked` …): API-Keys.
    State(String),
    /// E-Mail (ohne Beachtung der Groß-/Kleinschreibung): Nutzer.
    Email(String),
    /// Nur die zum Zeitpunkt gültige Version je (Anbieter, Modell): Preise.
    EffectiveAt(OffsetDateTime),
    /// Plugin-Art: Plugin-Bindungen.
    Plugin(String),
    /// Ebene (`global`, `organization` …): Plugin-Bindungen.
    Level(String),
    /// Ziel (Name, auf jeder Ebene): Plugin-Bindungen.
    Target(String),
}

/// Abfrage einer Listenseite.
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    /// Seitengröße (mindestens 1).
    pub limit: u32,
    /// Fortsetzung nach diesem Namen (`nextCursor` der vorigen Seite).
    pub after: Option<String>,
    /// Teilstring (Groß-/Kleinschreibung beachtet) im Namen oder einer Suchspalte der Art
    /// (Modellgruppen: Aliasse, API-Keys: Präfix, Deployments: Anbieter-Modell, Preise:
    /// Modell, MCP-Server: URL).
    pub search: Option<String>,
    /// Label-Selektor: alle müssen passen; `None` als Wert = Schlüssel vorhanden.
    pub labels: Vec<(String, Option<String>)>,
    /// Filter (alle müssen passen).
    pub filters: Vec<ListFilter>,
    /// Lesbarer Bereich des Aufrufers.
    pub scope: AuditScope,
}

/// Eine Seite.
#[derive(Debug, Clone, PartialEq)]
pub struct Page<T> {
    /// Einträge (nach Name sortiert).
    pub items: Vec<T>,
    /// Name des letzten Eintrags, falls es weitere gibt.
    pub next: Option<String>,
}

/// `id` eines Objekts der Art `kind` über seinen Namen (Unterabfrage).
fn id_of(kind: ResourceKind, q: &mut Q, name: &str) -> String {
    let p = q.arg(Val::text(name));
    format!("(SELECT r.id FROM {} r WHERE r.name = {p})", table(kind))
}

/// Teilstring-Suche (ohne LIKE-Sonderzeichen).
fn contains(backend: Backend, col: &str, p: &str) -> String {
    match backend {
        Backend::Sqlite => format!("instr({col}, {p}) > 0"),
        Backend::Postgres => format!("strpos({col}, {p}) > 0"),
    }
}

/// Bedingung „Team mit ID `expr` liegt im Bereich `rules`“.
fn team_in_scope(expr: &str, rules: &[ScopeRule], q: &mut Q) -> String {
    let mut parts = Vec::new();
    for r in rules {
        let mut conds = vec![format!("st.id = {expr}")];
        if let Some(o) = &r.organization {
            conds.push(format!("so.name = {}", q.arg(Val::text(o.clone()))));
        }
        if let Some(t) = &r.team {
            conds.push(format!("st.name = {}", q.arg(Val::text(t.clone()))));
        }
        if r.organization.is_none() && r.team.is_none() {
            continue;
        }
        parts.push(format!(
            "EXISTS (SELECT 1 FROM team st JOIN organization so ON so.id = st.organization_id \
             WHERE {})",
            conds.join(" AND ")
        ));
    }
    if parts.is_empty() {
        "1 = 0".to_owned()
    } else {
        format!("({})", parts.join(" OR "))
    }
}

/// Bereichsbedingung je Art (`None` = keine Einschränkung).
fn scope_sql(kind: ResourceKind, scope: &AuditScope, q: &mut Q) -> Option<String> {
    let AuditScope::Within(rules) = scope else {
        return None;
    };
    Some(match kind {
        ResourceKind::Organization => {
            let orgs: Vec<Val> = rules
                .iter()
                .filter_map(|r| r.organization.clone())
                .map(Val::text)
                .collect();
            if orgs.is_empty() {
                "1 = 0".to_owned()
            } else {
                format!("t.name IN ({})", q.list(orgs))
            }
        }
        ResourceKind::Team => team_in_scope("t.id", rules, q),
        ResourceKind::ApiKey => {
            format!(
                "(t.team_id IS NOT NULL AND {})",
                team_in_scope("t.team_id", rules, q)
            )
        }
        ResourceKind::McpGrant => {
            let team = team_in_scope("t.subject_id", rules, q);
            let key = team_in_scope("k.team_id", rules, q);
            format!(
                "((t.subject_level = 'team' AND {team}) OR (t.subject_level = 'api-key' AND \
                 EXISTS (SELECT 1 FROM api_key k WHERE k.id = t.subject_id AND \
                 k.team_id IS NOT NULL AND {key})))"
            )
        }
        // Katalog: Leserecht prüft der Aufrufer; global: nur mit `All`.
        ResourceKind::ModelGroup
        | ResourceKind::Deployment
        | ResourceKind::Price
        | ResourceKind::ProviderCredential
        | ResourceKind::McpServer
        | ResourceKind::PluginBinding => return None,
        ResourceKind::User => "1 = 0".to_owned(),
    })
}

fn not_applicable(kind: ResourceKind, f: &ListFilter) -> StoreError {
    StoreError::Invalid(format!("Filter {f:?} gilt nicht für {kind}"))
}

/// Bedingung eines Filters.
fn filter_sql(kind: ResourceKind, f: &ListFilter, q: &mut Q) -> Result<String, StoreError> {
    use ResourceKind as K;
    let na = || not_applicable(kind, f);
    Ok(match f {
        ListFilter::Organization(o) => match kind {
            K::Team | K::ApiKey => {
                format!("t.organization_id = {}", id_of(K::Organization, q, o))
            }
            _ => return Err(na()),
        },
        ListFilter::Team(t) if kind == K::ApiKey => {
            format!("t.team_id = {}", id_of(K::Team, q, t))
        }
        ListFilter::User(u) if kind == K::ApiKey => {
            format!("t.user_id = {}", id_of(K::User, q, u))
        }
        ListFilter::ModelGroup(g) if kind == K::Deployment => {
            format!("t.model_group_id = {}", id_of(K::ModelGroup, q, g))
        }
        ListFilter::Credential(c) if matches!(kind, K::Deployment | K::McpServer) => {
            format!("t.credential_id = {}", id_of(K::ProviderCredential, q, c))
        }
        ListFilter::Server(s) if kind == K::McpGrant => {
            format!("t.mcp_server_id = {}", id_of(K::McpServer, q, s))
        }
        ListFilter::Subject(level, name) if kind == K::McpGrant => {
            let target = match level {
                ScopeLevel::Team => K::Team,
                ScopeLevel::ApiKey => K::ApiKey,
                _ => return Err(na()),
            };
            let l = q.arg(Val::text(level.as_str()));
            format!(
                "(t.subject_level = {l} AND t.subject_id = {})",
                id_of(target, q, name)
            )
        }
        ListFilter::Provider(p)
            if matches!(kind, K::Deployment | K::ProviderCredential | K::Price) =>
        {
            format!("t.provider = {}", q.arg(Val::text(p.clone())))
        }
        ListFilter::Model(m) if kind == K::Price => {
            format!("t.model = {}", q.arg(Val::text(m.clone())))
        }
        ListFilter::Transport(tr) if kind == K::McpServer => {
            format!("t.transport = {}", q.arg(Val::text(tr.clone())))
        }
        ListFilter::Enabled(e) if matches!(kind, K::Deployment | K::McpServer) => {
            format!("t.enabled = {}", q.arg(Val::bool(*e)))
        }
        ListFilter::State(s) if kind == K::ApiKey => {
            format!("t.state = {}", q.arg(Val::text(s.clone())))
        }
        ListFilter::Email(e) if kind == K::User => {
            format!("lower(t.email) = lower({})", q.arg(Val::text(e.clone())))
        }
        ListFilter::EffectiveAt(at) if kind == K::Price => {
            let p1 = q.arg(Val::time(*at));
            format!(
                "t.valid_from <= {p1} AND NOT EXISTS (SELECT 1 FROM price p2 WHERE \
                 p2.provider = t.provider AND p2.model = t.model AND p2.valid_from > t.valid_from \
                 AND p2.valid_from <= {p1})"
            )
        }
        ListFilter::Plugin(p) if kind == K::PluginBinding => {
            format!("t.plugin = {}", q.arg(Val::text(p.clone())))
        }
        ListFilter::Level(l) if kind == K::PluginBinding => {
            format!("t.level = {}", q.arg(Val::text(l.clone())))
        }
        ListFilter::Target(name) if kind == K::PluginBinding => {
            let mut ors = Vec::new();
            for level in [
                ScopeLevel::Organization,
                ScopeLevel::Team,
                ScopeLevel::ApiKey,
                ScopeLevel::ModelGroup,
            ] {
                if let Some(k) = super::level_kind(level) {
                    let l = q.arg(Val::text(level.as_str()));
                    ors.push(format!(
                        "(t.level = {l} AND t.target_id = {})",
                        id_of(k, q, name)
                    ));
                }
            }
            format!("({})", ors.join(" OR "))
        }
        _ => return Err(na()),
    })
}

/// Label-Bedingung (JSON-Objekt `labels`).
fn label_sql(backend: Backend, key: &str, value: Option<&str>, q: &mut Q) -> String {
    let k = q.arg(Val::text(key));
    match (backend, value) {
        (Backend::Sqlite, None) => {
            format!("EXISTS (SELECT 1 FROM json_each(t.labels) l WHERE l.key = {k})")
        }
        (Backend::Sqlite, Some(v)) => {
            let v = q.arg(Val::text(v));
            format!(
                "EXISTS (SELECT 1 FROM json_each(t.labels) l WHERE l.key = {k} AND \
                 l.type = 'text' AND l.value = {v})"
            )
        }
        (Backend::Postgres, None) => format!("jsonb_exists(t.labels, {k})"),
        (Backend::Postgres, Some(v)) => {
            let v = q.arg(Val::text(v));
            format!("(t.labels ->> {k}) = {v}")
        }
    }
}

impl Store {
    /// Baut `WHERE … ORDER BY t.name LIMIT n+1` für eine Art.
    fn list_where(
        &self,
        kind: ResourceKind,
        search_cols: &[&str],
        lq: &ListQuery,
        q: &mut Q,
    ) -> Result<Option<String>, StoreError> {
        let backend = self.backend();
        let mut conds: Vec<String> = Vec::new();
        if let Some(scope) = scope_sql(kind, &lq.scope, q) {
            conds.push(scope);
        }
        if let Some(after) = &lq.after {
            conds.push(format!("t.name > {}", q.arg(Val::text(after.clone()))));
        }
        if let Some(s) = &lq.search {
            let p = q.arg(Val::text(s.clone()));
            let mut ors = vec![contains(backend, "t.name", &p)];
            for c in search_cols {
                ors.push(contains(backend, &format!("t.{c}"), &p));
            }
            if kind == ResourceKind::ModelGroup {
                ors.push(format!(
                    "EXISTS (SELECT 1 FROM model_alias a WHERE a.model_group_id = t.id AND {})",
                    contains(backend, "a.alias", &p)
                ));
            }
            conds.push(format!("({})", ors.join(" OR ")));
        }
        for (k, v) in &lq.labels {
            conds.push(label_sql(backend, k, v.as_deref(), q));
        }
        for f in &lq.filters {
            conds.push(filter_sql(kind, f, q)?);
        }
        if conds.iter().any(|c| c == "1 = 0") {
            return Ok(None);
        }
        let limit = i64::from(lq.limit.max(1)) + 1;
        let lim = q.arg(Val::int(limit));
        let filter = if conds.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conds.join(" AND "))
        };
        Ok(Some(format!("{filter} ORDER BY t.name LIMIT {lim}")))
    }

    /// Seite einer Liste mit gemeinsamer Abfrage; `map` wandelt eine Zeile.
    async fn list_rows<T>(
        &self,
        kind: ResourceKind,
        refs: &[super::RefCol],
        search_cols: &[&str],
        lq: &ListQuery,
        map: impl Fn(&crate::db::DbRow) -> Result<(T, String), StoreError>,
    ) -> Result<Page<T>, StoreError> {
        let mut q = Q::default();
        let Some(tail) = self.list_where(kind, search_cols, lq, &mut q)? else {
            return Ok(Page {
                items: Vec::new(),
                next: None,
            });
        };
        q.sql = super::select_sql(kind, refs, &tail);
        let rows = q.fetch_all(&self.pool).await?;
        let limit = usize::try_from(lq.limit.max(1)).unwrap_or(usize::MAX);
        let mut items = Vec::with_capacity(rows.len().min(limit));
        let mut last = None;
        for r in rows.iter().take(limit) {
            let (item, name) = map(r)?;
            items.push(item);
            last = Some(name);
        }
        let next = if rows.len() > limit { last } else { None };
        Ok(Page { items, next })
    }

    /// Seite einer Ressourcenart (F42): Filter, Suche, Bereich und Keyset-Cursor in SQL.
    pub async fn list_page<R: Resource>(
        &self,
        lq: &ListQuery,
    ) -> Result<Page<Object<R>>, StoreError> {
        self.list_rows(R::KIND, R::REFS, R::SEARCH, lq, |r| {
            let o = Self::object_from_row::<R>(r)?;
            let name = o.metadata.name.clone();
            Ok((o, name))
        })
        .await
    }

    /// Seite der API-Keys (ohne Klartext/Hash).
    pub async fn list_api_keys_page(&self, lq: &ListQuery) -> Result<Page<ApiKey>, StoreError> {
        self.list_rows(
            ResourceKind::ApiKey,
            ApiKeySpec::REFS,
            ApiKeySpec::SEARCH,
            lq,
            |r| {
                let (k, _) = super::api_key_from_row(r)?;
                let name = k.metadata.name.clone();
                Ok((k, name))
            },
        )
        .await
    }

    /// Seite der Zugangsdaten (nur Metadaten und Feldnamen).
    pub async fn list_credentials_page(
        &self,
        lq: &ListQuery,
    ) -> Result<Page<ProviderCredential>, StoreError> {
        self.list_rows(ResourceKind::ProviderCredential, &[], &[], lq, |r| {
            let c = super::credential::credential_from_row(r)?;
            let name = c.metadata.name.clone();
            Ok((c, name))
        })
        .await
    }
}
