// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Budget-Ereignisse als Webhooks (AIGW-61): [`BudgetEventSink`], der jedes Ereignis
//! protokolliert ([`LogEvents`]) und an einen [`EventDispatcher`] übergibt.
//!
//! Typen: `budget.warning` (Warnschwelle, `data.percent`) und `budget.exhausted`. Der
//! Dedup-Schlüssel `budget:<ebene>:<ziel-id>:<zeitraum>:<beginn-ms>:<warning-<p>|exhausted>`
//! sorgt dafür, dass jede Schwelle je Zeitraum **im Cluster** nur einmal gemeldet wird – auch
//! wenn mehrere Instanzen (oder eine neu gestartete) sie erreichen. Gültig bis Ende des
//! Zeitraums + 1 Tag (`total`: 100 Jahre).
//!
//! **Namen (F154):** Mit [`WebhookEvents::with_snapshot`] tragen die Daten zusätzlich die
//! Namen aus dem aktuellen Konfigurations-Snapshot: `targetName` (Organisation, Team bzw.
//! Key) sowie bei Teams `organization` und bei Keys `team`/`organization`. Fehlt das Ziel im
//! Snapshot (z. B. gerade gelöscht), bleiben die Felder weg.

use aigw_core::snapshot::{Snapshot, SnapshotHandle};
use aigw_webhook::{Event, EventDispatcher};
use serde_json::json;

use crate::budget::{BudgetEvent, BudgetEventKind, BudgetEventSink, BudgetLevel, LogEvents};
use crate::calendar::{Calendar, period_code};

const DAY_MS: i64 = 86_400_000;
const CENTURY_MS: i64 = 100 * 366 * DAY_MS;

/// Budget-Ereignisse an Log **und** Ereignis-Webhooks.
#[derive(Debug, Clone)]
pub struct WebhookEvents {
    dispatcher: EventDispatcher,
    calendar: Calendar,
    snapshot: Option<SnapshotHandle>,
}

/// Namen zu einem Budget-Ziel aus dem Snapshot: (Ziel, Team, Organisation).
fn names(
    snap: &Snapshot,
    level: BudgetLevel,
    target: aigw_core::resource::Id,
) -> (Option<String>, Option<String>, Option<String>) {
    match level {
        BudgetLevel::Organization => (
            snap.organizations
                .get(&target)
                .map(|o| o.metadata.name.clone()),
            None,
            None,
        ),
        BudgetLevel::Team => match snap.teams.get(&target) {
            Some(t) => (
                Some(t.metadata.name.clone()),
                None,
                Some(t.spec.organization.clone()),
            ),
            None => (None, None, None),
        },
        BudgetLevel::ApiKey => match snap.keys.values().find(|k| k.key.id == target) {
            Some(k) => (
                Some(k.key.metadata.name.clone()),
                k.team.as_ref().map(|t| t.metadata.name.clone()),
                k.organization
                    .as_ref()
                    .map(|o| o.metadata.name.clone())
                    .or_else(|| Some(k.key.spec.organization.clone())),
            ),
            None => (None, None, None),
        },
    }
}

impl WebhookEvents {
    /// Neuer Empfänger; `calendar` wie bei [`crate::Budgets`] (Ende des Zeitraums).
    pub fn new(dispatcher: EventDispatcher, calendar: Calendar) -> Self {
        Self {
            dispatcher,
            calendar,
            snapshot: None,
        }
    }

    /// Namen der Ziele aus dem aktuellen Snapshot ergänzen (F154).
    #[must_use]
    pub fn with_snapshot(mut self, snapshot: SnapshotHandle) -> Self {
        self.snapshot = Some(snapshot);
        self
    }

    /// Ereignis für die Webhook-Zustellung (Typ, Daten, Dedup-Schlüssel).
    pub fn to_event(&self, ev: &BudgetEvent) -> Event {
        let k = &ev.key;
        let range = self.calendar.period(k.period, k.period_start_ms);
        let (kind, suffix, percent) = match ev.kind {
            BudgetEventKind::Warning { percent } => (
                "budget.warning",
                format!("warning-{percent}"),
                Some(percent),
            ),
            BudgetEventKind::Exhausted => ("budget.exhausted", "exhausted".to_owned(), None),
        };
        let period = period_code(k.period);
        let key = format!(
            "budget:{}:{}:{period}:{}:{suffix}",
            k.level.as_str(),
            k.target,
            k.period_start_ms
        );
        let expires = range.end_ms.map_or_else(
            || ev_now().saturating_add(CENTURY_MS),
            |e| e.saturating_add(DAY_MS),
        );
        let mut data = json!({
            "level": k.level.as_str(),
            "targetId": k.target,
            "period": period,
            "periodStartMs": k.period_start_ms,
            "periodEndMs": range.end_ms,
            "percent": percent,
            "amount": ev.amount,
            "used": ev.used,
            "hardLimit": ev.hard_limit,
        });
        if let Some(h) = &self.snapshot {
            let (target, team, org) = names(&h.load(), k.level, k.target);
            for (field, v) in [
                ("targetName", target),
                ("team", team),
                ("organization", org),
            ] {
                if let (Some(v), Some(obj)) = (v, data.as_object_mut()) {
                    obj.insert(field.to_owned(), serde_json::Value::String(v));
                }
            }
        }
        Event::new(kind, data).with_dedup(key, expires)
    }
}

fn ev_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

impl BudgetEventSink for WebhookEvents {
    fn on_event(&self, ev: &BudgetEvent) {
        LogEvents.on_event(ev);
        self.dispatcher.emit(self.to_event(ev));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;

    use aigw_core::resource::{
        ApiKey, ApiKeySpec, ApiKeyState, ApiKeyStatus, Object, ObjectMeta, OrganizationSpec,
        TeamSpec, new_id,
    };
    use aigw_core::snapshot::{KeyCtx, KeyHash};

    use super::*;

    #[test]
    fn names_from_snapshot() {
        let org = Arc::new(Object {
            id: new_id(),
            metadata: ObjectMeta::named("acme"),
            spec: OrganizationSpec::default(),
        });
        let team = Arc::new(Object {
            id: new_id(),
            metadata: ObjectMeta::named("platform"),
            spec: TeamSpec {
                organization: "acme".into(),
                ..TeamSpec::default()
            },
        });
        let key = ApiKey {
            id: new_id(),
            metadata: ObjectMeta::named("ci"),
            spec: ApiKeySpec {
                organization: "acme".into(),
                team: Some("platform".into()),
                ..ApiKeySpec::default()
            },
            status: ApiKeyStatus {
                prefix: "sk".into(),
                state: ApiKeyState::Active,
                last_used_at: None,
            },
        };
        let key_id = key.id;
        let mut snap = Snapshot::default();
        snap.organizations.insert(org.id, Arc::clone(&org));
        snap.teams.insert(team.id, Arc::clone(&team));
        snap.keys.insert(
            KeyHash([7; 32]),
            Arc::new(KeyCtx {
                key,
                team: Some(Arc::clone(&team)),
                organization: Some(Arc::clone(&org)),
                expires_at: None,
            }),
        );
        let s = |v: &str| Some(v.to_owned());
        assert_eq!(
            names(&snap, BudgetLevel::Organization, org.id),
            (s("acme"), None, None)
        );
        assert_eq!(
            names(&snap, BudgetLevel::Team, team.id),
            (s("platform"), None, s("acme"))
        );
        assert_eq!(
            names(&snap, BudgetLevel::ApiKey, key_id),
            (s("ci"), s("platform"), s("acme"))
        );
        assert_eq!(
            names(&snap, BudgetLevel::ApiKey, new_id()),
            (None, None, None)
        );
    }
}
