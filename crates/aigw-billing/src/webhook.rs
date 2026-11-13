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

use aigw_webhook::{Event, EventDispatcher};
use serde_json::json;

use crate::budget::{BudgetEvent, BudgetEventKind, BudgetEventSink, LogEvents};
use crate::calendar::{Calendar, period_code};

const DAY_MS: i64 = 86_400_000;
const CENTURY_MS: i64 = 100 * 366 * DAY_MS;

/// Budget-Ereignisse an Log **und** Ereignis-Webhooks.
#[derive(Debug, Clone)]
pub struct WebhookEvents {
    dispatcher: EventDispatcher,
    calendar: Calendar,
}

impl WebhookEvents {
    /// Neuer Empfänger; `calendar` wie bei [`crate::Budgets`] (Ende des Zeitraums).
    pub fn new(dispatcher: EventDispatcher, calendar: Calendar) -> Self {
        Self {
            dispatcher,
            calendar,
        }
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
        let data = json!({
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
