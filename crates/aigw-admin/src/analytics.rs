// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Dashboards und Log-Explorer (AIGW-46).
//!
//! - `GET /admin/api/v1/analytics/timeseries` – Zeitreihe einer Kennzahl, optional gruppiert
//!   (Top-N + „other“)
//! - `GET /admin/api/v1/analytics/summary` – KPIs im Zeitraum und Vergleich mit der Vorperiode
//! - `GET /admin/api/v1/analytics/top` – Rangliste einer Dimension nach einer Kennzahl
//! - `GET /admin/api/v1/request-logs` – Log-Explorer (Filter, Cursor)
//! - `GET /admin/api/v1/request-logs/{id}` – Einzelanfrage mit Upstream-Versuchen und – mit
//!   [`Permission::ReadPayload`] – gespeichertem Payload
//! - `GET /admin/api/v1/request-logs/{id}/payload` – nur der Payload
//!
//! Quelle der Kennzahlen sind die Minuten- und Stunden-Rollups (`aigw_store::analytics`), des
//! Log-Explorers das Rohlog. **Geltungsbereich:** Wer global lesen darf, sieht alles; sonst nur
//! Anfragen von Keys in Teams, auf die er Leserecht hat (`Target::InTeam`). Anfragen ohne Team
//! (Keys ohne Team, Fehler vor der Anmeldung) sieht nur eine globale Rolle.
//!
//! Filter nehmen Namen oder IDs (kommagetrennt); Ergebnisse tragen beides (ID als `key`,
//! aufgelöster Name als `name` – fehlt bei gelöschten Objekten).

// `#[utoipa::path(params(…))]` erzeugt für IntoParams-Typen qualifizierte Pfade.
#![allow(unused_qualifications)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::sync::Arc;

use aigw_core::resource::{Amount, DeploymentSpec, OrganizationSpec, TeamSpec};
use aigw_store::Store;
use aigw_store::analytics::{
    CacheFilter, Grouping, KeyFilter, LogQuery, RollupDim, RollupQuery, StatusFilter,
    histogram_quantile,
};
use aigw_store::observe::{AttemptRow, MINUTE_MS, RequestLogRow, RollupCounts};
use axum::extract::rejection::QueryRejection;
use axum::extract::{Extension, Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use utoipa::{IntoParams, ToSchema};

use crate::auth::{AdminAuthenticator, Permission, Principal, require_principal};
use crate::problem::Problem;
use crate::rbac::Target;

/// Pfad der Zeitreihen.
pub const TIMESERIES_PATH: &str = "/admin/api/v1/analytics/timeseries";
/// Pfad der KPIs.
pub const SUMMARY_PATH: &str = "/admin/api/v1/analytics/summary";
/// Pfad der Ranglisten.
pub const TOP_PATH: &str = "/admin/api/v1/analytics/top";
/// Pfad des Log-Explorers.
pub const LOGS_PATH: &str = "/admin/api/v1/request-logs";
/// Pfad einer Einzelanfrage.
pub const LOG_PATH: &str = "/admin/api/v1/request-logs/{id}";
/// Pfad des Payloads einer Anfrage.
pub const LOG_PAYLOAD_PATH: &str = "/admin/api/v1/request-logs/{id}/payload";

/// Höchstzahl Punkte je Zeitreihe bei explizitem `step`.
pub const MAX_POINTS: i64 = 1500;
/// Zielzahl Punkte bei automatisch gewähltem `step`.
pub const TARGET_POINTS: i64 = 300;
/// Längster abfragbarer Zeitraum (Standard-Aufbewahrung der Rollups).
pub const MAX_RANGE_MS: i64 = 400 * 86_400_000;
/// Standard-Zeitraum ohne `from` (24 h).
pub const DEFAULT_RANGE_MS: i64 = 86_400_000;
/// Schlüssel der Sammelgruppe jenseits der Top-N.
pub const OTHER_KEY: &str = "__other__";
/// Schlüssel der Gesamt-Reihe ohne `groupBy`.
pub const TOTAL_KEY: &str = "total";

/// Zustand der Analyse-Routen.
#[derive(Clone)]
pub struct AnalyticsState {
    /// Datenbank.
    pub store: Store,
    /// Anmeldung (dieselbe wie für die übrige Admin-API).
    pub auth: Arc<dyn AdminAuthenticator>,
}

impl fmt::Debug for AnalyticsState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnalyticsState")
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------------------------
// Parameter
// ---------------------------------------------------------------------------------------------

/// Kennzahl.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    /// Anfragen.
    Requests,
    /// Tokens (Eingabe + Ausgabe).
    Tokens,
    /// Eingabe-Tokens.
    InputTokens,
    /// Ausgabe-Tokens.
    OutputTokens,
    /// Kosten.
    Cost,
    /// Fehler (Status ≥ 400 oder Fehlerklasse).
    Errors,
    /// Fehlerquote (Fehler / Anfragen, 0–1).
    ErrorRate,
    /// Mittlere Latenz (ms).
    LatencyAvg,
    /// Latenz p50 (ms, aus Histogramm).
    LatencyP50,
    /// Latenz p95 (ms).
    LatencyP95,
    /// Latenz p99 (ms).
    LatencyP99,
    /// Zeit bis zum ersten Token p95 (ms, nur Streams).
    TtftP95,
    /// Cache-Trefferquote (Treffer / Anfragen, 0–1).
    CacheHitRate,
}

/// Einheit einer Kennzahl.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    /// Anzahl.
    Count,
    /// Tokens.
    Tokens,
    /// Abrechnungswährung (`amount` exakt als Dezimaltext).
    Currency,
    /// Millisekunden.
    Ms,
    /// Anteil 0–1.
    Ratio,
}

impl Metric {
    fn unit(self) -> Unit {
        match self {
            Self::Requests | Self::Errors => Unit::Count,
            Self::Tokens | Self::InputTokens | Self::OutputTokens => Unit::Tokens,
            Self::Cost => Unit::Currency,
            Self::ErrorRate | Self::CacheHitRate => Unit::Ratio,
            Self::LatencyAvg
            | Self::LatencyP50
            | Self::LatencyP95
            | Self::LatencyP99
            | Self::TtftP95 => Unit::Ms,
        }
    }

    /// Summierbar (Anteile an einer Summe sinnvoll; Top-N nach dem Wert selbst).
    fn additive(self) -> bool {
        matches!(
            self,
            Self::Requests
                | Self::Tokens
                | Self::InputTokens
                | Self::OutputTokens
                | Self::Cost
                | Self::Errors
        )
    }

    /// Wert aus Summen; `None`, wenn nicht definiert (keine Anfragen, kein Histogramm).
    #[allow(clippy::cast_precision_loss)]
    fn value(self, c: &RollupCounts) -> Option<f64> {
        let ratio = |a: i64, b: i64| (b > 0).then(|| a as f64 / b as f64);
        match self {
            Self::Requests => Some(c.requests as f64),
            Self::Tokens => Some(c.input_tokens.saturating_add(c.output_tokens) as f64),
            Self::InputTokens => Some(c.input_tokens as f64),
            Self::OutputTokens => Some(c.output_tokens as f64),
            Self::Cost => Some(c.cost as f64 / 1e9),
            Self::Errors => Some(c.errors as f64),
            Self::ErrorRate => ratio(c.errors, c.requests),
            Self::CacheHitRate => ratio(c.cache_hits, c.requests),
            Self::LatencyAvg => ratio(c.latency_ms_sum, c.requests),
            Self::LatencyP50 => histogram_quantile(&c.latency_hist, 0.50),
            Self::LatencyP95 => histogram_quantile(&c.latency_hist, 0.95),
            Self::LatencyP99 => histogram_quantile(&c.latency_hist, 0.99),
            Self::TtftP95 => histogram_quantile(&c.ttft_hist, 0.95),
        }
    }

    /// Benötigte Zählspalten (immer mit `requests` für Rang und Anteile).
    fn columns(self) -> Vec<&'static str> {
        const LAT: [&str; 12] = [
            "lat_le_25",
            "lat_le_50",
            "lat_le_100",
            "lat_le_250",
            "lat_le_500",
            "lat_le_1000",
            "lat_le_2500",
            "lat_le_5000",
            "lat_le_10000",
            "lat_le_30000",
            "lat_le_60000",
            "lat_inf",
        ];
        const TTFT: [&str; 12] = [
            "ttft_le_25",
            "ttft_le_50",
            "ttft_le_100",
            "ttft_le_250",
            "ttft_le_500",
            "ttft_le_1000",
            "ttft_le_2500",
            "ttft_le_5000",
            "ttft_le_10000",
            "ttft_le_30000",
            "ttft_le_60000",
            "ttft_inf",
        ];
        let mut v = vec!["requests"];
        match self {
            Self::Requests => {}
            Self::Tokens => v.extend(["input_tokens", "output_tokens"]),
            Self::InputTokens => v.push("input_tokens"),
            Self::OutputTokens => v.push("output_tokens"),
            Self::Cost => v.push("cost"),
            Self::Errors | Self::ErrorRate => v.push("errors"),
            Self::CacheHitRate => v.push("cache_hits"),
            Self::LatencyAvg => v.push("latency_ms_sum"),
            Self::LatencyP50 | Self::LatencyP95 | Self::LatencyP99 => v.extend(LAT),
            Self::TtftP95 => v.extend(TTFT),
        }
        v
    }

    fn amount(self, c: &RollupCounts) -> Option<Amount> {
        (self == Self::Cost).then(|| Amount::from_nanos(c.cost))
    }
}

/// Dimension für Gruppierung und Ranglisten.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Dimension {
    /// Modellgruppe.
    ModelGroup,
    /// Deployment.
    Deployment,
    /// Anbieter (über das Deployment, laut aktueller Konfiguration).
    Provider,
    /// Team.
    Team,
    /// API-Key.
    Key,
    /// Organisation.
    Org,
}

impl Dimension {
    fn column(self) -> RollupDim {
        match self {
            Self::ModelGroup => RollupDim::ModelGroup,
            Self::Deployment | Self::Provider => RollupDim::Deployment,
            Self::Team => RollupDim::Team,
            Self::Key => RollupDim::ApiKey,
            Self::Org => RollupDim::Organization,
        }
    }
}

/// Bucket-Breite einer Zeitreihe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum Step {
    /// 1 Minute.
    #[serde(rename = "1m")]
    M1,
    /// 5 Minuten.
    #[serde(rename = "5m")]
    M5,
    /// 1 Stunde.
    #[serde(rename = "1h")]
    H1,
    /// 1 Tag (UTC).
    #[serde(rename = "1d")]
    D1,
}

impl Step {
    const ALL: [Self; 4] = [Self::M1, Self::M5, Self::H1, Self::D1];

    fn ms(self) -> i64 {
        match self {
            Self::M1 => MINUTE_MS,
            Self::M5 => 5 * MINUTE_MS,
            Self::H1 => 60 * MINUTE_MS,
            Self::D1 => 1440 * MINUTE_MS,
        }
    }
}

/// Gemeinsame Filter (Namen oder IDs, kommagetrennt).
#[derive(Debug, Clone, Default)]
struct Filters {
    model_group: Option<String>,
    deployment: Option<String>,
    provider: Option<String>,
    team: Option<String>,
    key: Option<String>,
    organization: Option<String>,
}

/// Parameter der Zeitreihe.
#[derive(Debug, Deserialize, IntoParams)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct TimeseriesQuery {
    /// Kennzahl.
    metric: Metric,
    /// Beginn (RFC 3339; Standard: `to` − 24 h). Wird auf `step` abgerundet.
    from: Option<String>,
    /// Ende (RFC 3339, exklusive; Standard: jetzt). Wird auf `step` aufgerundet.
    to: Option<String>,
    /// Bucket-Breite; ohne Angabe die kleinste mit höchstens 300 Punkten.
    step: Option<Step>,
    /// Gruppierung (Top-N + `__other__`).
    group_by: Option<Dimension>,
    /// Anzahl Gruppen (1–20, Standard 5).
    limit: Option<u32>,
    /// Modellgruppen.
    model_group: Option<String>,
    /// Deployments (Name oder ID).
    deployment: Option<String>,
    /// Anbieter (`openai`, `anthropic` …).
    provider: Option<String>,
    /// Teams (Name oder ID).
    team: Option<String>,
    /// API-Keys (Name oder ID).
    key: Option<String>,
    /// Organisationen (Name oder ID).
    organization: Option<String>,
}

/// Parameter der KPIs.
#[derive(Debug, Deserialize, IntoParams)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct SummaryQuery {
    /// Beginn (RFC 3339; Standard: `to` − 24 h). Auf die Minute abgerundet.
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

/// Parameter der Rangliste.
#[derive(Debug, Deserialize, IntoParams)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct TopQuery {
    /// Dimension.
    dimension: Dimension,
    /// Kennzahl (Standard `requests`); absteigend sortiert.
    metric: Option<Metric>,
    /// Beginn (RFC 3339; Standard: `to` − 24 h).
    from: Option<String>,
    /// Ende (RFC 3339, exklusive; Standard: jetzt).
    to: Option<String>,
    /// Anzahl (1–100, Standard 10).
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

/// Parameter des Log-Explorers.
#[derive(Debug, Deserialize, IntoParams)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct LogListQuery {
    /// Ab (RFC 3339, inklusive).
    from: Option<String>,
    /// Bis (RFC 3339, exklusive).
    to: Option<String>,
    /// Status: Code (`429`), Klasse (`4xx`), `error` (≥ 400 oder Fehlerklasse) oder `success`.
    status: Option<String>,
    /// Fehlerklassen (`error_code`, kommagetrennt, z. B. `rate_limited`).
    error_code: Option<String>,
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
    /// Genau diese Request-ID.
    request_id: Option<String>,
    /// Genau diese Client-Request-ID (`x-request-id` / `x-client-request-id` des Clients).
    client_request_id: Option<String>,
    /// Nur Streams (`true`) bzw. keine Streams (`false`).
    stream: Option<bool>,
    /// Cache: `hit`, `miss`, `none`, `hit-exact`, `hit-semantic`.
    cache: Option<String>,
    /// Seitengröße (1–500, Standard 50).
    limit: Option<u32>,
    /// Fortsetzung (`nextCursor` der vorigen Seite).
    cursor: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Antworten
// ---------------------------------------------------------------------------------------------

/// Ein Punkt einer Zeitreihe.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Point {
    /// Bucket-Beginn (RFC 3339).
    pub t: String,
    /// Wert; `null` ohne Anfragen (Quoten, Latenzen). Kosten in Währungseinheiten (gerundet).
    pub v: Option<f64>,
    /// Kosten exakt (nur `metric=cost`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<Amount>,
}

/// Eine Reihe (Gruppe).
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Series {
    /// Gruppenschlüssel: ID (Key, Team, Organisation, Deployment), Name (Modellgruppe,
    /// Anbieter), `""` = ohne Wert, `__other__` = alle übrigen, `total` ohne `groupBy`.
    pub key: String,
    /// Anzeigename (fehlt bei unbekannten bzw. gelöschten Objekten).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Sammelgruppe jenseits der Top-N.
    pub other: bool,
    /// Wert über den ganzen Zeitraum.
    pub total: Option<f64>,
    /// Kosten exakt über den ganzen Zeitraum (nur `metric=cost`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_amount: Option<Amount>,
    /// Punkte, lückenlos je Bucket.
    pub points: Vec<Point>,
}

/// Zeitreihe.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Timeseries {
    /// Kennzahl.
    pub metric: Metric,
    /// Einheit.
    pub unit: Unit,
    /// Beginn (auf `step` ausgerichtet).
    pub from: String,
    /// Ende (exklusive, auf `step` ausgerichtet).
    pub to: String,
    /// Bucket-Breite.
    pub step: Step,
    /// Bucket-Breite in Sekunden.
    pub step_seconds: i64,
    /// Gruppierung.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_by: Option<Dimension>,
    /// Reihen (nach Rang, `__other__` zuletzt).
    pub series: Vec<Series>,
}

/// Eine KPI mit Vergleich.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Kpi {
    /// Wert im Zeitraum.
    pub value: Option<f64>,
    /// Wert in der Vorperiode.
    pub previous: Option<f64>,
    /// Änderung in Prozent ((Wert − Vorperiode) / Vorperiode · 100); `null` ohne Vorwert.
    pub delta_percent: Option<f64>,
    /// Kosten exakt (nur `cost`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<Amount>,
    /// Kosten der Vorperiode exakt (nur `cost`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_amount: Option<Amount>,
}

/// KPIs.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AnalyticsSummary {
    /// Beginn.
    pub from: String,
    /// Ende (exklusive).
    pub to: String,
    /// Beginn der Vorperiode (gleich lang, direkt davor).
    pub previous_from: String,
    /// Ende der Vorperiode (= `from`).
    pub previous_to: String,
    /// Anfragen.
    pub requests: Kpi,
    /// Tokens (Eingabe + Ausgabe).
    pub tokens: Kpi,
    /// Kosten.
    pub cost: Kpi,
    /// Latenz p95 (ms).
    pub latency_p95_ms: Kpi,
    /// Fehlerquote (0–1).
    pub error_rate: Kpi,
    /// Cache-Trefferquote (0–1).
    pub cache_hit_rate: Kpi,
}

/// Eintrag einer Rangliste.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TopItem {
    /// Schlüssel (wie [`Series::key`]).
    pub key: String,
    /// Anzeigename.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Wert.
    pub value: Option<f64>,
    /// Kosten exakt (nur `metric=cost`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<Amount>,
    /// Anfragen.
    pub requests: i64,
    /// Anteil am Gesamtwert (0–1, nur summierbare Kennzahlen).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub share: Option<f64>,
}

/// Rangliste.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TopList {
    /// Dimension.
    pub dimension: Dimension,
    /// Kennzahl.
    pub metric: Metric,
    /// Einheit.
    pub unit: Unit,
    /// Beginn.
    pub from: String,
    /// Ende (exklusive).
    pub to: String,
    /// Einträge, absteigend nach Wert (ohne Wert zuletzt).
    pub items: Vec<TopItem>,
}

/// Eine Anfrage im Log-Explorer.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestLogItem {
    /// Request-ID.
    pub id: String,
    /// Annahme (RFC 3339).
    pub at: String,
    /// Eingangsformat (`openai-chat`, `anthropic-messages` …).
    pub endpoint: String,
    /// API-Key-ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_id: Option<String>,
    /// API-Key-Name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Team-ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    /// Team-Name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    /// Organisations-ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organization_id: Option<String>,
    /// Organisations-Name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    /// Angefordertes Modell.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_requested: Option<String>,
    /// Modellgruppe.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_group: Option<String>,
    /// Deployment-ID (zuletzt verwendet).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<String>,
    /// Deployment-Name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployment: Option<String>,
    /// Anbieter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Anbieter-Modell.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_model: Option<String>,
    /// HTTP-Status an den Client.
    pub status: i64,
    /// Fehlerklasse.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Eingabe-Tokens.
    pub input_tokens: i64,
    /// Ausgabe-Tokens.
    pub output_tokens: i64,
    /// Aus dem Anbieter-Cache gelesene Tokens.
    pub cached_tokens: i64,
    /// In den Anbieter-Cache geschriebene Tokens.
    pub cache_write_tokens: i64,
    /// Reasoning-Tokens.
    pub reasoning_tokens: i64,
    /// Usage geschätzt.
    pub usage_estimated: bool,
    /// Kosten.
    pub cost: Amount,
    /// Gesamtdauer (ms).
    pub latency_ms: i64,
    /// Zeit bis zum ersten Token (ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<i64>,
    /// Cache-Ergebnis.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache: Option<String>,
    /// Streaming.
    pub stream: bool,
    /// Client-Abbruch.
    pub client_aborted: bool,
    /// Client-Adresse.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_ip: Option<String>,
    /// Request-ID des Clients (`x-request-id` / `x-client-request-id`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_request_id: Option<String>,
    /// Anzahl Upstream-Versuche (Retries, Fallbacks).
    pub attempt_count: usize,
    /// Payload gespeichert (lesbar nur mit `admin`/`auditor`).
    pub has_payload: bool,
}

/// Seite des Log-Explorers.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestLogList {
    /// Einträge, neueste zuerst.
    pub items: Vec<RequestLogItem>,
    /// Cursor der nächsten Seite (`?cursor=`), fehlt auf der letzten Seite.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Zugriff auf den Payload einer Anfrage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PayloadStatus {
    /// Gespeichert und lesbar (`payload` gesetzt).
    Available,
    /// Gespeichert, aber die Rolle darf ihn nicht lesen (nur `admin`, `auditor`).
    Forbidden,
    /// Nicht gespeichert (Payload-Logging aus oder abgelaufen).
    NotStored,
}

/// Gespeicherter Payload (bereits gefiltert bzw. maskiert, siehe `observe.payload`).
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestPayload {
    /// Request-ID.
    pub id: String,
    /// Modus beim Speichern (`metadata`, `masked`, `full`).
    pub mode: String,
    /// `{"request": {…}, "response": {…}}`.
    #[schema(value_type = Object)]
    pub payload: serde_json::Value,
}

/// Einzelanfrage.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestLogDetail {
    /// Metadaten.
    #[serde(flatten)]
    pub request: RequestLogItem,
    /// Upstream-Versuche in Reihenfolge (leer bei älteren Einträgen bzw. ohne Upstream).
    pub attempts: Vec<AttemptRow>,
    /// Zugriff auf den Payload.
    pub payload_status: PayloadStatus,
    /// Payload (nur bei `payloadStatus = available`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<RequestPayload>,
}

// ---------------------------------------------------------------------------------------------
// Verzeichnis (Namen ↔ IDs) und Geltungsbereich
// ---------------------------------------------------------------------------------------------

struct TeamInfo {
    name: String,
    organization: String,
}

struct KeyInfo {
    name: String,
    team: Option<String>,
}

struct DeploymentInfo {
    name: String,
    provider: String,
}

/// Namen und Beziehungen der Objekte laut aktueller Konfiguration (je Anfrage geladen).
struct Directory {
    orgs: HashMap<String, String>,
    teams: HashMap<String, TeamInfo>,
    keys: HashMap<String, KeyInfo>,
    deployments: HashMap<String, DeploymentInfo>,
}

impl Directory {
    async fn load(store: &Store) -> Result<Self, Problem> {
        let orgs = store
            .list::<OrganizationSpec>()
            .await?
            .into_iter()
            .map(|o| (o.id.to_string(), o.metadata.name))
            .collect();
        let teams = store
            .list::<TeamSpec>()
            .await?
            .into_iter()
            .map(|t| {
                (
                    t.id.to_string(),
                    TeamInfo {
                        name: t.metadata.name,
                        organization: t.spec.organization,
                    },
                )
            })
            .collect();
        let keys = store
            .list_api_keys()
            .await?
            .into_iter()
            .map(|k| {
                (
                    k.id.to_string(),
                    KeyInfo {
                        name: k.metadata.name,
                        team: k.spec.team,
                    },
                )
            })
            .collect();
        let deployments = store
            .list::<DeploymentSpec>()
            .await?
            .into_iter()
            .map(|d| {
                (
                    d.id.to_string(),
                    DeploymentInfo {
                        name: d.metadata.name,
                        provider: d.spec.provider.as_str().to_owned(),
                    },
                )
            })
            .collect();
        Ok(Self {
            orgs,
            teams,
            keys,
            deployments,
        })
    }

    fn team_id_by_name(&self, name: &str) -> Option<&str> {
        self.teams
            .iter()
            .find(|(_, t)| t.name == name)
            .map(|(id, _)| id.as_str())
    }

    /// Ziel einer Anfrage für die Rechteprüfung: das Team (über Key bzw. Team-ID), sonst global.
    fn target_of(&self, team_id: Option<&str>) -> Target {
        match team_id.and_then(|id| self.teams.get(id)) {
            Some(t) => Target::in_team(t.organization.clone(), t.name.clone()),
            None => Target::Global,
        }
    }

    /// Anzeigename eines Gruppenschlüssels.
    fn name(&self, dim: Dimension, key: &str) -> Option<String> {
        match dim {
            Dimension::ModelGroup | Dimension::Provider => {
                (!key.is_empty() && key != OTHER_KEY).then(|| key.to_owned())
            }
            Dimension::Deployment => self.deployments.get(key).map(|d| d.name.clone()),
            Dimension::Team => self.teams.get(key).map(|t| t.name.clone()),
            Dimension::Key => self.keys.get(key).map(|k| k.name.clone()),
            Dimension::Org => self.orgs.get(key).cloned(),
        }
    }

    /// Anbieter eines Deployments (`unknown` bei gelöschten, `""` ohne Deployment).
    fn provider_of(&self, deployment_id: &str) -> String {
        if deployment_id.is_empty() {
            return String::new();
        }
        self.deployments
            .get(deployment_id)
            .map_or_else(|| "unknown".to_owned(), |d| d.provider.clone())
    }
}

/// Sichtbarer Bereich des Principals.
enum Scope {
    /// Alles (globale Leserolle).
    All,
    /// Nur diese Team-IDs.
    Teams(BTreeSet<String>),
}

fn scope_of(p: &Principal, dir: &Directory) -> Scope {
    if p.can(Permission::Read, &Target::Global) {
        return Scope::All;
    }
    Scope::Teams(
        dir.teams
            .iter()
            .filter(|(_, t)| {
                p.can(
                    Permission::Read,
                    &Target::in_team(t.organization.clone(), t.name.clone()),
                )
            })
            .map(|(id, _)| id.clone())
            .collect(),
    )
}

fn split(v: Option<&str>) -> Option<Vec<String>> {
    v.map(|s| {
        s.split(',')
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .map(str::to_owned)
            .collect()
    })
}

fn looks_like_id(s: &str) -> bool {
    uuid_like(s)
}

fn uuid_like(s: &str) -> bool {
    s.len() == 36 && s.parse::<aigw_core::resource::Id>().is_ok()
}

/// Löst Namen oder IDs auf; unbekannte Namen → 422.
fn resolve<'a, I>(values: &[String], what: &str, by_name: I) -> Result<Vec<String>, Problem>
where
    I: Fn(&str) -> Option<&'a str>,
{
    values
        .iter()
        .map(|v| {
            if looks_like_id(v) {
                Ok(v.to_lowercase())
            } else {
                by_name(v)
                    .map(str::to_owned)
                    .ok_or_else(|| Problem::invalid(format!("unbekannte(r) {what} {v:?}")))
            }
        })
        .collect()
}

fn intersect(a: Option<Vec<String>>, b: Option<Vec<String>>) -> Option<Vec<String>> {
    match (a, b) {
        (None, x) | (x, None) => x,
        (Some(a), Some(b)) => Some(a.into_iter().filter(|x| b.contains(x)).collect()),
    }
}

/// Aufgelöste Filter.
struct Resolved {
    keys: KeyFilter,
    /// Anbieter (für das Rohlog; in `keys.deployments` bereits auf Deployments abgebildet).
    providers: Option<Vec<String>>,
}

fn resolve_filters(
    f: &Filters,
    dir: &Directory,
    scope: &Scope,
    p: &Principal,
) -> Result<Resolved, Problem> {
    let teams = match split(f.team.as_deref()) {
        Some(v) => Some(resolve(&v, "Team", |n| dir.team_id_by_name(n))?),
        None => None,
    };
    let keys = match split(f.key.as_deref()) {
        Some(v) => Some(resolve(&v, "API-Key", |n| {
            dir.keys
                .iter()
                .find(|(_, k)| k.name == n)
                .map(|(id, _)| id.as_str())
        })?),
        None => None,
    };
    let organizations = match split(f.organization.as_deref()) {
        Some(v) => Some(resolve(&v, "Organisation", |n| {
            dir.orgs
                .iter()
                .find(|(_, o)| o.as_str() == n)
                .map(|(id, _)| id.as_str())
        })?),
        None => None,
    };
    let deployments = match split(f.deployment.as_deref()) {
        Some(v) => Some(resolve(&v, "Deployment", |n| {
            dir.deployments
                .iter()
                .find(|(_, d)| d.name == n)
                .map(|(id, _)| id.as_str())
        })?),
        None => None,
    };
    let providers = split(f.provider.as_deref());
    let provider_deployments = providers.as_ref().map(|ps| {
        dir.deployments
            .iter()
            .filter(|(_, d)| ps.contains(&d.provider))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>()
    });

    // Geltungsbereich: ausdrücklich gewählte Teams/Keys außerhalb → 403; sonst schneiden.
    let teams = match scope {
        Scope::All => teams,
        Scope::Teams(allowed) => {
            if let Some(t) = &teams
                && t.iter().any(|id| !allowed.contains(id))
            {
                return Err(Problem::forbidden("Team außerhalb des Geltungsbereichs"));
            }
            if let Some(k) = &keys {
                for id in k {
                    let team = dir.keys.get(id).and_then(|k| k.team.as_deref());
                    let target = match team.and_then(|t| dir.team_id_by_name(t)) {
                        Some(tid) => dir.target_of(Some(tid)),
                        None => Target::Global,
                    };
                    if !p.can(Permission::Read, &target) {
                        return Err(Problem::forbidden("API-Key außerhalb des Geltungsbereichs"));
                    }
                }
            }
            intersect(teams, Some(allowed.iter().cloned().collect()))
        }
    };
    Ok(Resolved {
        keys: KeyFilter {
            api_keys: keys,
            teams,
            organizations,
            model_groups: split(f.model_group.as_deref()),
            deployments: intersect(deployments, provider_deployments),
        },
        providers,
    })
}

// ---------------------------------------------------------------------------------------------
// Zeit
// ---------------------------------------------------------------------------------------------

fn now_ms() -> i64 {
    i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(i64::MAX)
}

fn rfc3339(ms: i64) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .ok()
        .and_then(|t| t.format(&Rfc3339).ok())
        .unwrap_or_default()
}

fn parse_time(name: &str, v: &str) -> Result<i64, Problem> {
    let t = OffsetDateTime::parse(v, &Rfc3339)
        .map_err(|_| Problem::bad_request(format!("{name}: kein RFC-3339-Zeitpunkt")))?;
    i64::try_from(t.unix_timestamp_nanos() / 1_000_000)
        .map_err(|_| Problem::bad_request(format!("{name}: außerhalb des Wertebereichs")))
}

fn floor(v: i64, step: i64) -> i64 {
    v.div_euclid(step) * step
}

fn ceil(v: i64, step: i64) -> i64 {
    let f = floor(v, step);
    if f == v { v } else { f + step }
}

/// Zeitraum aus `from`/`to` (Standard: letzte 24 h), auf Minuten ausgerichtet.
fn range(from: Option<&str>, to: Option<&str>) -> Result<(i64, i64), Problem> {
    let to = match to {
        Some(t) => parse_time("to", t)?,
        None => now_ms(),
    };
    let from = match from {
        Some(f) => parse_time("from", f)?,
        None => to - DEFAULT_RANGE_MS,
    };
    let (from, to) = (floor(from, MINUTE_MS), ceil(to, MINUTE_MS));
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

fn pick_step(from: i64, to: i64, step: Option<Step>) -> Result<Step, Problem> {
    let points = |s: Step| (ceil(to, s.ms()) - floor(from, s.ms())) / s.ms();
    match step {
        Some(s) if points(s) > MAX_POINTS => Err(Problem::bad_request(format!(
            "zu viele Punkte ({} > {MAX_POINTS}) – größeren step wählen",
            points(s)
        ))),
        Some(s) => Ok(s),
        None => Ok(Step::ALL
            .into_iter()
            .find(|s| points(*s) <= TARGET_POINTS)
            .unwrap_or(Step::D1)),
    }
}

// ---------------------------------------------------------------------------------------------
// Gruppierung
// ---------------------------------------------------------------------------------------------

/// Gesamtsummen je Gruppenschlüssel (für Anbieter bereits abgebildet) und Rohwerte je Schlüssel.
struct GroupTotals {
    totals: BTreeMap<String, RollupCounts>,
    raw: BTreeMap<String, Vec<String>>,
}

async fn group_totals(
    store: &Store,
    from: i64,
    to: i64,
    dim: Dimension,
    filter: &KeyFilter,
    dir: &Directory,
    columns: &[&'static str],
) -> Result<GroupTotals, Problem> {
    let rows = store
        .query_rollups(&RollupQuery {
            from_ms: from,
            to_ms: to,
            step_ms: None,
            grouping: Grouping::Raw(dim.column()),
            filter: filter.clone(),
            columns: columns.to_vec(),
        })
        .await?;
    let mut totals: BTreeMap<String, RollupCounts> = BTreeMap::new();
    let mut raw: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for r in rows {
        let label = if dim == Dimension::Provider {
            dir.provider_of(&r.group)
        } else {
            r.group.clone()
        };
        totals.entry(label.clone()).or_default().merge(&r.counts);
        raw.entry(label).or_default().push(r.group);
    }
    Ok(GroupTotals { totals, raw })
}

/// Rangfolge: summierbare Kennzahlen nach Wert, sonst nach Anfragen (Volumen).
fn rank(metric: Metric, totals: &BTreeMap<String, RollupCounts>) -> Vec<String> {
    let mut v: Vec<(&String, f64)> = totals
        .iter()
        .map(|(k, c)| {
            let score = if metric.additive() {
                metric.value(c).unwrap_or(0.0)
            } else {
                #[allow(clippy::cast_precision_loss)]
                let r = c.requests as f64;
                r
            };
            (k, score)
        })
        .collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    v.into_iter().map(|(k, _)| k.clone()).collect()
}

fn clamp_limit(v: Option<u32>, default: u32, max: u32) -> Result<u32, Problem> {
    match v {
        None => Ok(default),
        Some(n) if (1..=max).contains(&n) => Ok(n),
        Some(_) => Err(Problem::bad_request(format!("limit muss 1–{max} sein"))),
    }
}

struct Ctx {
    dir: Directory,
    resolved: Resolved,
}

async fn context(st: &AnalyticsState, p: &Principal, f: &Filters) -> Result<Ctx, Problem> {
    let dir = Directory::load(&st.store).await?;
    let scope = scope_of(p, &dir);
    let resolved = resolve_filters(f, &dir, &scope, p)?;
    Ok(Ctx { dir, resolved })
}

// ---------------------------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------------------------

/// Zeitreihe einer Kennzahl.
#[utoipa::path(
    get, path = "/admin/api/v1/analytics/timeseries", tag = "analytics", params(TimeseriesQuery),
    responses(
        (status = 200, description = "Zeitreihe", body = Timeseries),
        (status = 400, description = "Ungültige Parameter", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Filter außerhalb des Geltungsbereichs", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Unbekannter Name in einem Filter", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn timeseries(
    State(st): State<AnalyticsState>,
    Extension(p): Extension<Principal>,
    q: Result<Query<TimeseriesQuery>, QueryRejection>,
) -> Result<Json<Timeseries>, Problem> {
    let Query(q) = q?;
    let limit = clamp_limit(q.limit, 5, 20)?;
    let (from, to) = range(q.from.as_deref(), q.to.as_deref())?;
    let step = pick_step(from, to, q.step)?;
    let step_ms = step.ms();
    let (from, to) = (floor(from, step_ms), ceil(to, step_ms));
    let filters = Filters {
        model_group: q.model_group,
        deployment: q.deployment,
        provider: q.provider,
        team: q.team,
        key: q.key,
        organization: q.organization,
    };
    let ctx = context(&st, &p, &filters).await?;
    let filter = &ctx.resolved.keys;

    // Gruppen: erst Gesamtsummen je Gruppe (Rang), dann Zeitreihe mit Top-N + „other“.
    let (grouping, order, totals) = match q.group_by {
        None => (Grouping::None, vec![String::new()], None),
        Some(dim) => {
            let g = group_totals(
                &st.store,
                from,
                to,
                dim,
                filter,
                &ctx.dir,
                &q.metric.columns(),
            )
            .await?;
            let ranked = rank(q.metric, &g.totals);
            let top: Vec<String> = ranked.iter().take(limit as usize).cloned().collect();
            let has_other = ranked.len() > top.len();
            let groups = top
                .iter()
                .map(|l| (l.clone(), g.raw.get(l).cloned().unwrap_or_default()))
                .collect();
            let mut order = top;
            if has_other {
                order.push(OTHER_KEY.to_owned());
            }
            let mut totals = BTreeMap::new();
            for (label, c) in g.totals {
                let k = if order.contains(&label) {
                    label
                } else {
                    OTHER_KEY.to_owned()
                };
                totals
                    .entry(k)
                    .or_insert_with(RollupCounts::default)
                    .merge(&c);
            }
            (
                Grouping::Mapped {
                    dim: dim.column(),
                    groups,
                    other: OTHER_KEY.to_owned(),
                },
                order,
                Some(totals),
            )
        }
    };
    let rows = st
        .store
        .query_rollups(&RollupQuery {
            from_ms: from,
            to_ms: to,
            step_ms: Some(step_ms),
            grouping,
            filter: filter.clone(),
            columns: q.metric.columns(),
        })
        .await?;
    let mut by_group: HashMap<String, BTreeMap<i64, RollupCounts>> = HashMap::new();
    for r in rows {
        by_group
            .entry(r.group)
            .or_default()
            .entry(r.bucket_ms)
            .or_default()
            .merge(&r.counts);
    }

    let empty = RollupCounts::default();
    let series = order
        .into_iter()
        .map(|key| {
            let buckets = by_group.remove(&key).unwrap_or_default();
            let mut total = RollupCounts::default();
            let mut points = Vec::new();
            let mut t = from;
            while t < to {
                let c = buckets.get(&t).unwrap_or(&empty);
                total.merge(c);
                points.push(Point {
                    t: rfc3339(t),
                    v: q.metric.value(c),
                    amount: q.metric.amount(c),
                });
                t += step_ms;
            }
            let total = totals
                .as_ref()
                .and_then(|m| m.get(&key).cloned())
                .unwrap_or(total);
            let other = key == OTHER_KEY;
            let (key, name) = match q.group_by {
                None => (TOTAL_KEY.to_owned(), None),
                Some(dim) => {
                    let name = if other { None } else { ctx.dir.name(dim, &key) };
                    (key, name)
                }
            };
            Series {
                key,
                name,
                other,
                total: q.metric.value(&total),
                total_amount: q.metric.amount(&total),
                points,
            }
        })
        .collect();

    Ok(Json(Timeseries {
        metric: q.metric,
        unit: q.metric.unit(),
        from: rfc3339(from),
        to: rfc3339(to),
        step,
        step_seconds: step_ms / 1000,
        group_by: q.group_by,
        series,
    }))
}

async fn totals(
    store: &Store,
    from: i64,
    to: i64,
    filter: &KeyFilter,
) -> Result<RollupCounts, Problem> {
    let rows = store
        .query_rollups(&RollupQuery {
            from_ms: from,
            to_ms: to,
            step_ms: None,
            grouping: Grouping::None,
            filter: filter.clone(),
            columns: Vec::new(),
        })
        .await?;
    let mut c = RollupCounts::default();
    for r in rows {
        c.merge(&r.counts);
    }
    Ok(c)
}

fn kpi(metric: Metric, cur: &RollupCounts, prev: &RollupCounts) -> Kpi {
    let value = metric.value(cur);
    let previous = metric.value(prev);
    let delta_percent = match (value, previous) {
        (Some(v), Some(p)) if p != 0.0 => Some((v - p) / p.abs() * 100.0),
        _ => None,
    };
    Kpi {
        value,
        previous,
        delta_percent,
        amount: metric.amount(cur),
        previous_amount: metric.amount(prev),
    }
}

/// KPIs im Zeitraum und Vergleich mit der gleich langen Vorperiode.
#[utoipa::path(
    get, path = "/admin/api/v1/analytics/summary", tag = "analytics", params(SummaryQuery),
    responses(
        (status = 200, description = "KPIs", body = AnalyticsSummary),
        (status = 400, description = "Ungültige Parameter", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Filter außerhalb des Geltungsbereichs", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Unbekannter Name in einem Filter", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn summary(
    State(st): State<AnalyticsState>,
    Extension(p): Extension<Principal>,
    q: Result<Query<SummaryQuery>, QueryRejection>,
) -> Result<Json<AnalyticsSummary>, Problem> {
    let Query(q) = q?;
    let (from, to) = range(q.from.as_deref(), q.to.as_deref())?;
    let prev_from = from - (to - from);
    let filters = Filters {
        model_group: q.model_group,
        deployment: q.deployment,
        provider: q.provider,
        team: q.team,
        key: q.key,
        organization: q.organization,
    };
    let ctx = context(&st, &p, &filters).await?;
    let filter = &ctx.resolved.keys;
    let (cur, prev) = tokio::try_join!(
        totals(&st.store, from, to, filter),
        totals(&st.store, prev_from, from, filter)
    )?;
    Ok(Json(AnalyticsSummary {
        from: rfc3339(from),
        to: rfc3339(to),
        previous_from: rfc3339(prev_from),
        previous_to: rfc3339(from),
        requests: kpi(Metric::Requests, &cur, &prev),
        tokens: kpi(Metric::Tokens, &cur, &prev),
        cost: kpi(Metric::Cost, &cur, &prev),
        latency_p95_ms: kpi(Metric::LatencyP95, &cur, &prev),
        error_rate: kpi(Metric::ErrorRate, &cur, &prev),
        cache_hit_rate: kpi(Metric::CacheHitRate, &cur, &prev),
    }))
}

/// Rangliste einer Dimension nach einer Kennzahl.
#[utoipa::path(
    get, path = "/admin/api/v1/analytics/top", tag = "analytics", params(TopQuery),
    responses(
        (status = 200, description = "Rangliste", body = TopList),
        (status = 400, description = "Ungültige Parameter", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Filter außerhalb des Geltungsbereichs", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Unbekannter Name in einem Filter", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn top(
    State(st): State<AnalyticsState>,
    Extension(p): Extension<Principal>,
    q: Result<Query<TopQuery>, QueryRejection>,
) -> Result<Json<TopList>, Problem> {
    let Query(q) = q?;
    let limit = clamp_limit(q.limit, 10, 100)?;
    let metric = q.metric.unwrap_or(Metric::Requests);
    let (from, to) = range(q.from.as_deref(), q.to.as_deref())?;
    let filters = Filters {
        model_group: q.model_group,
        deployment: q.deployment,
        provider: q.provider,
        team: q.team,
        key: q.key,
        organization: q.organization,
    };
    let ctx = context(&st, &p, &filters).await?;
    let g = group_totals(
        &st.store,
        from,
        to,
        q.dimension,
        &ctx.resolved.keys,
        &ctx.dir,
        &metric.columns(),
    )
    .await?;
    let grand: Option<f64> = metric
        .additive()
        .then(|| g.totals.values().filter_map(|c| metric.value(c)).sum());
    let mut items: Vec<TopItem> = g
        .totals
        .iter()
        .map(|(k, c)| {
            let value = metric.value(c);
            TopItem {
                key: k.clone(),
                name: ctx.dir.name(q.dimension, k),
                value,
                amount: metric.amount(c),
                requests: c.requests,
                share: match (grand, value) {
                    (Some(t), Some(v)) if t > 0.0 => Some(v / t),
                    _ => None,
                },
            }
        })
        .collect();
    items.sort_by(|a, b| match (a.value, b.value) {
        (Some(x), Some(y)) => y.total_cmp(&x).then_with(|| a.key.cmp(&b.key)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.key.cmp(&b.key),
    });
    items.truncate(limit as usize);
    Ok(Json(TopList {
        dimension: q.dimension,
        metric,
        unit: metric.unit(),
        from: rfc3339(from),
        to: rfc3339(to),
        items,
    }))
}

fn item(row: RequestLogRow, has_payload: bool, dir: &Directory) -> RequestLogItem {
    let name = |m: Option<&String>, f: &dyn Fn(&str) -> Option<String>| m.and_then(|id| f(id));
    RequestLogItem {
        api_key: name(row.api_key_id.as_ref(), &|id| {
            dir.keys.get(id).map(|k| k.name.clone())
        }),
        team: name(row.team_id.as_ref(), &|id| {
            dir.teams.get(id).map(|t| t.name.clone())
        }),
        organization: name(row.organization_id.as_ref(), &|id| {
            dir.orgs.get(id).cloned()
        }),
        deployment: name(row.deployment_id.as_ref(), &|id| {
            dir.deployments.get(id).map(|d| d.name.clone())
        }),
        at: rfc3339(row.at),
        cost: Amount::from_nanos(row.cost),
        attempt_count: row.attempts.len(),
        has_payload,
        id: row.id,
        endpoint: row.endpoint,
        api_key_id: row.api_key_id,
        team_id: row.team_id,
        organization_id: row.organization_id,
        model_requested: row.model_requested,
        model_group: row.model_group,
        deployment_id: row.deployment_id,
        provider: row.provider,
        provider_model: row.provider_model,
        status: row.status,
        error_code: row.error_code,
        input_tokens: row.input_tokens,
        output_tokens: row.output_tokens,
        cached_tokens: row.cached_tokens,
        cache_write_tokens: row.cache_write_tokens,
        reasoning_tokens: row.reasoning_tokens,
        usage_estimated: row.usage_estimated,
        latency_ms: row.latency_ms,
        ttft_ms: row.ttft_ms,
        cache: row.cache,
        stream: row.stream,
        client_aborted: row.client_aborted,
        client_ip: row.client_ip,
        client_request_id: row.client_request_id,
    }
}

fn encode_cursor(at: i64, id: &str) -> String {
    URL_SAFE_NO_PAD.encode(format!("{at}|{id}"))
}

fn decode_cursor(c: &str) -> Result<(i64, String), Problem> {
    let bad = || Problem::bad_request("cursor ungültig");
    let raw = URL_SAFE_NO_PAD.decode(c).map_err(|_| bad())?;
    let s = String::from_utf8(raw).map_err(|_| bad())?;
    let (at, id) = s.split_once('|').ok_or_else(bad)?;
    Ok((at.parse().map_err(|_| bad())?, id.to_owned()))
}

fn parse_status(s: &str) -> Result<StatusFilter, Problem> {
    let bad = || Problem::bad_request("status: Code (z. B. 429), Klasse (4xx), error oder success");
    match s {
        "error" => Ok(StatusFilter::Error),
        "success" => Ok(StatusFilter::Success),
        _ if s.len() == 3 && s.ends_with("xx") => {
            let c = s[..1].parse::<i64>().map_err(|_| bad())?;
            if (1..=5).contains(&c) {
                Ok(StatusFilter::Class(c))
            } else {
                Err(bad())
            }
        }
        _ => match s.parse::<i64>() {
            Ok(c) if (100..=599).contains(&c) => Ok(StatusFilter::Code(c)),
            _ => Err(bad()),
        },
    }
}

fn parse_cache(s: &str) -> Result<CacheFilter, Problem> {
    match s {
        "hit" => Ok(CacheFilter::Hit),
        "miss" => Ok(CacheFilter::Miss),
        "none" => Ok(CacheFilter::None),
        "hit-exact" | "hit-semantic" => Ok(CacheFilter::Value(s.to_owned())),
        _ => Err(Problem::bad_request(
            "cache: hit, miss, none, hit-exact oder hit-semantic",
        )),
    }
}

/// Log-Explorer: Anfragen, neueste zuerst.
#[utoipa::path(
    get, path = "/admin/api/v1/request-logs", tag = "request-logs", params(LogListQuery),
    responses(
        (status = 200, description = "Seite", body = RequestLogList),
        (status = 400, description = "Ungültige Parameter", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Filter außerhalb des Geltungsbereichs", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Unbekannter Name in einem Filter", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn list_request_logs(
    State(st): State<AnalyticsState>,
    Extension(p): Extension<Principal>,
    q: Result<Query<LogListQuery>, QueryRejection>,
) -> Result<Json<RequestLogList>, Problem> {
    let Query(q) = q?;
    let limit = clamp_limit(q.limit, 50, 500)?;
    let from_ms = q
        .from
        .as_deref()
        .map(|v| parse_time("from", v))
        .transpose()?;
    let to_ms = q.to.as_deref().map(|v| parse_time("to", v)).transpose()?;
    let status = q.status.as_deref().map(parse_status).transpose()?;
    let cache = q.cache.as_deref().map(parse_cache).transpose()?;
    let before = q.cursor.as_deref().map(decode_cursor).transpose()?;
    let filters = Filters {
        model_group: q.model_group,
        deployment: q.deployment,
        provider: q.provider,
        team: q.team,
        key: q.key,
        organization: q.organization,
    };
    let ctx = context(&st, &p, &filters).await?;
    let mut keys = ctx.resolved.keys;
    // Im Rohlog steht der Anbieter direkt; die Abbildung auf Deployments gilt nur für Rollups.
    if ctx.resolved.providers.is_some() {
        keys.deployments = match split(filters.deployment.as_deref()) {
            Some(v) => Some(resolve(&v, "Deployment", |n| {
                ctx.dir
                    .deployments
                    .iter()
                    .find(|(_, d)| d.name == n)
                    .map(|(id, _)| id.as_str())
            })?),
            None => None,
        };
    }
    let mut rows = st
        .store
        .list_request_logs(&LogQuery {
            from_ms,
            to_ms,
            keys,
            providers: ctx.resolved.providers.clone(),
            status,
            error_codes: split(q.error_code.as_deref()),
            request_id: q.request_id,
            client_request_id: q.client_request_id,
            stream: q.stream,
            cache,
            before,
            limit: limit + 1,
        })
        .await?;
    let next_cursor = if rows.len() > limit as usize {
        rows.truncate(limit as usize);
        rows.last().map(|r| encode_cursor(r.row.at, &r.row.id))
    } else {
        None
    };
    let items = rows
        .into_iter()
        .map(|r| item(r.row, r.has_payload, &ctx.dir))
        .collect();
    Ok(Json(RequestLogList { items, next_cursor }))
}

/// Lädt eine Anfrage und prüft das Leserecht (Team der Anfrage, ohne Team: global).
async fn load_log(
    st: &AnalyticsState,
    p: &Principal,
    id: &str,
) -> Result<(RequestLogRow, Directory, Target), Problem> {
    let row = st
        .store
        .get_request_log(id)
        .await?
        .ok_or_else(|| Problem::not_found(format!("Anfrage {id:?} nicht gefunden")))?;
    let dir = Directory::load(&st.store).await?;
    let target = dir.target_of(row.team_id.as_deref());
    p.require_on(Permission::Read, &target)?;
    Ok((row, dir, target))
}

/// Eine Anfrage mit Upstream-Versuchen und – für `admin`/`auditor` – dem Payload.
#[utoipa::path(
    get, path = "/admin/api/v1/request-logs/{id}", tag = "request-logs",
    params(("id" = String, Path, description = "Request-ID")),
    responses(
        (status = 200, description = "Anfrage", body = RequestLogDetail),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Außerhalb des Geltungsbereichs", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Unbekannt bzw. abgelaufen", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn get_request_log(
    State(st): State<AnalyticsState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Json<RequestLogDetail>, Problem> {
    let (mut row, dir, target) = load_log(&st, &p, &id).await?;
    let attempts = std::mem::take(&mut row.attempts);
    let may_read = p.can(Permission::ReadPayload, &target);
    let (payload_status, payload, has_payload) = if may_read {
        match st.store.get_request_payload(&id).await? {
            Some(pl) => (
                PayloadStatus::Available,
                Some(RequestPayload {
                    id: pl.id,
                    mode: pl.mode,
                    payload: pl.payload,
                }),
                true,
            ),
            None => (PayloadStatus::NotStored, None, false),
        }
    } else if st.store.has_request_payload(&id).await? {
        (PayloadStatus::Forbidden, None, true)
    } else {
        (PayloadStatus::NotStored, None, false)
    };
    let mut request = item(row, has_payload, &dir);
    request.attempt_count = attempts.len();
    Ok(Json(RequestLogDetail {
        request,
        attempts,
        payload_status,
        payload,
    }))
}

/// Nur der gespeicherte Payload einer Anfrage (`admin`, `auditor`).
#[utoipa::path(
    get, path = "/admin/api/v1/request-logs/{id}/payload", tag = "request-logs",
    params(("id" = String, Path, description = "Request-ID")),
    responses(
        (status = 200, description = "Payload", body = RequestPayload),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Rolle darf Payloads nicht lesen bzw. außerhalb des Geltungsbereichs", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Anfrage unbekannt oder kein Payload gespeichert", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn get_request_payload(
    State(st): State<AnalyticsState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Json<RequestPayload>, Problem> {
    let (_, _, target) = load_log(&st, &p, &id).await?;
    if !p.can(Permission::ReadPayload, &target) {
        return Err(Problem::forbidden(
            "Payloads dürfen nur admin und auditor lesen",
        ));
    }
    let pl = st
        .store
        .get_request_payload(&id)
        .await?
        .ok_or_else(|| Problem::not_found("kein Payload gespeichert"))?;
    Ok(Json(RequestPayload {
        id: pl.id,
        mode: pl.mode,
        payload: pl.payload,
    }))
}

/// Router der Analyse- und Log-Routen (mit Anmeldung).
pub fn router(state: AnalyticsState) -> Router {
    Router::new()
        .route(TIMESERIES_PATH, get(timeseries))
        .route(SUMMARY_PATH, get(summary))
        .route(TOP_PATH, get(top))
        .route(LOGS_PATH, get(list_request_logs))
        .route(LOG_PATH, get(get_request_log))
        .route(LOG_PAYLOAD_PATH, get(get_request_payload))
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
    fn step_selection() {
        let h = 3_600_000;
        assert_eq!(pick_step(0, h, None).unwrap(), Step::M1);
        assert_eq!(pick_step(0, 24 * h, None).unwrap(), Step::M5);
        assert_eq!(pick_step(0, 7 * 24 * h, None).unwrap(), Step::H1);
        assert_eq!(pick_step(0, 30 * 24 * h, None).unwrap(), Step::D1);
        assert!(pick_step(0, 7 * 24 * h, Some(Step::M1)).is_err());
        assert_eq!(pick_step(0, 24 * h, Some(Step::M1)).unwrap(), Step::M1);
    }

    #[test]
    fn status_and_cursor() {
        assert_eq!(parse_status("4xx").unwrap(), StatusFilter::Class(4));
        assert_eq!(parse_status("429").unwrap(), StatusFilter::Code(429));
        assert_eq!(parse_status("error").unwrap(), StatusFilter::Error);
        assert!(parse_status("9xx").is_err() && parse_status("abc").is_err());
        let c = encode_cursor(123, "0190-abc");
        assert_eq!(decode_cursor(&c).unwrap(), (123, "0190-abc".to_owned()));
        assert!(decode_cursor("!!").is_err());
    }

    #[test]
    fn metric_values() {
        let c = RollupCounts {
            requests: 4,
            errors: 1,
            cache_hits: 2,
            input_tokens: 10,
            output_tokens: 5,
            cost: 1_500_000_000,
            ..RollupCounts::default()
        };
        assert_eq!(Metric::Tokens.value(&c), Some(15.0));
        assert_eq!(Metric::ErrorRate.value(&c), Some(0.25));
        assert_eq!(Metric::CacheHitRate.value(&c), Some(0.5));
        assert_eq!(Metric::Cost.value(&c), Some(1.5));
        assert_eq!(
            Metric::Cost.amount(&c).map(|a| a.nanos()),
            Some(1_500_000_000)
        );
        assert_eq!(Metric::LatencyP95.value(&c), None);
        assert_eq!(Metric::ErrorRate.value(&RollupCounts::default()), None);
    }
}
