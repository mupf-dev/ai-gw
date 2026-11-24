// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Kostenanalyse / FinOps (AIGW-117).
//!
//! - `GET /admin/api/v1/costs/summary` – Gesamtkosten mit Vorperiode, Kosten je Anfrage und je
//!   1000 Ein-/Ausgabe-Tokens, Aufschlüsselung nach Token-Art (Eingabe, Cache lesen/schreiben,
//!   Ausgabe, Plugins, Rest), Ersparnis durch Prompt-Caching des Anbieters und – als Schätzung –
//!   durch den Antwort-Cache des Gateways
//! - `GET /admin/api/v1/costs/breakdown` – Kostentreiber einer Dimension mit Vorperiode
//!   (Delta absolut und in Prozent)
//! - `GET /admin/api/v1/costs/budgets` – Budgets mit Prognose bis Periodenende (Durchschnitt der
//!   letzten N Tage bzw. lineare Hochrechnung) und – für ein einzelnes Budget – Burn-down
//! - `GET /admin/api/v1/costs/export` – Chargeback-Export (CSV oder JSON, gestreamt) je
//!   Zeitraum, Abschnitt (gesamt, Tag, Woche, Monat) und Dimension mit Kostenstelle, Labels und
//!   Metadaten der Objekte sowie Währung
//!
//! Der Kostenverlauf gestapelt nach Dimension kommt unverändert aus
//! `GET /admin/api/v1/analytics/timeseries?metric=cost&groupBy=…` (keine zweite Zeitreihe).
//!
//! Quelle sind die Minuten- und Stunden-Rollups (`aigw_store::analytics`) mit den Spalten der
//! Migration 0013 (Kosten je Token-Art, Plugin-Kosten und -Tokens, Ersparnis durch
//! Prompt-Caching). **Geltungsbereich** wie Analytics ([`crate::analytics`]): globale Leserollen
//! sehen alles, sonst nur Teams mit Leserecht und – mit an die Organisation gebundener Rolle –
//! teamlose Keys dieser Organisation; ausdrücklich gewählte fremde Teams/Keys → `403`. Budgets
//! nur mit Leserecht auf das Ziel (wie `/usage/budgets`).
//!
//! Beträge sind exakte Dezimaltexte in der Abrechnungswährung (`billing.currency`, nur
//! Beschriftung – das Gateway rechnet nicht um).

// `#[utoipa::path(params(…))]` erzeugt für IntoParams-Typen qualifizierte Pfade.
#![allow(unused_qualifications)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};
use std::sync::Arc;

use aigw_billing::Calendar;
use aigw_core::resource::{Amount, BudgetPeriod};
use aigw_store::Store;
use aigw_store::analytics::{Grouping, KeyFilter, RollupDim, RollupQuery};
use aigw_store::observe::{MINUTE_MS, RollupCounts};
use axum::body::{Body, Bytes};
use axum::extract::rejection::QueryRejection;
use axum::extract::{Extension, Query, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::analytics::{
    Dimension, Directory, Filters, Scope, Step, bucket_edges, clamp_limit, context, group_totals,
    now_ms, parse_time, rfc3339, scope_of, totals,
};
use crate::auth::{AdminAuthenticator, Principal, require_principal};
use crate::problem::Problem;
use crate::usage::{BudgetLevelName, BudgetStatus, collect_budgets};

/// Pfad der Kosten-Kennzahlen.
pub const SUMMARY_PATH: &str = "/admin/api/v1/costs/summary";
/// Pfad der Kostentreiber.
pub const BREAKDOWN_PATH: &str = "/admin/api/v1/costs/breakdown";
/// Pfad der Budget-Prognosen.
pub const BUDGETS_PATH: &str = "/admin/api/v1/costs/budgets";
/// Pfad des Chargeback-Exports.
pub const EXPORT_PATH: &str = "/admin/api/v1/costs/export";

/// Standard-Zeitraum der Kostenanalyse ohne `from` (30 Tage).
pub const DEFAULT_RANGE_MS: i64 = 30 * DAY_MS;
/// Längster Zeitraum (wie Analytics, Standard-Aufbewahrung der Rollups).
pub const MAX_RANGE_MS: i64 = 400 * DAY_MS;
/// Standard-Rückblick der Budget-Prognose (Tage).
pub const DEFAULT_LOOKBACK_DAYS: u32 = 7;

const DAY_MS: i64 = 86_400_000;

/// Zustand der Kosten-Routen.
#[derive(Clone)]
pub struct CostsState {
    /// Datenbank.
    pub store: Store,
    /// Anmeldung (dieselbe wie für die übrige Admin-API).
    pub auth: Arc<dyn AdminAuthenticator>,
    /// Kalender für Tage, Wochen, Monate und Budget-Zeiträume (`billing.timezone`).
    pub calendar: Arc<Calendar>,
    /// `observe.rollups` (F33): ohne Rollups gibt es keine Daten (`rollups: false`).
    pub rollups: bool,
    /// Abrechnungswährung (`billing.currency`).
    pub currency: String,
    /// Metadaten-Schlüssel der Kostenstelle (`billing.cost_center_key`).
    pub cost_center_key: String,
}

impl fmt::Debug for CostsState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CostsState")
            .field("store", &self.store)
            .field("currency", &self.currency)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------------------------
// Parameter
// ---------------------------------------------------------------------------------------------

/// Parameter der Kosten-Kennzahlen.
#[derive(Debug, Deserialize, IntoParams)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct CostSummaryQuery {
    /// Beginn (RFC 3339; Standard: `to` − 30 Tage). Auf die Minute abgerundet.
    from: Option<String>,
    /// Ende (RFC 3339, exklusive; Standard: jetzt). Auf die Minute aufgerundet.
    to: Option<String>,
    /// Modellgruppen.
    model_group: Option<String>,
    /// Deployments (Name oder ID).
    deployment: Option<String>,
    /// Anbieter.
    provider: Option<String>,
    /// Teams (Name oder ID).
    team: Option<String>,
    /// API-Keys (Name oder ID).
    key: Option<String>,
    /// Organisationen (Name oder ID).
    organization: Option<String>,
}

/// Parameter der Kostentreiber.
#[derive(Debug, Deserialize, IntoParams)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct CostBreakdownQuery {
    /// Dimension (nicht `error_class`).
    dimension: Dimension,
    /// Beginn (RFC 3339; Standard: `to` − 30 Tage).
    from: Option<String>,
    /// Ende (RFC 3339, exklusive; Standard: jetzt).
    to: Option<String>,
    /// Anzahl (1–100, Standard 10); absteigend nach Kosten im Zeitraum.
    limit: Option<u32>,
    /// Modellgruppen.
    model_group: Option<String>,
    /// Deployments (Name oder ID).
    deployment: Option<String>,
    /// Anbieter.
    provider: Option<String>,
    /// Teams (Name oder ID).
    team: Option<String>,
    /// API-Keys (Name oder ID).
    key: Option<String>,
    /// Organisationen (Name oder ID).
    organization: Option<String>,
}

/// Verfahren der Budget-Prognose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ForecastMethod {
    /// Durchschnittliche Kosten je Tag der letzten `lookbackDays` Tage (auch vor dem
    /// Periodenbeginn) bis zum Periodenende fortgeschrieben.
    Average,
    /// Lineare Hochrechnung: Verbrauch seit Periodenbeginn / verstrichene Zeit.
    Linear,
}

/// Parameter der Budget-Prognosen.
#[derive(Debug, Deserialize, IntoParams)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct BudgetForecastQuery {
    /// Nur diese Ebene.
    level: Option<BudgetLevelName>,
    /// Nur dieses Ziel (Name oder ID; erfordert `level`) – dann mit Burn-down.
    target: Option<String>,
    /// Verfahren (Standard `average`).
    method: Option<ForecastMethod>,
    /// Rückblick für `average` in Tagen (1–90, Standard 7).
    lookback_days: Option<u32>,
}

/// Dateiformat des Exports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    /// CSV (RFC 4180, Kopfzeile, UTF-8, Trennzeichen Komma).
    Csv,
    /// JSON-Objekt mit `rows`.
    Json,
}

/// Abschnitte des Exports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Granularity {
    /// Ein Abschnitt für den ganzen Zeitraum.
    Total,
    /// Kalendertage in `billing.timezone`.
    Day,
    /// Kalenderwochen (ab Montag) in `billing.timezone`.
    Week,
    /// Kalendermonate in `billing.timezone`.
    Month,
}

/// Parameter des Chargeback-Exports.
#[derive(Debug, Deserialize, IntoParams)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct CostExportQuery {
    /// Dimension der Zeilen (Standard `team`; nicht `error_class`).
    dimension: Option<Dimension>,
    /// Format (Standard `csv`).
    format: Option<ExportFormat>,
    /// Abschnitte (Standard `month`).
    granularity: Option<Granularity>,
    /// Beginn (RFC 3339; Standard: `to` − 30 Tage).
    from: Option<String>,
    /// Ende (RFC 3339, exklusive; Standard: jetzt).
    to: Option<String>,
    /// Modellgruppen.
    model_group: Option<String>,
    /// Deployments (Name oder ID).
    deployment: Option<String>,
    /// Anbieter.
    provider: Option<String>,
    /// Teams (Name oder ID).
    team: Option<String>,
    /// API-Keys (Name oder ID).
    key: Option<String>,
    /// Organisationen (Name oder ID).
    organization: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Antworten
// ---------------------------------------------------------------------------------------------

/// Betrag mit Vergleich zur Vorperiode.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CostDelta {
    /// Wert im Zeitraum.
    pub amount: Amount,
    /// Wert in der Vorperiode.
    pub previous: Amount,
    /// Änderung absolut (Wert − Vorperiode, negativ = gesunken).
    pub delta: Amount,
    /// Änderung in Prozent; `null` ohne Vorwert.
    pub delta_percent: Option<f64>,
}

/// Teil der Kostenaufschlüsselung.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CostPartKind {
    /// Eingabe-Tokens ohne Anbieter-Cache.
    Input,
    /// Aus dem Anbieter-Cache gelesene Tokens.
    CacheRead,
    /// In den Anbieter-Cache geschriebene Tokens.
    CacheWrite,
    /// Ausgabe-Tokens inklusive Reasoning.
    Output,
    /// Modellaufrufe von Plugins (content-safety, schema-guard-Wiederholungen).
    Plugins,
    /// Nicht aufgeschlüsselt: Preise je Anfrage, Bild, Audio-Minute oder Zeichen sowie Daten
    /// vor der Aufschlüsselung (Migration 0013).
    Other,
}

/// Ein Teil der Kosten.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CostPart {
    /// Art.
    pub part: CostPartKind,
    /// Betrag.
    pub amount: Amount,
    /// Anteil an den Gesamtkosten (0–1); `null` ohne Kosten.
    pub share: Option<f64>,
}

/// Ersparnisse.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CostSavings {
    /// Prompt-Caching des Anbieters: gelesene Cache-Tokens × (Eingabepreis − Cache-Lesepreis),
    /// exakt zum Preis der Anfrage (nur Modelle mit eigenem Cache-Lesepreis).
    pub provider_cache: Amount,
    /// Aus dem Anbieter-Cache gelesene Tokens.
    pub provider_cache_tokens: i64,
    /// **Schätzung** der Ersparnis durch den Antwort-Cache des Gateways: Treffer × mittlere
    /// Kosten einer bezahlten Anfrage derselben Modellgruppe im Zeitraum (ohne Plugin-Kosten).
    /// `null` ohne Treffer mit Vergleichsbasis.
    pub gateway_cache_estimate: Option<Amount>,
    /// Treffer im Antwort-Cache.
    pub gateway_cache_hits: i64,
    /// Treffer ohne Vergleichsbasis (Modellgruppe ohne bezahlte Anfrage im Zeitraum) – nicht
    /// in der Schätzung enthalten.
    pub gateway_cache_hits_unestimated: i64,
}

/// Kosten der Plugin-Modellaufrufe (in den Gesamtkosten enthalten).
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginCosts {
    /// Betrag.
    pub amount: Amount,
    /// Eingabe-Tokens.
    pub input_tokens: i64,
    /// Ausgabe-Tokens.
    pub output_tokens: i64,
    /// Anteil an den Gesamtkosten (0–1).
    pub share: Option<f64>,
}

/// Kosten-Kennzahlen.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CostSummary {
    /// Beginn.
    pub from: String,
    /// Ende (exklusive).
    pub to: String,
    /// Beginn der Vorperiode (gleich lang, direkt davor).
    pub previous_from: String,
    /// Ende der Vorperiode (= `from`).
    pub previous_to: String,
    /// Abrechnungswährung (ISO 4217).
    pub currency: String,
    /// `false`, wenn `observe.rollups` aus ist (F33) – dann gibt es keine Daten.
    pub rollups: bool,
    /// Gesamtkosten (inkl. Plugins).
    pub total: CostDelta,
    /// Anfragen im Zeitraum.
    pub requests: i64,
    /// Anfragen in der Vorperiode.
    pub previous_requests: i64,
    /// Kosten je Anfrage; `null` ohne Anfragen.
    pub cost_per_request: Option<CostDelta>,
    /// Eingabe-Tokens (inkl. Cache).
    pub input_tokens: i64,
    /// Ausgabe-Tokens (inkl. Reasoning).
    pub output_tokens: i64,
    /// Aus dem Anbieter-Cache gelesene Tokens.
    pub cached_tokens: i64,
    /// In den Anbieter-Cache geschriebene Tokens.
    pub cache_write_tokens: i64,
    /// Kosten je 1000 Eingabe-Tokens (Eingabe + Cache lesen/schreiben); `null` ohne Tokens.
    pub cost_per_1k_input_tokens: Option<Amount>,
    /// Kosten je 1000 Ausgabe-Tokens; `null` ohne Tokens.
    pub cost_per_1k_output_tokens: Option<Amount>,
    /// Aufschlüsselung (Summe = `total.amount`).
    pub breakdown: Vec<CostPart>,
    /// Ersparnisse.
    pub savings: CostSavings,
    /// Plugin-Modellaufrufe.
    pub plugins: PluginCosts,
}

/// Ein Kostentreiber.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CostDriver {
    /// Schlüssel (ID bzw. Name wie bei Analytics; `""` = ohne Wert).
    pub key: String,
    /// Anzeigename (fehlt bei unbekannten bzw. gelöschten Objekten).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Kosten mit Vorperiode.
    pub cost: CostDelta,
    /// Anteil an den Gesamtkosten im Zeitraum (0–1).
    pub share: Option<f64>,
    /// Anfragen.
    pub requests: i64,
    /// Eingabe-Tokens.
    pub input_tokens: i64,
    /// Ausgabe-Tokens.
    pub output_tokens: i64,
    /// Kosten je Anfrage.
    pub cost_per_request: Option<Amount>,
    /// Davon Plugin-Modellaufrufe.
    pub plugin_cost: Amount,
}

/// Kostentreiber einer Dimension.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CostDrivers {
    /// Dimension.
    pub dimension: Dimension,
    /// Beginn.
    pub from: String,
    /// Ende (exklusive).
    pub to: String,
    /// Beginn der Vorperiode.
    pub previous_from: String,
    /// Abrechnungswährung.
    pub currency: String,
    /// `false`, wenn `observe.rollups` aus ist.
    pub rollups: bool,
    /// Gesamtkosten mit Vorperiode (alle Gruppen, nicht nur die gelisteten).
    pub total: CostDelta,
    /// Einträge absteigend nach Kosten (bei Gleichstand nach Vorperiode, Schlüssel).
    pub items: Vec<CostDriver>,
}

/// Zustand eines Budgets laut Prognose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BudgetHealth {
    /// Unter der ersten Warnschwelle, keine Überschreitung erwartet.
    Ok,
    /// Erste Warnschwelle erreicht, keine Überschreitung erwartet.
    Warning,
    /// Überschreitung bis Periodenende erwartet (Prognose).
    AtRisk,
    /// Betrag erreicht.
    Exhausted,
}

/// Prognose eines Budgets (Hochrechnung, keine Zusage).
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BudgetProjection {
    /// Verfahren.
    pub method: ForecastMethod,
    /// Rückblick in Tagen (nur `average`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lookback_days: Option<u32>,
    /// Kosten je Tag laut Verfahren.
    pub daily_rate: Amount,
    /// Erwarteter Verbrauch am Periodenende; `null` bei `total`.
    pub projected: Option<Amount>,
    /// Erwarteter Verbrauch in Prozent des Betrags (abgerundet); `null` bei `total`.
    pub projected_percent: Option<u64>,
    /// Voraussichtlicher Zeitpunkt der Überschreitung (vor Periodenende); `null`, wenn keine
    /// erwartet wird oder der Betrag schon erreicht ist.
    pub exceeds_at: Option<String>,
    /// Überschreitung bis Periodenende erwartet.
    pub will_exceed: bool,
}

/// Punkt des Burn-downs.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BurnPoint {
    /// Ende des Abschnitts (RFC 3339).
    pub t: String,
    /// Kumulierte Kosten laut Rollups bis `t` (vergangene Abschnitte).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spent: Option<Amount>,
    /// Kumulierte Kosten laut Prognose bis `t` (künftige Abschnitte, ab dem Stand jetzt).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projected: Option<Amount>,
}

/// Budget mit Prognose.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BudgetForecast {
    /// Stand (wie `/usage/budgets`; Verbrauch aus `budget_usage`).
    pub budget: BudgetStatus,
    /// Zustand.
    pub health: BudgetHealth,
    /// Prognose.
    pub forecast: BudgetProjection,
    /// Burn-down je Tag (bei Tagesbudgets je Stunde) über den laufenden Zeitraum; nur mit
    /// `target`, leer bei `total`.
    pub burn_down: Vec<BurnPoint>,
}

/// Budget-Prognosen.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BudgetForecastList {
    /// Zeitzone der Zeiträume (`billing.timezone`).
    pub timezone: String,
    /// Zeitpunkt der Auswertung.
    pub at: String,
    /// Abrechnungswährung.
    pub currency: String,
    /// `false`, wenn `observe.rollups` aus ist (Prognose `average` und Burn-down dann leer).
    pub rollups: bool,
    /// Budgets (wie `/usage/budgets`).
    pub items: Vec<BudgetForecast>,
}

/// Zeile des Chargeback-Exports (JSON; CSV mit denselben Spalten in `snake_case`, Labels und
/// Metadaten als Spalten `label.<name>` bzw. `meta.<name>`).
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CostExportRow {
    /// Beginn des Abschnitts.
    pub period_start: String,
    /// Ende des Abschnitts (exklusive).
    pub period_end: String,
    /// Schlüssel (ID bzw. Name).
    pub key: String,
    /// Anzeigename.
    pub name: String,
    /// Organisation (Name; nur Organisation/Team/Key).
    pub organization: String,
    /// Team (Name; nur Team/Key).
    pub team: String,
    /// Kostenstelle (`spec.metadata[billing.cost_center_key]` von Key, Team, Organisation).
    pub cost_center: String,
    /// Labels des Objekts (`metadata.labels`).
    pub labels: BTreeMap<String, String>,
    /// Metadaten des Objekts (`spec.metadata`).
    pub metadata: BTreeMap<String, String>,
    /// Anfragen.
    pub requests: i64,
    /// Eingabe-Tokens.
    pub input_tokens: i64,
    /// Ausgabe-Tokens.
    pub output_tokens: i64,
    /// Aus dem Anbieter-Cache gelesene Tokens.
    pub cached_tokens: i64,
    /// Kosten (inkl. Plugins).
    pub cost: Amount,
    /// Davon Plugin-Modellaufrufe.
    pub plugin_cost: Amount,
    /// Währung.
    pub currency: String,
}

/// Chargeback-Export als JSON (gestreamt).
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CostExport {
    /// Dimension.
    pub dimension: Dimension,
    /// Abschnitte.
    pub granularity: Granularity,
    /// Beginn.
    pub from: String,
    /// Ende (exklusive).
    pub to: String,
    /// Zeitzone der Abschnitte.
    pub timezone: String,
    /// Währung.
    pub currency: String,
    /// Zeilen je Abschnitt und Gruppe (ohne Gruppen ohne Anfragen und Kosten).
    pub rows: Vec<CostExportRow>,
}

// ---------------------------------------------------------------------------------------------
// Hilfen
// ---------------------------------------------------------------------------------------------

/// Zeitraum (Standard: letzte 30 Tage), auf Minuten ausgerichtet.
fn cost_range(from: Option<&str>, to: Option<&str>) -> Result<(i64, i64), Problem> {
    let to = match to {
        Some(t) => parse_time("to", t)?,
        None => now_ms(),
    };
    let from = match from {
        Some(f) => parse_time("from", f)?,
        None => to - DEFAULT_RANGE_MS,
    };
    let (from, to) = (
        crate::analytics::floor(from, MINUTE_MS),
        crate::analytics::ceil(to, MINUTE_MS),
    );
    if from >= to {
        return Err(Problem::bad_request("from muss vor to liegen"));
    }
    if to - from > MAX_RANGE_MS {
        return Err(Problem::bad_request(
            "Zeitraum zu lang (höchstens 400 Tage)",
        ));
    }
    Ok((from, to))
}

fn no_error_class(d: Dimension) -> Result<(), Problem> {
    if d == Dimension::ErrorClass {
        return Err(Problem::bad_request(
            "dimension=error_class ist für Kosten nicht vorgesehen",
        ));
    }
    Ok(())
}

fn nanos(v: i128) -> Amount {
    Amount::from_nanos(i64::try_from(v).unwrap_or(if v < 0 { i64::MIN } else { i64::MAX }))
}

#[allow(clippy::cast_precision_loss)]
fn share(part: i64, total: i64) -> Option<f64> {
    (total > 0).then(|| part as f64 / total as f64)
}

#[allow(clippy::cast_precision_loss)]
fn delta(cur: i64, prev: i64) -> CostDelta {
    CostDelta {
        amount: Amount::from_nanos(cur),
        previous: Amount::from_nanos(prev),
        delta: Amount::from_nanos(cur.saturating_sub(prev)),
        delta_percent: (prev != 0).then(|| (cur - prev) as f64 / (prev as f64).abs() * 100.0),
    }
}

/// `amount / count` (exakt in Nano, abgerundet); `None` bei `count` 0.
fn per(amount: i64, count: i64, scale: i128) -> Option<Amount> {
    (count > 0).then(|| nanos(i128::from(amount) * scale / i128::from(count)))
}

/// Aufschlüsselung; der Rest (`other`) ist nie negativ (Rundung der Teile).
fn parts(c: &RollupCounts) -> Vec<CostPart> {
    let known = [
        (CostPartKind::Input, c.cost_input),
        (CostPartKind::CacheRead, c.cost_cache_read),
        (CostPartKind::CacheWrite, c.cost_cache_write),
        (CostPartKind::Output, c.cost_output),
        (CostPartKind::Plugins, c.plugin_cost),
    ];
    let sum: i64 = known.iter().map(|(_, v)| *v).fold(0, i64::saturating_add);
    let other = c.cost.saturating_sub(sum).max(0);
    known
        .into_iter()
        .chain([(CostPartKind::Other, other)])
        .map(|(part, v)| CostPart {
            part,
            amount: Amount::from_nanos(v),
            share: share(v, c.cost),
        })
        .collect()
}

/// Schätzung der Ersparnis durch den Antwort-Cache: je Modellgruppe Treffer × mittlere Kosten
/// einer bezahlten Anfrage (ohne Treffer und Fehler, ohne Plugin-Kosten – Plugins laufen auch
/// bei Treffern). Liefert (Schätzung in Nano, Treffer ohne Basis).
fn gateway_cache_estimate(groups: &BTreeMap<String, RollupCounts>) -> (Option<i128>, i64) {
    let mut est: Option<i128> = None;
    let mut unestimated = 0_i64;
    for c in groups.values() {
        if c.cache_hits <= 0 {
            continue;
        }
        let paid = c.requests - c.cache_hits - c.errors;
        let cost = c.cost.saturating_sub(c.plugin_cost).max(0);
        if paid > 0 && cost > 0 {
            *est.get_or_insert(0) += i128::from(cost) * i128::from(c.cache_hits) / i128::from(paid);
        } else {
            unestimated += c.cache_hits;
        }
    }
    (est, unestimated)
}

/// Spalten für Kostentreiber und Export.
const DRIVER_COLUMNS: [&str; 7] = [
    "requests",
    "input_tokens",
    "output_tokens",
    "cached_tokens",
    "cost",
    "plugin_cost",
    "errors",
];

// ---------------------------------------------------------------------------------------------
// Handler: Kennzahlen und Kostentreiber
// ---------------------------------------------------------------------------------------------

/// Kosten-Kennzahlen im Zeitraum mit Vorperiode.
#[utoipa::path(
    get, path = "/admin/api/v1/costs/summary", tag = "costs", params(CostSummaryQuery),
    responses(
        (status = 200, description = "Kennzahlen", body = CostSummary),
        (status = 400, description = "Ungültige Parameter", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Filter außerhalb des Geltungsbereichs", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Unbekannter Name in einem Filter", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub(crate) async fn summary(
    State(st): State<CostsState>,
    Extension(p): Extension<Principal>,
    q: Result<Query<CostSummaryQuery>, QueryRejection>,
) -> Result<Json<CostSummary>, Problem> {
    let Query(q) = q?;
    let (from, to) = cost_range(q.from.as_deref(), q.to.as_deref())?;
    let prev_from = from - (to - from);
    let filters = Filters {
        model_group: q.model_group,
        deployment: q.deployment,
        provider: q.provider,
        team: q.team,
        key: q.key,
        organization: q.organization,
        error_class: None,
    };
    let ctx = context_of(&st, &p, &filters).await?;
    let filter = &ctx.resolved.keys;
    let (cur, prev, groups) = tokio::try_join!(
        totals(&st.store, from, to, filter),
        totals(&st.store, prev_from, from, filter),
        group_totals(
            &st.store,
            from,
            to,
            Dimension::ModelGroup,
            filter,
            &ctx.dir,
            &["requests", "cache_hits", "errors", "cost", "plugin_cost"],
        ),
    )?;
    let (est, unestimated) = gateway_cache_estimate(&groups.totals);
    let input_cost = cur
        .cost_input
        .saturating_add(cur.cost_cache_read)
        .saturating_add(cur.cost_cache_write);
    Ok(Json(CostSummary {
        from: rfc3339(from),
        to: rfc3339(to),
        previous_from: rfc3339(prev_from),
        previous_to: rfc3339(from),
        currency: st.currency.clone(),
        rollups: st.rollups,
        total: delta(cur.cost, prev.cost),
        requests: cur.requests,
        previous_requests: prev.requests,
        cost_per_request: (cur.requests > 0).then(|| {
            let c = per(cur.cost, cur.requests, 1).unwrap_or_default();
            let pv = per(prev.cost, prev.requests, 1).unwrap_or_default();
            delta(c.nanos(), pv.nanos())
        }),
        input_tokens: cur.input_tokens,
        output_tokens: cur.output_tokens,
        cached_tokens: cur.cached_tokens,
        cache_write_tokens: cur.cache_write_tokens,
        cost_per_1k_input_tokens: per(input_cost, cur.input_tokens, 1000),
        cost_per_1k_output_tokens: per(cur.cost_output, cur.output_tokens, 1000),
        breakdown: parts(&cur),
        savings: CostSavings {
            provider_cache: Amount::from_nanos(cur.cache_read_saved),
            provider_cache_tokens: cur.cached_tokens,
            gateway_cache_estimate: est.map(nanos),
            gateway_cache_hits: cur.cache_hits,
            gateway_cache_hits_unestimated: unestimated,
        },
        plugins: PluginCosts {
            amount: Amount::from_nanos(cur.plugin_cost),
            input_tokens: cur.plugin_input_tokens,
            output_tokens: cur.plugin_output_tokens,
            share: share(cur.plugin_cost, cur.cost),
        },
    }))
}

async fn context_of(
    st: &CostsState,
    p: &Principal,
    f: &Filters,
) -> Result<crate::analytics::Ctx, Problem> {
    context(&st.store, p, f).await
}

/// Kostentreiber einer Dimension mit Vorperiode.
#[utoipa::path(
    get, path = "/admin/api/v1/costs/breakdown", tag = "costs", params(CostBreakdownQuery),
    responses(
        (status = 200, description = "Kostentreiber", body = CostDrivers),
        (status = 400, description = "Ungültige Parameter", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Filter außerhalb des Geltungsbereichs", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Unbekannter Name in einem Filter", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub(crate) async fn breakdown(
    State(st): State<CostsState>,
    Extension(p): Extension<Principal>,
    q: Result<Query<CostBreakdownQuery>, QueryRejection>,
) -> Result<Json<CostDrivers>, Problem> {
    let Query(q) = q?;
    no_error_class(q.dimension)?;
    let limit = clamp_limit(q.limit, 10, 100)?;
    let (from, to) = cost_range(q.from.as_deref(), q.to.as_deref())?;
    let prev_from = from - (to - from);
    let filters = Filters {
        model_group: q.model_group,
        deployment: q.deployment,
        provider: q.provider,
        team: q.team,
        key: q.key,
        organization: q.organization,
        error_class: None,
    };
    let ctx = context_of(&st, &p, &filters).await?;
    let filter = &ctx.resolved.keys;
    let (cur, prev) = tokio::try_join!(
        group_totals(
            &st.store,
            from,
            to,
            q.dimension,
            filter,
            &ctx.dir,
            &DRIVER_COLUMNS
        ),
        group_totals(
            &st.store,
            prev_from,
            from,
            q.dimension,
            filter,
            &ctx.dir,
            &["cost"]
        ),
    )?;
    let total_cur: i64 = cur
        .totals
        .values()
        .map(|c| c.cost)
        .fold(0, i64::saturating_add);
    let total_prev: i64 = prev
        .totals
        .values()
        .map(|c| c.cost)
        .fold(0, i64::saturating_add);
    let empty = RollupCounts::default();
    let keys: BTreeSet<&String> = cur.totals.keys().chain(prev.totals.keys()).collect();
    let mut items: Vec<CostDriver> = keys
        .into_iter()
        .map(|k| {
            let c = cur.totals.get(k).unwrap_or(&empty);
            let pv = prev.totals.get(k).map_or(0, |c| c.cost);
            CostDriver {
                key: k.clone(),
                name: ctx.dir.name(q.dimension, k),
                cost: delta(c.cost, pv),
                share: share(c.cost, total_cur),
                requests: c.requests,
                input_tokens: c.input_tokens,
                output_tokens: c.output_tokens,
                cost_per_request: per(c.cost, c.requests, 1),
                plugin_cost: Amount::from_nanos(c.plugin_cost),
            }
        })
        .collect();
    items.sort_by(|a, b| {
        b.cost
            .amount
            .cmp(&a.cost.amount)
            .then_with(|| b.cost.previous.cmp(&a.cost.previous))
            .then_with(|| a.key.cmp(&b.key))
    });
    items.truncate(limit as usize);
    Ok(Json(CostDrivers {
        dimension: q.dimension,
        from: rfc3339(from),
        to: rfc3339(to),
        previous_from: rfc3339(prev_from),
        currency: st.currency.clone(),
        rollups: st.rollups,
        total: delta(total_cur, total_prev),
        items,
    }))
}

// ---------------------------------------------------------------------------------------------
// Handler: Budgets mit Prognose
// ---------------------------------------------------------------------------------------------

fn rollup_dim(level: BudgetLevelName) -> RollupDim {
    match level {
        BudgetLevelName::Organization => RollupDim::Organization,
        BudgetLevelName::Team => RollupDim::Team,
        BudgetLevelName::ApiKey => RollupDim::ApiKey,
        BudgetLevelName::Deployment => RollupDim::Deployment,
    }
}

fn level_filter(level: BudgetLevelName, ids: Vec<String>) -> KeyFilter {
    let mut f = KeyFilter::default();
    match level {
        BudgetLevelName::Organization => f.organizations = Some(ids),
        BudgetLevelName::Team => f.teams = Some(ids),
        BudgetLevelName::ApiKey => f.api_keys = Some(ids),
        BudgetLevelName::Deployment => f.deployments = Some(ids),
    }
    f
}

/// Prognose aus Verbrauch, Kostenrate (Nano je ms) und Zeitraum.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn project(
    b: &BudgetStatus,
    end_ms: Option<i64>,
    now: i64,
    rate_per_ms: f64,
    method: ForecastMethod,
    lookback_days: Option<u32>,
) -> (BudgetHealth, BudgetProjection) {
    let used = b.used.nanos();
    let amount = b.amount.nanos();
    let projected = end_ms.map(|e| {
        let add = rate_per_ms * (e - now).max(0) as f64;
        used.saturating_add(add.round().min(i64::MAX as f64) as i64)
    });
    let exceeds_at = if used < amount && rate_per_ms > 0.0 {
        let t = now + ((amount - used) as f64 / rate_per_ms).ceil().min(1e15) as i64;
        end_ms.is_none_or(|e| t < e).then_some(t)
    } else {
        None
    };
    let will_exceed = used >= amount || exceeds_at.is_some();
    let projected_percent = projected.map(|v| {
        if amount > 0 {
            u64::try_from(i128::from(v.max(0)) * 100 / i128::from(amount)).unwrap_or(u64::MAX)
        } else {
            0
        }
    });
    let first_warn = b.warn_at_percent.iter().copied().min().map(u64::from);
    let health = if used >= amount {
        BudgetHealth::Exhausted
    } else if will_exceed {
        BudgetHealth::AtRisk
    } else if first_warn.is_some_and(|w| b.percent >= w) {
        BudgetHealth::Warning
    } else {
        BudgetHealth::Ok
    };
    (
        health,
        BudgetProjection {
            method,
            lookback_days,
            daily_rate: Amount::from_nanos((rate_per_ms * DAY_MS as f64).round() as i64),
            projected: projected.map(Amount::from_nanos),
            projected_percent,
            exceeds_at: exceeds_at.map(rfc3339),
            will_exceed,
        },
    )
}

/// Abschnittsgrenzen des Burn-downs: Stunden bei Tagesbudgets, sonst Kalendertage.
fn burn_edges(cal: &Calendar, period: BudgetPeriod, start: i64, end: i64) -> (Vec<i64>, i64) {
    if period == BudgetPeriod::Day {
        return bucket_edges(cal, start, end, Step::H1);
    }
    bucket_edges(cal, start, end, Step::D1)
}

/// Burn-down eines Budgets: kumulierte Kosten je Abschnitt bis jetzt, danach Prognose.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
async fn burn_down(
    st: &CostsState,
    b: &BudgetStatus,
    period_start: i64,
    period_end: i64,
    now: i64,
    rate_per_ms: f64,
) -> Result<Vec<BurnPoint>, Problem> {
    let (edges, query_step) = burn_edges(&st.calendar, b.period, period_start, period_end);
    let upto = crate::analytics::ceil(now.min(period_end), MINUTE_MS);
    let rows = st
        .store
        .query_rollups(&RollupQuery {
            from_ms: edges.first().copied().unwrap_or(period_start),
            to_ms: upto,
            step_ms: Some(query_step),
            grouping: Grouping::None,
            filter: level_filter(b.level, vec![b.target_id.to_string()]),
            columns: vec!["cost"],
        })
        .await?;
    let mut per_bucket: BTreeMap<i64, i64> = BTreeMap::new();
    for r in rows {
        let i = edges.partition_point(|e| *e <= r.bucket_ms);
        if let Some(&t) = edges.get(i.saturating_sub(1)) {
            let e = per_bucket.entry(t).or_default();
            *e = e.saturating_add(r.counts.cost);
        }
    }
    let used = b.used.nanos();
    let mut cum = 0_i64;
    let mut out = Vec::with_capacity(edges.len());
    for w in edges.windows(2) {
        let (start, end) = (w[0], w[1]);
        if start < now {
            cum = cum.saturating_add(per_bucket.get(&start).copied().unwrap_or(0));
            out.push(BurnPoint {
                t: rfc3339(end.min(now)),
                spent: Some(Amount::from_nanos(cum)),
                projected: None,
            });
        } else {
            let add = rate_per_ms * (end - now) as f64;
            out.push(BurnPoint {
                t: rfc3339(end),
                spent: None,
                projected: Some(Amount::from_nanos(
                    used.saturating_add(add.round().min(i64::MAX as f64) as i64),
                )),
            });
        }
    }
    Ok(out)
}

/// Budgets mit Verbrauch, Prognose bis Periodenende und (für ein Ziel) Burn-down.
#[utoipa::path(
    get, path = "/admin/api/v1/costs/budgets", tag = "costs", params(BudgetForecastQuery),
    responses(
        (status = 200, description = "Budgets mit Prognose (nur Ziele mit Leserecht)", body = BudgetForecastList),
        (status = 400, description = "Ungültige Parameter", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
#[allow(clippy::cast_precision_loss)]
pub(crate) async fn budgets(
    State(st): State<CostsState>,
    Extension(p): Extension<Principal>,
    q: Result<Query<BudgetForecastQuery>, QueryRejection>,
) -> Result<Json<BudgetForecastList>, Problem> {
    let Query(q) = q?;
    let method = q.method.unwrap_or(ForecastMethod::Average);
    let lookback = q.lookback_days.unwrap_or(DEFAULT_LOOKBACK_DAYS);
    if !(1..=90).contains(&lookback) {
        return Err(Problem::bad_request("lookbackDays muss 1–90 sein"));
    }
    if q.target.is_some() && q.level.is_none() {
        return Err(Problem::bad_request("target erfordert level"));
    }
    let (now, mut items) = collect_budgets(&st.store, &st.calendar, &p, q.level).await?;
    if let Some(t) = &q.target {
        items.retain(|(b, _)| b.target_name == *t || b.target_id.to_string() == *t);
    }

    // Rate je Budget (Nano je ms): Durchschnitt der letzten N Tage aus den Rollups – je Ebene
    // eine gruppierte Abfrage – bzw. linear aus dem Verbrauch seit Periodenbeginn.
    let window_from = crate::analytics::floor(now - i64::from(lookback) * DAY_MS, MINUTE_MS);
    let window_to = crate::analytics::ceil(now, MINUTE_MS);
    let mut recent: BTreeMap<(BudgetLevelName, String), i64> = BTreeMap::new();
    if method == ForecastMethod::Average {
        for level in [
            BudgetLevelName::Organization,
            BudgetLevelName::Team,
            BudgetLevelName::ApiKey,
            BudgetLevelName::Deployment,
        ] {
            let ids: Vec<String> = items
                .iter()
                .filter(|(b, _)| b.level == level)
                .map(|(b, _)| b.target_id.to_string())
                .collect();
            if ids.is_empty() {
                continue;
            }
            let rows = st
                .store
                .query_rollups(&RollupQuery {
                    from_ms: window_from,
                    to_ms: window_to,
                    step_ms: None,
                    grouping: Grouping::Raw(rollup_dim(level)),
                    filter: level_filter(level, ids),
                    columns: vec!["cost"],
                })
                .await?;
            for r in rows {
                let e = recent.entry((level, r.group)).or_default();
                *e = e.saturating_add(r.counts.cost);
            }
        }
    }

    let mut out = Vec::with_capacity(items.len());
    for (b, range) in items {
        let rate = match method {
            ForecastMethod::Average => {
                let c = recent
                    .get(&(b.level, b.target_id.to_string()))
                    .copied()
                    .unwrap_or(0);
                c as f64 / (window_to - window_from).max(1) as f64
            }
            ForecastMethod::Linear => {
                b.used.nanos().max(0) as f64 / (now - range.start_ms).max(MINUTE_MS) as f64
            }
        };
        let (health, forecast) = project(
            &b,
            range.end_ms,
            now,
            rate,
            method,
            (method == ForecastMethod::Average).then_some(lookback),
        );
        let burn = match (&q.target, range.end_ms) {
            (Some(_), Some(end)) => burn_down(&st, &b, range.start_ms, end, now, rate).await?,
            _ => Vec::new(),
        };
        out.push(BudgetForecast {
            budget: b,
            health,
            forecast,
            burn_down: burn,
        });
    }
    Ok(Json(BudgetForecastList {
        timezone: st.calendar.timezone().to_owned(),
        at: rfc3339(now),
        currency: st.currency.clone(),
        rollups: st.rollups,
        items: out,
    }))
}

// ---------------------------------------------------------------------------------------------
// Handler: Chargeback-Export
// ---------------------------------------------------------------------------------------------

/// Abschnitte `[start, end)` des Exports (an den Rändern auf den Zeitraum gekürzt).
fn windows(cal: &Calendar, g: Granularity, from: i64, to: i64) -> Vec<(i64, i64)> {
    let period = match g {
        Granularity::Total => return vec![(from, to)],
        Granularity::Day => BudgetPeriod::Day,
        Granularity::Week => BudgetPeriod::Week,
        Granularity::Month => BudgetPeriod::Month,
    };
    let mut out = Vec::new();
    let mut t = from;
    while t < to {
        let end = cal
            .period(period, t)
            .end_ms
            .unwrap_or(to)
            .min(to)
            .max(t + MINUTE_MS);
        out.push((t, end));
        t = end;
    }
    out
}

/// Text für CSV: Anführungszeichen bei Bedarf, Schutz vor Formel-Injektion in
/// Tabellenkalkulationen (führendes `=`, `+`, `-`, `@`, Tab, CR → mit `'` entschärft).
fn csv_text(v: &str) -> String {
    let v = if v.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{v}")
    } else {
        v.to_owned()
    };
    if v.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else {
        v
    }
}

/// Alles, was der gestreamte Export braucht (unabhängig von der Anfrage).
struct Exporter {
    store: Store,
    dir: Directory,
    filter: KeyFilter,
    dim: Dimension,
    format: ExportFormat,
    currency: String,
    cost_center_key: String,
    label_cols: Vec<String>,
    meta_cols: Vec<String>,
}

impl Exporter {
    /// Organisation, Team, Kostenstelle und Attribute einer Gruppe.
    fn context(&self, key: &str) -> (String, String, String, Option<&crate::analytics::Attrs>) {
        let d = &self.dir;
        let attrs = d.attrs.get(key);
        let cc = |id: Option<&str>| {
            id.and_then(|i| d.attrs.get(i))
                .and_then(|a| a.metadata.get(&self.cost_center_key))
                .cloned()
        };
        match self.dim {
            Dimension::Org => (
                d.orgs.get(key).cloned().unwrap_or_default(),
                String::new(),
                cc(Some(key)).unwrap_or_default(),
                attrs,
            ),
            Dimension::Team => {
                let t = d.teams.get(key);
                let org = t.map(|t| t.organization.clone()).unwrap_or_default();
                let org_id = d.org_id_by_name(&org);
                (
                    org.clone(),
                    t.map(|t| t.name.clone()).unwrap_or_default(),
                    cc(Some(key)).or_else(|| cc(org_id)).unwrap_or_default(),
                    attrs,
                )
            }
            Dimension::Key => {
                let k = d.keys.get(key);
                let org = k.map(|k| k.organization.clone()).unwrap_or_default();
                let team = k.and_then(|k| k.team.clone()).unwrap_or_default();
                let team_id = d.team_id_by_name(&team);
                let org_id = d.org_id_by_name(&org);
                (
                    org.clone(),
                    team.clone(),
                    cc(Some(key))
                        .or_else(|| cc(team_id))
                        .or_else(|| cc(org_id))
                        .unwrap_or_default(),
                    attrs,
                )
            }
            _ => (String::new(), String::new(), String::new(), None),
        }
    }

    fn header(&self, from: i64, to: i64, g: Granularity, tz: &str) -> String {
        match self.format {
            ExportFormat::Csv => {
                let mut h = String::from(
                    "period_start,period_end,dimension,key,name,organization,team,cost_center",
                );
                for l in &self.label_cols {
                    let _ = write!(h, ",{}", csv_text(&format!("label.{l}")));
                }
                for m in &self.meta_cols {
                    let _ = write!(h, ",{}", csv_text(&format!("meta.{m}")));
                }
                h.push_str(
                    ",requests,input_tokens,output_tokens,cached_tokens,cost,plugin_cost,currency\r\n",
                );
                h
            }
            ExportFormat::Json => {
                let head = serde_json::json!({
                    "dimension": self.dim,
                    "granularity": g,
                    "from": rfc3339(from),
                    "to": rfc3339(to),
                    "timezone": tz,
                    "currency": self.currency,
                });
                // Objekt ohne schließende Klammer, dann `"rows":[`.
                let mut s = head.to_string();
                s.pop();
                s.push_str(",\"rows\":[");
                s
            }
        }
    }

    /// Zeilen eines Abschnitts; `first` = noch keine JSON-Zeile geschrieben.
    async fn window(&self, start: i64, end: i64, first: &mut bool) -> Result<String, Problem> {
        let rows = self
            .store
            .query_rollups(&RollupQuery {
                from_ms: start,
                to_ms: end,
                step_ms: None,
                grouping: Grouping::Raw(self.dim.column()),
                filter: self.filter.clone(),
                columns: DRIVER_COLUMNS.to_vec(),
            })
            .await?;
        let mut groups: BTreeMap<String, RollupCounts> = BTreeMap::new();
        for r in rows {
            let k = if self.dim == Dimension::Provider {
                self.dir.provider_of(&r.group)
            } else {
                r.group
            };
            groups.entry(k).or_default().merge(&r.counts);
        }
        let mut list: Vec<(String, String, RollupCounts)> = groups
            .into_iter()
            .filter(|(_, c)| c.requests != 0 || c.cost != 0)
            .map(|(k, c)| {
                let name = self.dir.name(self.dim, &k).unwrap_or_default();
                (k, name, c)
            })
            .collect();
        list.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        let (ps, pe) = (rfc3339(start), rfc3339(end));
        let dim = serde_json::to_value(self.dim)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        let mut out = String::new();
        for (key, name, c) in list {
            let (org, team, cc, attrs) = self.context(&key);
            let empty = BTreeMap::new();
            let labels = attrs.map_or(&empty, |a| &a.labels);
            let meta = attrs.map_or(&empty, |a| &a.metadata);
            match self.format {
                ExportFormat::Csv => {
                    let _ = write!(
                        out,
                        "{ps},{pe},{dim},{},{},{},{},{}",
                        csv_text(&key),
                        csv_text(&name),
                        csv_text(&org),
                        csv_text(&team),
                        csv_text(&cc)
                    );
                    for l in &self.label_cols {
                        let _ = write!(
                            out,
                            ",{}",
                            csv_text(labels.get(l).map_or("", String::as_str))
                        );
                    }
                    for m in &self.meta_cols {
                        let _ =
                            write!(out, ",{}", csv_text(meta.get(m).map_or("", String::as_str)));
                    }
                    let _ = write!(
                        out,
                        ",{},{},{},{},{},{},{}\r\n",
                        c.requests,
                        c.input_tokens,
                        c.output_tokens,
                        c.cached_tokens,
                        Amount::from_nanos(c.cost),
                        Amount::from_nanos(c.plugin_cost),
                        self.currency
                    );
                }
                ExportFormat::Json => {
                    let row = CostExportRow {
                        period_start: ps.clone(),
                        period_end: pe.clone(),
                        key,
                        name,
                        organization: org,
                        team,
                        cost_center: cc,
                        labels: labels.clone(),
                        metadata: meta.clone(),
                        requests: c.requests,
                        input_tokens: c.input_tokens,
                        output_tokens: c.output_tokens,
                        cached_tokens: c.cached_tokens,
                        cost: Amount::from_nanos(c.cost),
                        plugin_cost: Amount::from_nanos(c.plugin_cost),
                        currency: self.currency.clone(),
                    };
                    if !*first {
                        out.push(',');
                    }
                    *first = false;
                    out.push_str(&serde_json::to_string(&row).map_err(|e| {
                        tracing::error!(error = %e, "Kosten-Export: Zeile nicht serialisierbar");
                        Problem::internal()
                    })?);
                }
            }
        }
        Ok(out)
    }
}

/// Label- und Metadaten-Schlüssel der sichtbaren Objekte der Dimension (Spalten des CSV).
fn attr_columns(dir: &Directory, dim: Dimension, scope: &Scope) -> (Vec<String>, Vec<String>) {
    let ids: Vec<&String> = match dim {
        Dimension::Org => dir.orgs.keys().collect(),
        Dimension::Team => dir.teams.keys().collect(),
        Dimension::Key => dir.keys.keys().collect(),
        _ => Vec::new(),
    };
    let visible = |id: &str| match scope {
        Scope::All => true,
        Scope::Restricted {
            teams,
            teamless_orgs,
        } => match dim {
            Dimension::Org => teamless_orgs.contains(id),
            Dimension::Team => teams.contains(id),
            Dimension::Key => dir.keys.get(id).is_some_and(|k| match &k.team {
                Some(t) => dir.team_id_by_name(t).is_some_and(|t| teams.contains(t)),
                None => dir
                    .org_id_by_name(&k.organization)
                    .is_some_and(|o| teamless_orgs.contains(o)),
            }),
            _ => false,
        },
    };
    let mut labels = BTreeSet::new();
    let mut meta = BTreeSet::new();
    for id in ids {
        if visible(id)
            && let Some(a) = dir.attrs.get(id)
        {
            labels.extend(a.labels.keys().cloned());
            meta.extend(a.metadata.keys().cloned());
        }
    }
    (labels.into_iter().collect(), meta.into_iter().collect())
}

/// Chargeback-Export (CSV oder JSON, gestreamt je Abschnitt).
#[utoipa::path(
    get, path = "/admin/api/v1/costs/export", tag = "costs", params(CostExportQuery),
    responses(
        (status = 200, description = "Kosten je Abschnitt und Gruppe, gestreamt. CSV: Kopfzeile \
            `period_start,period_end,dimension,key,name,organization,team,cost_center`, dann \
            `label.<name>` und `meta.<name>` der sichtbaren Objekte, dann \
            `requests,input_tokens,output_tokens,cached_tokens,cost,plugin_cost,currency`.",
            content(
                (String = "text/csv"),
                (CostExport = "application/json"),
            )),
        (status = 400, description = "Ungültige Parameter", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Filter außerhalb des Geltungsbereichs", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Unbekannter Name in einem Filter", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub(crate) async fn export(
    State(st): State<CostsState>,
    Extension(p): Extension<Principal>,
    q: Result<Query<CostExportQuery>, QueryRejection>,
) -> Result<Response, Problem> {
    let Query(q) = q?;
    let dim = q.dimension.unwrap_or(Dimension::Team);
    no_error_class(dim)?;
    let format = q.format.unwrap_or(ExportFormat::Csv);
    let granularity = q.granularity.unwrap_or(Granularity::Month);
    let (from, to) = cost_range(q.from.as_deref(), q.to.as_deref())?;
    let filters = Filters {
        model_group: q.model_group,
        deployment: q.deployment,
        provider: q.provider,
        team: q.team,
        key: q.key,
        organization: q.organization,
        error_class: None,
    };
    let ctx = context_of(&st, &p, &filters).await?;
    let scope = scope_of(&p, &ctx.dir);
    let (label_cols, meta_cols) = attr_columns(&ctx.dir, dim, &scope);
    let exporter = Arc::new(Exporter {
        store: st.store.clone(),
        dir: ctx.dir,
        filter: ctx.resolved.keys,
        dim,
        format,
        currency: st.currency.clone(),
        cost_center_key: st.cost_center_key.clone(),
        label_cols,
        meta_cols,
    });
    let head = exporter.header(from, to, granularity, st.calendar.timezone());
    let wins = windows(&st.calendar, granularity, from, to);

    /// Fortschritt des Streams: nächster Abschnitt, JSON-Zustand, fertig.
    struct Progress {
        exporter: Arc<Exporter>,
        wins: std::vec::IntoIter<(i64, i64)>,
        first: bool,
        done: bool,
    }
    let state = Progress {
        exporter,
        wins: wins.into_iter(),
        first: true,
        done: false,
    };
    let body =
        futures_util::stream::once(async move { Ok::<_, std::io::Error>(Bytes::from(head)) })
            .chain(futures_util::stream::unfold(state, |mut s| async move {
                if s.done {
                    return None;
                }
                let Some((a, b)) = s.wins.next() else {
                    s.done = true;
                    return match s.exporter.format {
                        ExportFormat::Json => Some((Ok(Bytes::from_static(b"]}\n")), s)),
                        ExportFormat::Csv => None,
                    };
                };
                let ex = Arc::clone(&s.exporter);
                match ex.window(a, b, &mut s.first).await {
                    Ok(chunk) => Some((Ok(Bytes::from(chunk)), s)),
                    Err(err) => {
                        // Status schon gesendet: Abbruch der Übertragung zeigt den Fehler an.
                        tracing::error!(error = ?err, "Kosten-Export: Datenbankfehler");
                        s.done = true;
                        Some((Err(std::io::Error::other("Export abgebrochen")), s))
                    }
                }
            }));
    let mut res = Body::from_stream(body).into_response();
    let (ctype, ext) = match format {
        ExportFormat::Csv => ("text/csv; charset=utf-8", "csv"),
        ExportFormat::Json => ("application/json", "json"),
    };
    let dim_name = serde_json::to_value(dim)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    let day = |ms: i64| rfc3339(ms).get(..10).unwrap_or_default().to_owned();
    let file = format!(
        "attachment; filename=\"aigw-kosten-{dim_name}-{}-{}.{ext}\"",
        day(from),
        day(to - 1)
    );
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Ok(v) = HeaderValue::from_str(&file) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(res)
}

/// Router der Kosten-Routen (mit Anmeldung).
pub fn router(state: CostsState) -> Router {
    Router::new()
        .route(SUMMARY_PATH, get(summary))
        .route(BREAKDOWN_PATH, get(breakdown))
        .route(BUDGETS_PATH, get(budgets))
        .route(EXPORT_PATH, get(export))
        .route_layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state.auth),
            require_principal,
        ))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_escaping() {
        assert_eq!(csv_text("team-a"), "team-a");
        assert_eq!(csv_text("a,b"), "\"a,b\"");
        assert_eq!(csv_text("sag \"hi\""), "\"sag \"\"hi\"\"\"");
        assert_eq!(csv_text("=SUMME(A1)"), "'=SUMME(A1)");
        assert_eq!(csv_text("@x,y"), "\"'@x,y\"");
    }

    #[test]
    fn parts_never_negative() {
        let c = RollupCounts {
            cost: 100,
            cost_input: 40,
            cost_output: 50,
            plugin_cost: 15,
            ..RollupCounts::default()
        };
        let p = parts(&c);
        assert_eq!(p.len(), 6);
        assert_eq!(p[5].part, CostPartKind::Other);
        assert_eq!(p[5].amount, Amount::ZERO);
        let c = RollupCounts {
            cost: 100,
            cost_input: 40,
            ..RollupCounts::default()
        };
        assert_eq!(parts(&c)[5].amount.nanos(), 60);
        assert_eq!(parts(&c)[0].share, Some(0.4));
    }

    #[test]
    fn cache_estimate_per_group() {
        let mut g = BTreeMap::new();
        // 10 Anfragen, 2 Treffer, 1 Fehler → 7 bezahlte für 70 (+ 7 Plugin) → 10 je Anfrage.
        g.insert(
            "a".to_owned(),
            RollupCounts {
                requests: 10,
                cache_hits: 2,
                errors: 1,
                cost: 77,
                plugin_cost: 7,
                ..RollupCounts::default()
            },
        );
        // Nur Treffer: keine Basis.
        g.insert(
            "b".to_owned(),
            RollupCounts {
                requests: 3,
                cache_hits: 3,
                ..RollupCounts::default()
            },
        );
        assert_eq!(gateway_cache_estimate(&g), (Some(20), 3));
        assert_eq!(gateway_cache_estimate(&BTreeMap::new()), (None, 0));
    }

    fn status(used: i64, amount: i64) -> BudgetStatus {
        BudgetStatus {
            level: BudgetLevelName::Team,
            target_id: aigw_core::resource::new_id(),
            target_name: "t".into(),
            period: BudgetPeriod::Month,
            period_start: String::new(),
            period_end: None,
            amount: Amount::from_nanos(amount),
            used: Amount::from_nanos(used),
            remaining: Amount::from_nanos((amount - used).max(0)),
            percent: u64::try_from(used * 100 / amount).unwrap_or(0),
            warn_at_percent: vec![80],
            hard_limit: true,
            exhausted: used >= amount,
            model_group: None,
            switch_at_percent: None,
        }
    }

    #[test]
    fn projection_and_health() {
        let day = DAY_MS as f64;
        // 50 von 100 verbraucht, 10 je Tag, noch 10 Tage → 150 erwartet, Überschreitung in 5 Tagen.
        let (h, f) = project(
            &status(50, 100),
            Some(10 * DAY_MS),
            0,
            10.0 / day,
            ForecastMethod::Average,
            Some(7),
        );
        assert_eq!(h, BudgetHealth::AtRisk);
        assert_eq!(f.projected, Some(Amount::from_nanos(150)));
        assert_eq!(f.projected_percent, Some(150));
        assert_eq!(f.exceeds_at.as_deref(), Some(rfc3339(5 * DAY_MS).as_str()));
        assert_eq!(f.daily_rate, Amount::from_nanos(10));
        assert!(f.will_exceed);
        // 85 verbraucht, kein weiterer Verbrauch → Warnung (Schwelle 80), keine Überschreitung.
        let (h, f) = project(
            &status(85, 100),
            Some(DAY_MS),
            0,
            0.0,
            ForecastMethod::Linear,
            None,
        );
        assert_eq!(h, BudgetHealth::Warning);
        assert!(!f.will_exceed && f.exceeds_at.is_none());
        // Unter der Schwelle, Hochrechnung bleibt darunter → ok; ohne Periodenende keine Projektion.
        let (h, f) = project(&status(10, 100), None, 0, 0.0, ForecastMethod::Linear, None);
        assert_eq!(h, BudgetHealth::Ok);
        assert_eq!(f.projected, None);
        // Erreicht → erschöpft.
        assert_eq!(
            project(
                &status(100, 100),
                Some(DAY_MS),
                0,
                1.0,
                ForecastMethod::Linear,
                None
            )
            .0,
            BudgetHealth::Exhausted
        );
    }

    #[test]
    fn export_windows_follow_calendar() {
        let cal = Calendar::utc();
        let d = DAY_MS;
        assert_eq!(windows(&cal, Granularity::Total, 5, 10), vec![(5, 10)]);
        // Tage: angebrochene Ränder werden gekürzt.
        assert_eq!(
            windows(&cal, Granularity::Day, d / 2, 2 * d + 60_000),
            vec![(d / 2, d), (d, 2 * d), (2 * d, 2 * d + 60_000)]
        );
        // Monate: Januar/Februar 1970.
        let feb = 31 * d;
        assert_eq!(
            windows(&cal, Granularity::Month, 0, feb + d),
            vec![(0, feb), (feb, feb + d)]
        );
    }
}
