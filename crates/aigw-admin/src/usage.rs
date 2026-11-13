// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Verbrauch lesen (AIGW-31).
//!
//! - `GET /admin/api/v1/usage/budgets?level=` – alle Budgets (Organisationen, Teams, aktive
//!   API-Keys) mit Stand im laufenden Zeitraum. Gefiltert nach Leserecht (`Read`) auf dem
//!   jeweiligen Ziel: Organisation, Team bzw. Keys im Team (ohne Team: global).
//!
//! Der Stand kommt aus `budget_usage` (instanzübergreifend). Die Gateways schreiben ihre
//! Deltas im Abstand `billing.budget_sync_ms` (Standard 1 s), der Wert hinkt daher bis zu
//! einem Abgleichintervall hinterher. Die Zeiträume rechnet derselbe Kalender wie der Proxy
//! (`billing.timezone`).

// `#[utoipa::path(params(…))]` erzeugt für IntoParams-Typen qualifizierte Pfade.
#![allow(unused_qualifications)]

use std::fmt;
use std::sync::Arc;

use aigw_billing::{BudgetKey, BudgetLevel, Calendar};
use aigw_core::resource::{
    Amount, ApiKeyState, Budget, BudgetPeriod, Id, OrganizationSpec, TeamSpec,
};
use aigw_store::Store;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Extension, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use utoipa::{IntoParams, ToSchema};

use crate::auth::{AdminAuthenticator, Permission, Principal, require_principal};
use crate::problem::Problem;
use crate::rbac::Target;

/// Pfad der Budget-Übersicht.
pub const BUDGETS_PATH: &str = "/admin/api/v1/usage/budgets";

/// Zustand der Verbrauchs-Routen.
#[derive(Clone)]
pub struct UsageState {
    /// Datenbank.
    pub store: Store,
    /// Kalender der Budget-Zeiträume (`billing.timezone`).
    pub calendar: Arc<Calendar>,
    /// Anmeldung (dieselbe wie für die übrige Admin-API).
    pub auth: Arc<dyn AdminAuthenticator>,
}

impl fmt::Debug for UsageState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UsageState")
            .field("store", &self.store)
            .field("timezone", &self.calendar.timezone())
            .finish_non_exhaustive()
    }
}

/// Ebene eines Budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum BudgetLevelName {
    /// Organisation.
    Organization,
    /// Team.
    Team,
    /// API-Key.
    ApiKey,
}

impl From<BudgetLevelName> for BudgetLevel {
    fn from(l: BudgetLevelName) -> Self {
        match l {
            BudgetLevelName::Organization => Self::Organization,
            BudgetLevelName::Team => Self::Team,
            BudgetLevelName::ApiKey => Self::ApiKey,
        }
    }
}

/// Parameter der Budget-Übersicht.
#[derive(Debug, Deserialize, IntoParams)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct BudgetQuery {
    /// Nur diese Ebene.
    #[serde(default)]
    level: Option<BudgetLevelName>,
}

/// Stand eines Budgets im laufenden Zeitraum.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BudgetStatus {
    /// Ebene.
    pub level: BudgetLevelName,
    /// ID des Ziels.
    #[schema(value_type = String, format = Uuid)]
    pub target_id: Id,
    /// Name des Ziels.
    pub target_name: String,
    /// Zeitraum.
    pub period: BudgetPeriod,
    /// Beginn des laufenden Zeitraums (RFC 3339, UTC).
    pub period_start: String,
    /// Ende des laufenden Zeitraums (exklusive; fehlt bei `total`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub period_end: Option<String>,
    /// Betrag je Zeitraum.
    pub amount: Amount,
    /// Verbraucht im laufenden Zeitraum.
    pub used: Amount,
    /// Verbleibend (nicht negativ).
    pub remaining: Amount,
    /// Verbrauch in Prozent des Betrags (ganzzahlig abgerundet; 0 bei Betrag 0).
    pub percent: u64,
    /// Warnschwellen in Prozent.
    pub warn_at_percent: Vec<u8>,
    /// Harte Grenze (Ablehnung mit `budget_exceeded`).
    pub hard_limit: bool,
    /// Betrag erreicht.
    pub exhausted: bool,
}

/// Budget-Übersicht.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BudgetList {
    /// Zeitzone der Zeiträume (`billing.timezone`).
    pub timezone: String,
    /// Zeitpunkt der Auswertung (RFC 3339).
    pub at: String,
    /// Budgets (Organisationen, Teams, aktive API-Keys; je nach Name sortiert).
    pub items: Vec<BudgetStatus>,
}

fn rfc3339(ms: i64) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .ok()
        .and_then(|t| t.format(&Rfc3339).ok())
        .unwrap_or_default()
}

/// Budgets mit Verbrauch im laufenden Zeitraum.
#[utoipa::path(
    get, path = "/admin/api/v1/usage/budgets", tag = "usage", params(BudgetQuery),
    responses(
        (status = 200, description = "Budgets mit Stand", body = BudgetList),
        (status = 400, description = "Ungültige Parameter", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn list_budgets(
    State(st): State<UsageState>,
    Extension(p): Extension<Principal>,
    q: Result<Query<BudgetQuery>, QueryRejection>,
) -> Result<Json<BudgetList>, Problem> {
    let Query(q) = q?;
    let want = |l: BudgetLevelName| q.level.is_none_or(|w| w == l);
    let read = |t: &Target| p.can(Permission::Read, t);

    let mut targets: Vec<(BudgetLevelName, Id, String, Budget)> = Vec::new();
    if want(BudgetLevelName::Organization) {
        for o in st.store.list::<OrganizationSpec>().await? {
            if let Some(b) = o.spec.budget
                && read(&Target::Organization(o.metadata.name.clone()))
            {
                targets.push((BudgetLevelName::Organization, o.id, o.metadata.name, b));
            }
        }
    }
    let teams = if want(BudgetLevelName::Team) || want(BudgetLevelName::ApiKey) {
        st.store.list::<TeamSpec>().await?
    } else {
        Vec::new()
    };
    if want(BudgetLevelName::Team) {
        for t in &teams {
            if let Some(b) = &t.spec.budget
                && read(&Target::team(
                    t.spec.organization.clone(),
                    t.metadata.name.clone(),
                ))
            {
                targets.push((
                    BudgetLevelName::Team,
                    t.id,
                    t.metadata.name.clone(),
                    b.clone(),
                ));
            }
        }
    }
    if want(BudgetLevelName::ApiKey) {
        let org_of = |team: &str| {
            teams
                .iter()
                .find(|t| t.metadata.name == team)
                .map(|t| t.spec.organization.clone())
                .unwrap_or_default()
        };
        for k in st.store.list_api_keys().await? {
            let target = match &k.spec.team {
                Some(t) => Target::in_team(org_of(t), t.clone()),
                None => Target::Global,
            };
            if k.status.state == ApiKeyState::Active
                && read(&target)
                && let Some(b) = k.spec.budget
            {
                targets.push((BudgetLevelName::ApiKey, k.id, k.metadata.name, b));
            }
        }
    }

    let now = OffsetDateTime::now_utc();
    let now_ms = i64::try_from(now.unix_timestamp_nanos() / 1_000_000).unwrap_or(i64::MAX);
    let ranges: Vec<_> = targets
        .iter()
        .map(|(_, _, _, b)| st.calendar.period(b.period, now_ms))
        .collect();
    let keys: Vec<BudgetKey> = targets
        .iter()
        .zip(&ranges)
        .map(|((level, id, _, b), r)| BudgetKey {
            level: (*level).into(),
            target: *id,
            period: b.period,
            period_start_ms: r.start_ms,
        })
        .collect();
    let used = st.store.budget_usage(&keys).await?;

    let items = targets
        .into_iter()
        .zip(ranges)
        .zip(used)
        .map(|(((level, id, name, b), r), used)| {
            let amount = b.amount.nanos();
            let percent = if amount > 0 {
                u64::try_from(i128::from(used.max(0)) * 100 / i128::from(amount))
                    .unwrap_or(u64::MAX)
            } else {
                0
            };
            BudgetStatus {
                level,
                target_id: id,
                target_name: name,
                period: b.period,
                period_start: rfc3339(r.start_ms),
                period_end: r.end_ms.map(rfc3339),
                amount: b.amount,
                used: Amount::from_nanos(used),
                remaining: Amount::from_nanos(amount.saturating_sub(used).max(0)),
                percent,
                warn_at_percent: b.warn_at_percent,
                hard_limit: b.hard_limit,
                exhausted: used >= amount,
            }
        })
        .collect();
    Ok(Json(BudgetList {
        timezone: st.calendar.timezone().to_owned(),
        at: rfc3339(now_ms),
        items,
    }))
}

/// Router der Verbrauchs-Routen (mit Anmeldung).
pub fn router(state: UsageState) -> Router {
    Router::new()
        .route(BUDGETS_PATH, get(list_budgets))
        .route_layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state.auth),
            require_principal,
        ))
        .with_state(state)
}
