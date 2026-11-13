// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Audit-Log (AIGW-9, AIGW-66): Einträge schreiben (Geltungsbereich, Ereignisse ohne
//! Konfigurationsänderung) und gefiltert lesen (Keyset-Paginierung, neueste zuerst).
//!
//! Jede Konfigurationsänderung schreibt ihren Eintrag in derselben Transaktion
//! ([`AdminTx::audit_kind`]). Dazu kommen **Ereignisse** ohne neue Konfigurationsversion
//! ([`Store::record_audit_event`]): Login, fehlgeschlagener Login, Logout, Erstnutzung eines
//! Admin-API-Keys, Abschalten des Bootstrap-Tokens; sie tragen die zu diesem Zeitpunkt
//! aktuelle `config_version`.
//!
//! **Geltungsbereich:** Jeder Eintrag trägt Organisation und Team (Namen) des betroffenen
//! Objekts – nachher (`organization`, `team`) und vorher (`organization_before`,
//! `team_before`) –, damit eingeschränkte Rollen nur ihren Bereich lesen. Bereich haben
//! Organisationen (sich selbst), Teams (Organisation + Team) und API-Keys mit Team; alles
//! andere (Nutzer, Katalog, Admin-API-Keys, Sessions …) ist global.
//!
//! **Geheimnisse:** Vorher/Nachher sind die Darstellungen der Admin-API – ohne Werte von
//! Zugangsdaten, ohne Key-Hashes und -Klartexte (`SecretString` serialisiert `<redacted>`).

use aigw_core::resource::{Id, ResourceKind, new_id};
use serde::Serialize;
use sqlx::AssertSqlSafe;
use sqlx::any::AnyRow;
use time::OffsetDateTime;

use super::{AdminTx, Row, Val, millis, now, parse_id};
use crate::{Store, StoreError};

/// Art im Audit-Log für Sessions der Admin-UI (Login, Logout, beendete Sessions).
pub const ADMIN_SESSION_KIND: &str = "admin-session";
/// Art im Audit-Log für das Bootstrap-Token.
pub const BOOTSTRAP_TOKEN_KIND: &str = "bootstrap-token";

/// Geltungsbereich eines Objekts (Namen zum Zeitpunkt des Eintrags).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Scope {
    pub(crate) organization: Option<String>,
    pub(crate) team: Option<String>,
}

/// `true`, wenn eine Änderung eines Nutzers seine Sessions beenden muss: andere Rollen oder
/// eine andere OIDC-Identität (`spec` vorher/nachher als JSON).
pub(crate) fn user_change_ends_sessions(
    before: &serde_json::Value,
    after: &serde_json::Value,
) -> bool {
    fn roles(v: &serde_json::Value) -> Vec<String> {
        let mut r: Vec<String> = v
            .get("roles")
            .and_then(serde_json::Value::as_array)
            .map(|a| a.iter().map(ToString::to_string).collect())
            .unwrap_or_default();
        r.sort();
        r.dedup();
        r
    }
    roles(before) != roles(after)
        || before.get("oidcIssuer") != after.get("oidcIssuer")
        || before.get("oidcSubject") != after.get("oidcSubject")
}

impl AdminTx {
    /// Geltungsbereich eines Objekts aus seiner JSON-Darstellung.
    pub(crate) async fn scope_of(
        &mut self,
        kind: &str,
        name: &str,
        v: &serde_json::Value,
    ) -> Result<Scope, StoreError> {
        let spec_str = |field: &str| {
            v.get("spec")
                .and_then(|s| s.get(field))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        if kind == ResourceKind::Organization.as_str() {
            return Ok(Scope {
                organization: Some(name.to_owned()),
                team: None,
            });
        }
        if kind == ResourceKind::Team.as_str() {
            return Ok(Scope {
                organization: spec_str("organization"),
                team: Some(name.to_owned()),
            });
        }
        if kind == ResourceKind::ApiKey.as_str()
            && let Some(team) = spec_str("team")
        {
            let organization: Option<String> = sqlx::query_scalar(
                "SELECT o.name FROM team t JOIN organization o ON o.id = t.organization_id
                 WHERE t.name = $1",
            )
            .bind(team.clone())
            .fetch_optional(&mut *self.tx)
            .await?;
            return Ok(Scope {
                organization,
                team: Some(team),
            });
        }
        Ok(Scope::default())
    }

    /// Beendet alle Sessions eines Nutzers in dieser Transaktion (Rollenänderung, Löschen);
    /// vermerkt das im Audit-Log, falls es Sessions gab. Liefert die Anzahl.
    pub(crate) async fn end_user_sessions(
        &mut self,
        user_id: Id,
        user_name: &str,
        reason: &str,
    ) -> Result<u64, StoreError> {
        let n = sqlx::query("DELETE FROM admin_session WHERE user_id = $1")
            .bind(user_id.to_string())
            .execute(&mut *self.tx)
            .await?
            .rows_affected();
        if n > 0 {
            let details = serde_json::json!({"reason": reason, "sessions": n});
            insert_event(
                &mut self.tx,
                &AuditEvent {
                    actor: self.actor.clone(),
                    action: "revoke".into(),
                    kind: ADMIN_SESSION_KIND.into(),
                    object_id: Some(user_id),
                    object_name: Some(user_name.to_owned()),
                    details: Some(details),
                },
                Some(self.version),
            )
            .await?;
        }
        Ok(n)
    }
}

/// Ereignis ohne Konfigurationsänderung (Login, Logout, Erstnutzung eines Admin-Keys …).
#[derive(Debug, Clone, PartialEq)]
pub struct AuditEvent {
    /// Akteur (`user:<name>`, `admin-key:<name>`, `oidc-login`, `system` …).
    pub actor: String,
    /// Aktion (`login`, `login-failed`, `logout`, `first-use`, `disable`, `revoke`).
    pub action: String,
    /// Art (`admin-session`, `admin-api-key`, `bootstrap-token`).
    pub kind: String,
    /// Betroffenes Objekt (ID).
    pub object_id: Option<Id>,
    /// Betroffenes Objekt (Name).
    pub object_name: Option<String>,
    /// Einzelheiten (stehen in `after`); keine Geheimnisse.
    pub details: Option<serde_json::Value>,
}

async fn insert_event(
    conn: &mut sqlx::AnyConnection,
    ev: &AuditEvent,
    version: Option<i64>,
) -> Result<(), StoreError> {
    let version = match version {
        Some(v) => v,
        None => {
            sqlx::query_scalar("SELECT version FROM config_version WHERE id = 1")
                .fetch_one(&mut *conn)
                .await?
        }
    };
    let details = ev.details.as_ref().map(serde_json::to_string).transpose()?;
    sqlx::query(
        "INSERT INTO audit_log (id, at, actor, action, kind, object_id, object_name, config_version,
                                before_json, after_json)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NULL, $9)",
    )
    .bind(new_id().to_string())
    .bind(millis(now()))
    .bind(ev.actor.clone())
    .bind(ev.action.clone())
    .bind(ev.kind.clone())
    .bind(ev.object_id.map(|i| i.to_string()))
    .bind(ev.object_name.clone())
    .bind(version)
    .bind(details)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Lesen
// ---------------------------------------------------------------------------------------------

/// Eintrag im Audit-Log.
#[derive(Debug, Clone, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AuditEntry {
    /// ID (UUIDv7).
    #[schema(value_type = String, format = Uuid)]
    pub id: Id,
    /// Zeitpunkt.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub at: OffsetDateTime,
    /// Wer: `user:<name>`, `admin-key:<name>`, `bootstrap-admin`, `gitops` bzw.
    /// `gitops:<principal>`, `oidc-login`, `system`.
    pub actor: String,
    /// `create` | `update` | `delete` | `rotate` | `revoke` | `reactivate` | `login` |
    /// `login-failed` | `logout` | `first-use` | `disable`.
    pub action: String,
    /// Art (`Organization`, `Team`, `User`, `ApiKey`, `ModelGroup`, `Deployment`,
    /// `ProviderCredential`, `Price` …, `admin-api-key`, `admin-session`, `bootstrap-token`).
    pub kind: String,
    /// Objekt-ID.
    #[schema(value_type = Option<String>, format = Uuid)]
    pub object_id: Option<Id>,
    /// Objektname.
    pub object_name: Option<String>,
    /// Konfigurationsversion nach der Änderung (Ereignisse: die damals aktuelle).
    pub config_version: i64,
    /// Organisation des Objekts (nachher; beim Löschen: vorher).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    /// Team des Objekts (nachher; beim Löschen: vorher).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    /// Organisation vor der Änderung.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organization_before: Option<String>,
    /// Team vor der Änderung.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_before: Option<String>,
    /// Zustand vorher (Geheimnisse redigiert).
    #[schema(value_type = Option<Object>)]
    pub before: Option<serde_json::Value>,
    /// Zustand nachher bzw. Einzelheiten eines Ereignisses (Geheimnisse redigiert).
    #[schema(value_type = Option<Object>)]
    pub after: Option<serde_json::Value>,
}

/// Art des Akteurs (Filter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorType {
    /// Angemeldeter Nutzer (`user:<name>`).
    User,
    /// Admin-API-Key (`admin-key:<name>`).
    AdminKey,
    /// Bootstrap-Token (`bootstrap-admin`).
    Bootstrap,
    /// GitOps (`gitops` aus der CLI, `gitops:<principal>` über die Admin-API).
    Gitops,
    /// Nutzeranlage/-verknüpfung und fehlgeschlagene Logins (`oidc-login`).
    OidcLogin,
    /// Server selbst (`system`).
    System,
}

impl ActorType {
    /// Alle Arten.
    pub const ALL: [Self; 6] = [
        Self::User,
        Self::AdminKey,
        Self::Bootstrap,
        Self::Gitops,
        Self::OidcLogin,
        Self::System,
    ];

    /// Bezeichner im Filter.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::AdminKey => "admin-key",
            Self::Bootstrap => "bootstrap",
            Self::Gitops => "gitops",
            Self::OidcLogin => "oidc-login",
            Self::System => "system",
        }
    }

    /// Gegenstück zu [`ActorType::as_str`].
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == s)
    }

    /// SQL-Bedingung (feste Muster, keine Eingaben).
    fn sql(self) -> &'static str {
        match self {
            Self::User => "actor LIKE 'user:%'",
            Self::AdminKey => "actor LIKE 'admin-key:%'",
            Self::Bootstrap => "actor = 'bootstrap-admin'",
            Self::Gitops => "(actor = 'gitops' OR actor LIKE 'gitops:%')",
            Self::OidcLogin => "actor = 'oidc-login'",
            Self::System => "actor = 'system'",
        }
    }
}

/// Bereich, auf den ein Leser eingeschränkt ist.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum AuditScope {
    /// Alle Einträge (globaler `admin`/`auditor`).
    #[default]
    All,
    /// Nur Einträge, deren Objekt vorher oder nachher in einem der Bereiche lag.
    Within(Vec<ScopeRule>),
}

/// Ein Bereich: Organisation und/oder Team (wie eine Rollenzuweisung).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeRule {
    /// Organisation (bei Team-Bereichen optional, dann passt jede Organisation).
    pub organization: Option<String>,
    /// Team.
    pub team: Option<String>,
}

/// Position in der Liste (Zeitpunkt, ID) – als Text `<ms>.<id>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditCursor {
    /// Zeitpunkt (Unix-Millisekunden).
    pub at: i64,
    /// ID des Eintrags.
    pub id: Id,
}

impl AuditCursor {
    /// Textform für `nextCursor`.
    pub fn encode(&self) -> String {
        format!("{}.{}", self.at, self.id)
    }

    /// Liest die Textform; `None` bei ungültigem Wert.
    pub fn parse(s: &str) -> Option<Self> {
        let (at, id) = s.split_once('.')?;
        Some(Self {
            at: at.parse().ok()?,
            id: Id::parse_str(id).ok()?,
        })
    }
}

/// Filter und Seite einer Abfrage.
#[derive(Debug, Clone, Default)]
pub struct AuditQuery {
    /// Ab (einschließlich).
    pub from: Option<OffsetDateTime>,
    /// Bis (ausschließlich).
    pub to: Option<OffsetDateTime>,
    /// Akteure (exakt; leer = alle).
    pub actors: Vec<String>,
    /// Akteursarten (leer = alle).
    pub actor_types: Vec<ActorType>,
    /// Arten (exakt; leer = alle).
    pub kinds: Vec<String>,
    /// Aktionen (exakt; leer = alle).
    pub actions: Vec<String>,
    /// Objekt: Name oder ID (exakt).
    pub object: Option<String>,
    /// Bereich des Lesers.
    pub scope: AuditScope,
    /// Fortsetzung nach diesem Eintrag.
    pub cursor: Option<AuditCursor>,
    /// Älteste zuerst (Standard: neueste zuerst).
    pub ascending: bool,
    /// Seitengröße (mindestens 1).
    pub limit: u32,
}

/// Eine Seite.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditPage {
    /// Einträge.
    pub items: Vec<AuditEntry>,
    /// Fortsetzung (fehlt auf der letzten Seite).
    pub next: Option<AuditCursor>,
}

/// Baut WHERE-Bedingungen mit fortlaufend nummerierten Platzhaltern.
#[derive(Default)]
struct Where {
    conds: Vec<String>,
    args: Vec<Val>,
}

impl Where {
    fn arg(&mut self, v: Val) -> String {
        self.args.push(v);
        format!("${}", self.args.len())
    }

    fn text(&mut self, s: &str) -> String {
        self.arg(Val::Text(Some(s.to_owned())))
    }

    fn int(&mut self, i: i64) -> String {
        self.arg(Val::Int(Some(i)))
    }

    fn any_of(&mut self, col: &str, values: &[String]) {
        if values.is_empty() {
            return;
        }
        let ps: Vec<String> = values.iter().map(|v| self.text(v)).collect();
        self.conds.push(format!("{col} IN ({})", ps.join(", ")));
    }

    fn scope_rule(&mut self, r: &ScopeRule) -> String {
        let side = |w: &mut Self, org_col: &str, team_col: &str| {
            let mut parts = Vec::new();
            if let Some(o) = &r.organization {
                parts.push(format!("{org_col} = {}", w.text(o)));
            }
            if let Some(t) = &r.team {
                parts.push(format!("{team_col} = {}", w.text(t)));
            }
            if parts.is_empty() {
                "1 = 0".to_owned()
            } else {
                parts.join(" AND ")
            }
        };
        let now = side(self, "organization", "team");
        let before = side(self, "organization_before", "team_before");
        format!("(({now}) OR ({before}))")
    }
}

fn entry_from_row(row: &AnyRow) -> Result<AuditEntry, StoreError> {
    let r = Row(row);
    Ok(AuditEntry {
        id: parse_id(&r.text("id")?)?,
        at: r.time("at")?,
        actor: r.text("actor")?,
        action: r.text("action")?,
        kind: r.text("kind")?,
        object_id: r.opt_text("object_id")?.map(|s| parse_id(&s)).transpose()?,
        object_name: r.opt_text("object_name")?,
        config_version: r.int("config_version")?,
        organization: r.opt_text("organization")?,
        team: r.opt_text("team")?,
        organization_before: r.opt_text("organization_before")?,
        team_before: r.opt_text("team_before")?,
        before: r.opt_json("before_json")?,
        after: r.opt_json("after_json")?,
    })
}

impl Store {
    /// Schreibt ein Ereignis ohne Konfigurationsänderung (keine neue `config_version`,
    /// kein Snapshot-Neubau).
    pub async fn record_audit_event(&self, ev: &AuditEvent) -> Result<(), StoreError> {
        let mut conn = self.pool.acquire().await?;
        insert_event(&mut conn, ev, None).await
    }

    /// Letzte Einträge des Audit-Logs (neueste zuerst, ohne Filter).
    pub async fn audit_log(&self, limit: i64) -> Result<Vec<AuditEntry>, StoreError> {
        let limit = u32::try_from(limit.max(1)).unwrap_or(u32::MAX);
        Ok(self
            .query_audit_log(&AuditQuery {
                limit,
                ..AuditQuery::default()
            })
            .await?
            .items)
    }

    /// Liest eine Seite des Audit-Logs (Keyset-Paginierung über (Zeitpunkt, ID)).
    pub async fn query_audit_log(&self, q: &AuditQuery) -> Result<AuditPage, StoreError> {
        let mut w = Where::default();
        if let Some(from) = q.from {
            let p = w.int(millis(from));
            w.conds.push(format!("at >= {p}"));
        }
        if let Some(to) = q.to {
            let p = w.int(millis(to));
            w.conds.push(format!("at < {p}"));
        }
        w.any_of("actor", &q.actors);
        if !q.actor_types.is_empty() {
            let parts: Vec<&str> = q.actor_types.iter().map(|t| t.sql()).collect();
            w.conds.push(format!("({})", parts.join(" OR ")));
        }
        w.any_of("kind", &q.kinds);
        w.any_of("action", &q.actions);
        if let Some(o) = &q.object {
            let a = w.text(o);
            let b = w.text(o);
            w.conds
                .push(format!("(object_name = {a} OR object_id = {b})"));
        }
        if let AuditScope::Within(rules) = &q.scope {
            let parts: Vec<String> = rules.iter().map(|r| w.scope_rule(r)).collect();
            if parts.is_empty() {
                w.conds.push("1 = 0".into());
            } else {
                w.conds.push(format!("({})", parts.join(" OR ")));
            }
        }
        let (cmp, dir) = if q.ascending {
            (">", "ASC")
        } else {
            ("<", "DESC")
        };
        if let Some(c) = &q.cursor {
            let a1 = w.int(c.at);
            let a2 = w.int(c.at);
            let id = w.text(&c.id.to_string());
            w.conds
                .push(format!("(at {cmp} {a1} OR (at = {a2} AND id {cmp} {id}))"));
        }
        let limit = q.limit.max(1);
        let lim = w.int(i64::from(limit) + 1);
        let filter = if w.conds.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", w.conds.join(" AND "))
        };
        let sql = format!(
            "SELECT id, at, actor, action, kind, object_id, object_name, config_version,
                    organization, team, organization_before, team_before, before_json, after_json
             FROM audit_log {filter} ORDER BY at {dir}, id {dir} LIMIT {lim}"
        );
        let mut query = sqlx::query(AssertSqlSafe(sql));
        for v in w.args {
            query = match v {
                Val::Text(t) => query.bind(t),
                Val::Int(i) => query.bind(i),
            };
        }
        let rows = query.fetch_all(&self.pool).await?;
        let mut items = rows
            .iter()
            .map(entry_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        let next = if items.len() > limit {
            items.truncate(limit);
            items.last().map(|e| AuditCursor {
                at: millis(e.at),
                id: e.id,
            })
        } else {
            None
        };
        Ok(AuditPage { items, next })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use aigw_core::resource::{
        ApiKeySpec, ObjectMeta, OrganizationSpec, Role, RoleBinding, TeamSpec, UserSpec,
    };
    use time::Duration;

    use super::*;

    async fn store() -> Store {
        let s = Store::connect("sqlite::memory:").await.unwrap();
        s.migrate().await.unwrap();
        s
    }

    fn rb(role: Role, org: Option<&str>) -> RoleBinding {
        RoleBinding {
            role,
            organization: org.map(Into::into),
            team: None,
        }
    }

    #[test]
    fn cursor_roundtrip_and_session_rule() {
        let c = AuditCursor {
            at: 1_700_000_000_123,
            id: new_id(),
        };
        assert_eq!(AuditCursor::parse(&c.encode()), Some(c));
        for bad in ["", "x.y", "12", "12.nope", ".0192"] {
            assert!(AuditCursor::parse(bad).is_none(), "{bad}");
        }
        let a = serde_json::json!({"roles": [{"role": "viewer"}, {"role": "admin"}]});
        let b = serde_json::json!({"roles": [{"role": "admin"}, {"role": "viewer"}]});
        assert!(!user_change_ends_sessions(&a, &b), "Reihenfolge egal");
        assert!(user_change_ends_sessions(&a, &serde_json::json!({})));
        assert!(!user_change_ends_sessions(
            &serde_json::json!({"roles": []}),
            &serde_json::json!({"email": "x@y"})
        ));
        assert!(user_change_ends_sessions(
            &serde_json::json!({"oidcSubject": "a"}),
            &serde_json::json!({"oidcSubject": "b"})
        ));
        for t in ActorType::ALL {
            assert_eq!(ActorType::parse(t.as_str()), Some(t));
        }
    }

    async fn count(s: &Store, q: AuditQuery) -> usize {
        s.query_audit_log(&q).await.unwrap().items.len()
    }

    #[tokio::test]
    async fn scope_filters_and_pagination() {
        let s = store().await;
        for o in ["acme", "other"] {
            s.create(
                "user:anna",
                ObjectMeta::named(o),
                OrganizationSpec::default(),
            )
            .await
            .unwrap();
        }
        for (t, o) in [("platform", "acme"), ("ops", "other")] {
            s.create(
                "admin-key:ci",
                ObjectMeta::named(t),
                TeamSpec {
                    organization: o.into(),
                    ..TeamSpec::default()
                },
            )
            .await
            .unwrap();
        }
        let key = s
            .create_api_key(
                "gitops",
                ObjectMeta::named("k1"),
                ApiKeySpec {
                    team: Some("platform".into()),
                    ..ApiKeySpec::default()
                },
            )
            .await
            .unwrap();
        // Key wandert ins Team einer anderen Organisation: bleibt für acme sichtbar.
        let mut spec = key.key.spec.clone();
        spec.team = Some("ops".into());
        s.update_api_key("bootstrap-admin", key.key.metadata.clone(), spec)
            .await
            .unwrap();
        s.revoke_api_key("bootstrap-admin", "k1").await.unwrap();

        let all = s
            .query_audit_log(&AuditQuery {
                limit: 100,
                ..AuditQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(all.items.len(), 7);
        assert!(all.next.is_none());
        assert_eq!(all.items[0].action, "revoke");
        assert_eq!(all.items[0].organization.as_deref(), Some("other"));
        let moved = &all.items[1];
        assert_eq!(
            (moved.team.as_deref(), moved.team_before.as_deref()),
            (Some("ops"), Some("platform"))
        );
        assert_eq!(moved.organization_before.as_deref(), Some("acme"));

        // Seitenweise (2 je Seite) ergibt dieselbe Reihenfolge, auch aufsteigend.
        for ascending in [false, true] {
            let mut seen = Vec::new();
            let mut cursor = None;
            loop {
                let page = s
                    .query_audit_log(&AuditQuery {
                        limit: 2,
                        cursor: cursor.clone(),
                        ascending,
                        ..AuditQuery::default()
                    })
                    .await
                    .unwrap();
                seen.extend(page.items.iter().map(|e| e.id));
                match page.next {
                    Some(c) => cursor = Some(c),
                    None => break,
                }
            }
            let mut want: Vec<Id> = all.items.iter().map(|e| e.id).collect();
            if ascending {
                want.reverse();
            }
            assert_eq!(seen, want);
        }

        let q = |f: &dyn Fn(&mut AuditQuery)| {
            let mut q = AuditQuery {
                limit: 100,
                ..AuditQuery::default()
            };
            f(&mut q);
            q
        };
        assert_eq!(
            count(
                &s,
                q(&|q| q.scope = AuditScope::Within(vec![ScopeRule {
                    organization: Some("acme".into()),
                    team: None
                }]))
            )
            .await,
            4, // Org acme, Team platform, Key angelegt, Key verschoben
        );
        assert_eq!(
            count(
                &s,
                q(&|q| q.scope = AuditScope::Within(vec![ScopeRule {
                    organization: None,
                    team: Some("ops".into())
                }]))
            )
            .await,
            3, // Team ops, Key verschoben, Key gesperrt
        );
        assert_eq!(
            count(&s, q(&|q| q.scope = AuditScope::Within(vec![]))).await,
            0
        );
        assert_eq!(
            count(&s, q(&|q| q.actor_types = vec![ActorType::User])).await,
            2
        );
        assert_eq!(
            count(
                &s,
                q(&|q| q.actor_types = vec![ActorType::Gitops, ActorType::AdminKey])
            )
            .await,
            3
        );
        assert_eq!(
            count(&s, q(&|q| q.actors = vec!["user:anna".into()])).await,
            2
        );
        assert_eq!(count(&s, q(&|q| q.kinds = vec!["Team".into()])).await, 2);
        assert_eq!(
            count(
                &s,
                q(&|q| q.actions = vec!["create".into(), "revoke".into()])
            )
            .await,
            6
        );
        assert_eq!(count(&s, q(&|q| q.object = Some("k1".into()))).await, 3);
        assert_eq!(
            count(&s, q(&|q| q.object = Some(key.key.id.to_string()))).await,
            3
        );
        let t0 = all.items[3].at;
        assert_eq!(
            count(&s, q(&|q| q.from = Some(t0 + Duration::days(1)))).await,
            0
        );
        assert_eq!(
            count(&s, q(&|q| q.to = Some(t0 - Duration::days(1)))).await,
            0
        );
        assert_eq!(
            count(
                &s,
                q(&|q| {
                    q.from = Some(t0 - Duration::days(1));
                    q.to = Some(t0 + Duration::days(1));
                })
            )
            .await,
            7
        );
    }

    #[tokio::test]
    async fn user_changes_end_sessions() {
        let s = store().await;
        let user = s
            .create(
                "bootstrap-admin",
                ObjectMeta::named("anna"),
                UserSpec {
                    roles: vec![rb(Role::Admin, None)],
                    ..UserSpec::default()
                },
            )
            .await
            .unwrap();
        let exp = OffsetDateTime::now_utc() + Duration::hours(1);
        let sid = |n: &str| crate::hash_api_key(n);
        s.create_admin_session(&sid("s1"), user.id, &[], exp)
            .await
            .unwrap();
        s.create_admin_session(&sid("s2"), user.id, &[], exp)
            .await
            .unwrap();

        // Änderung ohne Rollen/Identität: Sessions bleiben.
        let mut spec = user.spec.clone();
        spec.display_name = Some("Anna".into());
        s.update("bootstrap-admin", user.metadata.clone(), spec.clone())
            .await
            .unwrap();
        assert!(s.get_admin_session(&sid("s1")).await.unwrap().is_some());

        // Rollenänderung: alle Sessions weg, Ereignis im Audit-Log.
        spec.roles = vec![rb(Role::Viewer, None)];
        s.update("bootstrap-admin", user.metadata.clone(), spec)
            .await
            .unwrap();
        assert!(s.get_admin_session(&sid("s1")).await.unwrap().is_none());
        assert!(s.get_admin_session(&sid("s2")).await.unwrap().is_none());
        let log = s.audit_log(1).await.unwrap();
        assert_eq!(
            (log[0].kind.as_str(), log[0].action.as_str()),
            (ADMIN_SESSION_KIND, "revoke")
        );
        assert_eq!(log[0].after.as_ref().unwrap()["sessions"], 2);
        assert_eq!(log[0].after.as_ref().unwrap()["reason"], "roles-changed");

        // Löschen: Sessions weg.
        s.create_admin_session(&sid("s3"), user.id, &[], exp)
            .await
            .unwrap();
        s.delete("bootstrap-admin", ResourceKind::User, "anna")
            .await
            .unwrap();
        assert!(s.get_admin_session(&sid("s3")).await.unwrap().is_none());
        let log = s.audit_log(2).await.unwrap();
        assert_eq!(log[0].action, "delete");
        assert_eq!(log[1].after.as_ref().unwrap()["reason"], "user-deleted");

        // Ereignisse erhöhen die Konfigurationsversion nicht.
        let v = s.config_version().await.unwrap();
        s.record_audit_event(&AuditEvent {
            actor: "user:anna".into(),
            action: "login".into(),
            kind: ADMIN_SESSION_KIND.into(),
            object_id: None,
            object_name: Some("anna".into()),
            details: None,
        })
        .await
        .unwrap();
        assert_eq!(s.config_version().await.unwrap(), v);
        assert_eq!(s.audit_log(1).await.unwrap()[0].config_version, v);
    }
}
