// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

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
//! - [`suggest`] – Preisvorschläge (AIGW-85) aus der Azure Retail Prices API und dem
//!   mitgelieferten Preiskatalog (`catalog/prices.yaml`).
//!
//! Eigenes Crate statt Modul im Proxy, weil drei Schichten dieselbe Logik brauchen: der Proxy
//! (prüfen und buchen), `aigw-store` (implementiert [`BudgetLedger`] für SQLite/Postgres) und
//! die Admin-API (Zeiträume für `GET /admin/api/v1/usage/budgets`). Das Crate selbst hat
//! keine Datenbank-Abhängigkeit; ausgehende HTTP-Aufrufe nur in [`suggest`] über
//! `aigw-outbound`.
//!
//! Ausführlich: `docs/konzept/11-limits-und-budgets.md`.

pub mod budget;
pub mod calendar;
pub mod cost;
pub mod suggest;
pub mod webhook;

pub use budget::{
    BudgetEvent, BudgetEventKind, BudgetEventSink, BudgetExceeded, BudgetHold, BudgetKey,
    BudgetLedger, BudgetLevel, BudgetSyncTask, BudgetTarget, Budgets, ExceededKind, LedgerError,
    LogEvents, MemoryLedger, MemoryShared, SharedBudgets, SharedItem, SharedReserve,
};
pub use calendar::{Calendar, CalendarError, PeriodRange, period_code, period_from_code};
pub use cost::{cost, cost_at, cost_breakdown_at, cost_with_breakdown};
pub use webhook::WebhookEvents;
