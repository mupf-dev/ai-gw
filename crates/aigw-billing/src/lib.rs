// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Kostenabrechnung und Budgets von ai-gw (Architektur §8, AIGW-31).
//!
//! - [`cost`] – Kosten je Anfrage aus der versionierten Preistabelle des Snapshots
//!   (`price_at` zum Anfragezeitpunkt; Token-Arten Eingabe, Ausgabe, Cache lesen/schreiben,
//!   Reasoning; Festkomma [`Amount`](aigw_core::resource::Amount)).
//! - [`calendar`] – Budget-Zeiträume (Kalendertag/-woche/-monat) in konfigurierbarer
//!   Zeitzone.
//! - [`budget`] – Budgetstände im RAM mit Abgleich gegen ein [`BudgetLedger`] (Tabelle
//!   `budget_usage`), harte Grenze und Warnschwellen.
//!
//! Eigenes Crate statt Modul im Proxy, weil drei Schichten dieselbe Logik brauchen: der Proxy
//! (prüfen und buchen), `aigw-store` (implementiert [`BudgetLedger`] für SQLite/Postgres) und
//! die Admin-API (Zeiträume für `GET /admin/api/v1/usage/budgets`). Das Crate selbst hat
//! keine Datenbank- oder HTTP-Abhängigkeit.
//!
//! Ausführlich: `docs/konzept/11-limits-und-budgets.md`.

pub mod budget;
pub mod calendar;
pub mod cost;
pub mod webhook;

pub use budget::{
    BudgetEvent, BudgetEventKind, BudgetEventSink, BudgetExceeded, BudgetHold, BudgetKey,
    BudgetLedger, BudgetLevel, BudgetSyncTask, BudgetTarget, Budgets, ExceededKind, LedgerError,
    LogEvents, MemoryLedger, MemoryShared, SharedBudgets, SharedItem, SharedReserve,
};
pub use calendar::{Calendar, CalendarError, PeriodRange, period_code, period_from_code};
pub use cost::{cost, cost_at};
pub use webhook::WebhookEvents;
