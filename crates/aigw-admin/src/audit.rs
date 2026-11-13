// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Audit-Log-Endpunkt (AIGW-66): `GET /admin/api/v1/audit-log`.
//!
//! - Keyset-Paginierung über (Zeitpunkt, ID), neueste zuerst (`order=asc` für älteste zuerst),
//!   `nextCursor` wie bei den Ressourcenlisten.
//! - Filter: `from`/`to` (RFC 3339), `actor`, `actorType`, `kind`, `action` (je kommagetrennt),
//!   `object` (Name oder ID).
//! - `Accept: application/x-ndjson`: Export als JSON Lines (ein Eintrag je Zeile, gestreamt,
//!   ohne Seitengrenze) für SIEM-Systeme.
//!
//! Berechtigung (§5.4): globaler `admin` und `auditor` lesen alles; `org-admin`, `team-admin`
//! sowie auf Organisation/Team eingeschränkte `admin`/`auditor` lesen nur Einträge, deren
//! Objekt vorher oder nachher in ihrem Bereich lag; `viewer` hat keinen Zugriff.

// `#[utoipa::path(params(…))]` erzeugt für IntoParams-Typen qualifizierte Pfade.
#![allow(unused_qualifications)]

use aigw_core::resource::Role;
use aigw_store::{
    ActorType, AuditCursor, AuditEntry, AuditPage, AuditQuery, AuditScope, ScopeRule, Store,
};
use axum::body::{Body, Bytes};
use axum::extract::rejection::QueryRejection;
use axum::extract::{Extension, Query, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use utoipa::{IntoParams, ToSchema};

use crate::AdminState;
use crate::auth::Principal;
use crate::problem::Problem;
use crate::rbac;

/// Medientyp des Exports.
pub const NDJSON: &str = "application/x-ndjson";

const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 500;
/// Seitengröße beim Export (je Datenbankabfrage).
const EXPORT_BATCH: u32 = 500;

/// Filter und Seite des Audit-Logs.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuditLogQuery {
    /// Seitengröße (1–500, Standard 50). Beim NDJSON-Export: Höchstzahl der Einträge
    /// (ohne Angabe: alle).
    pub limit: Option<u32>,
    /// Fortsetzung (`nextCursor` der vorigen Seite).
    pub cursor: Option<String>,
    /// Ab diesem Zeitpunkt (einschließlich, RFC 3339).
    pub from: Option<String>,
    /// Bis zu diesem Zeitpunkt (ausschließlich, RFC 3339).
    pub to: Option<String>,
    /// Akteure, exakt und kommagetrennt (z. B. `user:anna,admin-key:ci`).
    pub actor: Option<String>,
    /// Akteursarten, kommagetrennt: `user`, `admin-key`, `bootstrap`, `gitops`, `oidc-login`,
    /// `system`.
    pub actor_type: Option<String>,
    /// Ressourcenarten, kommagetrennt (z. B. `Team,ApiKey,admin-api-key,admin-session`).
    pub kind: Option<String>,
    /// Aktionen, kommagetrennt (`create`, `update`, `delete`, `rotate`, `revoke`,
    /// `reactivate`, `login`, `login-failed`, `logout`, `first-use`, `disable`).
    pub action: Option<String>,
    /// Objekt: Name oder ID (exakt).
    pub object: Option<String>,
    /// Sortierung: `desc` (Standard, neueste zuerst) oder `asc`.
    pub order: Option<String>,
}

/// Eine Seite des Audit-Logs.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AuditLogPage {
    /// Einträge (neueste zuerst bzw. nach `order`).
    pub items: Vec<AuditEntry>,
    /// Fortsetzung; fehlt auf der letzten Seite.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Bereich, den ein Principal im Audit-Log lesen darf.
pub fn visibility(p: &Principal) -> Result<AuditScope, Problem> {
    let mut rules = Vec::new();
    for b in p.roles.iter().filter(|b| rbac::is_effective(b)) {
        if b.role == Role::Viewer {
            continue;
        }
        match (&b.organization, &b.team) {
            (None, None) => {
                if matches!(b.role, Role::Admin | Role::Auditor) {
                    return Ok(AuditScope::All);
                }
            }
            (organization, team) => {
                let rule = ScopeRule {
                    organization: organization.clone(),
                    team: team.clone(),
                };
                if !rules.contains(&rule) {
                    rules.push(rule);
                }
            }
        }
    }
    if rules.is_empty() {
        Err(Problem::forbidden(
            "Audit-Log: Rolle auditor, admin, org-admin oder team-admin nötig",
        ))
    } else {
        Ok(AuditScope::Within(rules))
    }
}

fn list(v: Option<&str>) -> Vec<String> {
    v.map(|s| {
        s.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    })
    .unwrap_or_default()
}

fn time_param(name: &str, v: Option<&str>) -> Result<Option<OffsetDateTime>, Problem> {
    let Some(v) = v else { return Ok(None) };
    // Ein unkodiertes `+` im Offset kommt als Leerzeichen an.
    OffsetDateTime::parse(v, &Rfc3339)
        .or_else(|_| OffsetDateTime::parse(&v.replace(' ', "+"), &Rfc3339))
        .map(Some)
        .map_err(|_| Problem::bad_request(format!("{name}: Zeitpunkt nach RFC 3339 erwartet")))
}

/// Wandelt die Query-Parameter in eine Store-Abfrage (ohne Seitengröße).
fn to_query(q: &AuditLogQuery, scope: AuditScope) -> Result<AuditQuery, Problem> {
    let actor_types = list(q.actor_type.as_deref())
        .iter()
        .map(|s| {
            ActorType::parse(s).ok_or_else(|| {
                Problem::bad_request(format!(
                    "actorType: unbekannte Art {s:?} (user, admin-key, bootstrap, gitops, oidc-login, system)"
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let ascending = match q.order.as_deref() {
        None | Some("desc") => false,
        Some("asc") => true,
        Some(o) => {
            return Err(Problem::bad_request(format!(
                "order: {o:?} – erlaubt sind asc und desc"
            )));
        }
    };
    let cursor = q
        .cursor
        .as_deref()
        .map(|c| AuditCursor::parse(c).ok_or_else(|| Problem::bad_request("cursor ungültig")))
        .transpose()?;
    Ok(AuditQuery {
        from: time_param("from", q.from.as_deref())?,
        to: time_param("to", q.to.as_deref())?,
        actors: list(q.actor.as_deref()),
        actor_types,
        kinds: list(q.kind.as_deref()),
        actions: list(q.action.as_deref()),
        object: q.object.clone().filter(|o| !o.is_empty()),
        scope,
        cursor,
        ascending,
        limit: DEFAULT_LIMIT,
    })
}

fn wants_ndjson(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|m| {
            m.split(';')
                .next()
                .is_some_and(|t| t.trim().eq_ignore_ascii_case(NDJSON))
        })
}

/// Liest das Audit-Log (gefiltert, seitenweise) oder exportiert es als JSON Lines.
#[utoipa::path(
    get, path = "/admin/api/v1/audit-log", tag = "audit-log", params(AuditLogQuery),
    responses(
        (status = 200, description = "Einträge mit Vorher/Nachher (Geheimnisse redigiert). Mit \
            `Accept: application/x-ndjson`: alle passenden Einträge als JSON Lines \
            (je Zeile ein `AuditEntry`, gestreamt).",
            content(
                (AuditLogPage = "application/json"),
                (AuditEntry = "application/x-ndjson"),
            )),
        (status = 400, description = "Ungültiger Parameter", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung (z. B. viewer)", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub(crate) async fn audit_log(
    State(st): State<AdminState>,
    Extension(p): Extension<Principal>,
    headers: HeaderMap,
    q: Result<Query<AuditLogQuery>, QueryRejection>,
) -> Result<Response, Problem> {
    let Query(q) = q?;
    let scope = visibility(&p)?;
    let mut query = to_query(&q, scope)?;
    if wants_ndjson(&headers) {
        if q.limit == Some(0) {
            return Err(Problem::bad_request("limit muss mindestens 1 sein"));
        }
        return Ok(export(st.store.clone(), query, q.limit.map(u64::from)));
    }
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(Problem::bad_request(format!(
            "limit muss zwischen 1 und {MAX_LIMIT} liegen"
        )));
    }
    query.limit = limit;
    let AuditPage { items, next } = st.store.query_audit_log(&query).await?;
    let mut res = Json(AuditLogPage {
        items,
        next_cursor: next.map(|c| c.encode()),
    })
    .into_response();
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(res)
}

/// Zustand des gestreamten Exports.
struct Export {
    store: Store,
    query: AuditQuery,
    remaining: Option<u64>,
    done: bool,
}

/// Streamt alle passenden Einträge als JSON Lines (seitenweise aus der Datenbank).
fn export(store: Store, query: AuditQuery, limit: Option<u64>) -> Response {
    let state = Export {
        store,
        query,
        remaining: limit,
        done: false,
    };
    let stream = futures_util::stream::unfold(state, |mut s| async move {
        if s.done || s.remaining == Some(0) {
            return None;
        }
        let batch = s.remaining.map_or(EXPORT_BATCH, |r| {
            u32::try_from(r.min(u64::from(EXPORT_BATCH))).unwrap_or(EXPORT_BATCH)
        });
        s.query.limit = batch;
        match s.store.query_audit_log(&s.query).await {
            Ok(page) => {
                let mut buf = Vec::new();
                for e in &page.items {
                    if let Err(err) = serde_json::to_writer(&mut buf, e) {
                        tracing::error!(error = %err, "Audit-Export: Eintrag nicht serialisierbar");
                        s.done = true;
                        return Some((Err(std::io::Error::other("Export abgebrochen")), s));
                    }
                    buf.push(b'\n');
                }
                if let Some(r) = s.remaining.as_mut() {
                    *r = r.saturating_sub(page.items.len() as u64);
                }
                match page.next {
                    Some(c) => s.query.cursor = Some(c),
                    None => s.done = true,
                }
                Some((Ok(Bytes::from(buf)), s))
            }
            Err(err) => {
                // Der Status ist schon gesendet: Abbruch der Übertragung zeigt den Fehler an.
                tracing::error!(error = %err, "Audit-Export: Datenbankfehler");
                s.done = true;
                Some((Err(std::io::Error::other("Export abgebrochen")), s))
            }
        }
    });
    let mut res = Body::from_stream(stream).into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(NDJSON));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

/// Prüft die Hash-Kette des Audit-Logs (F116) – nur globaler `admin`/`auditor`.
///
/// Liefert immer `200` mit dem Prüfbericht (`ok: false` bei Abweichungen). `headHash` kann
/// extern gesichert und später verglichen werden (Schutz gegen Neuberechnung der ganzen
/// Kette durch jemanden mit Schreibrechten auf die Datenbank).
#[utoipa::path(
    get, path = "/admin/api/v1/audit-log/verify", tag = "audit-log",
    responses(
        (status = 200, description = "Prüfbericht", body = aigw_store::AuditChainReport),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Nur globaler admin/auditor", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub(crate) async fn verify_audit_log(
    State(st): State<AdminState>,
    Extension(p): Extension<Principal>,
) -> Result<Response, Problem> {
    if visibility(&p)? != AuditScope::All {
        return Err(Problem::forbidden(
            "Prüfung der Hash-Kette: nur globaler admin oder auditor",
        ));
    }
    let report = st.store.verify_audit_chain().await?;
    if !report.ok {
        tracing::error!(
            issues = report.issues.len(),
            "Audit-Log: Hash-Kette verletzt (Manipulation oder Datenverlust)"
        );
    }
    let mut res = Json(report).into_response();
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(res)
}

/// Routen (mit Anmeldung).
pub(crate) fn router() -> Router<AdminState> {
    Router::new()
        .route("/admin/api/v1/audit-log", get(audit_log))
        .route("/admin/api/v1/audit-log/verify", get(verify_audit_log))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use aigw_core::resource::RoleBinding;

    use super::*;
    use crate::auth::AuthMethod;

    fn principal(roles: Vec<RoleBinding>) -> Principal {
        Principal {
            roles,
            ..Principal::global("x", &[], AuthMethod::AdminApiKey)
        }
    }

    fn rb(role: Role, org: Option<&str>, team: Option<&str>) -> RoleBinding {
        RoleBinding {
            role,
            organization: org.map(Into::into),
            team: team.map(Into::into),
        }
    }

    #[test]
    fn visibility_by_role() {
        for r in [Role::Admin, Role::Auditor] {
            assert_eq!(
                visibility(&principal(vec![rb(r, None, None)])).unwrap(),
                AuditScope::All
            );
        }
        assert_eq!(
            visibility(&principal(vec![rb(Role::Viewer, None, None)]))
                .unwrap_err()
                .status,
            403
        );
        assert_eq!(visibility(&principal(vec![])).unwrap_err().status, 403);
        // eingeschränkte Rollen: ihr Bereich; viewer zählt nicht, wirkungslose Rollen auch nicht
        let p = principal(vec![
            rb(Role::OrgAdmin, Some("acme"), None),
            rb(Role::TeamAdmin, Some("other"), Some("ops")),
            rb(Role::Viewer, Some("third"), None),
            rb(Role::TeamAdmin, Some("x"), None),
            rb(Role::Auditor, Some("fourth"), None),
        ]);
        let AuditScope::Within(rules) = visibility(&p).unwrap() else {
            unreachable!()
        };
        assert_eq!(
            rules,
            vec![
                ScopeRule {
                    organization: Some("acme".into()),
                    team: None
                },
                ScopeRule {
                    organization: Some("other".into()),
                    team: Some("ops".into())
                },
                ScopeRule {
                    organization: Some("fourth".into()),
                    team: None
                },
            ]
        );
        // globale Rolle gewinnt
        let p = principal(vec![
            rb(Role::OrgAdmin, Some("acme"), None),
            rb(Role::Auditor, None, None),
        ]);
        assert_eq!(visibility(&p).unwrap(), AuditScope::All);
    }

    #[test]
    fn query_params() {
        let q = AuditLogQuery {
            from: Some("2026-11-13T10:00:00 01:00".into()),
            to: Some("2026-11-14T00:00:00Z".into()),
            actor_type: Some("user, gitops".into()),
            kind: Some("Team,ApiKey".into()),
            order: Some("asc".into()),
            ..AuditLogQuery::default()
        };
        let a = to_query(&q, AuditScope::All).unwrap();
        assert_eq!(a.actor_types, vec![ActorType::User, ActorType::Gitops]);
        assert_eq!(a.kinds, vec!["Team", "ApiKey"]);
        assert!(a.ascending);
        assert_eq!(a.from.unwrap().offset().whole_hours(), 1);
        for bad in [
            AuditLogQuery {
                from: Some("gestern".into()),
                ..AuditLogQuery::default()
            },
            AuditLogQuery {
                actor_type: Some("robot".into()),
                ..AuditLogQuery::default()
            },
            AuditLogQuery {
                order: Some("up".into()),
                ..AuditLogQuery::default()
            },
            AuditLogQuery {
                cursor: Some("kaputt".into()),
                ..AuditLogQuery::default()
            },
        ] {
            assert_eq!(to_query(&bad, AuditScope::All).unwrap_err().status, 400);
        }
        let mut h = HeaderMap::new();
        assert!(!wants_ndjson(&h));
        h.insert(
            header::ACCEPT,
            HeaderValue::from_static("application/json;q=0.5, application/x-ndjson"),
        );
        assert!(wants_ndjson(&h));
    }
}
