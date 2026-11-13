// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Lesefunktionen für Dashboards und Log-Explorer (AIGW-46).
//!
//! - [`Store::query_rollups`]: Summen aus den Rollups je Zeit-Bucket und Gruppe. Für Bereiche
//!   über ganze Stunden liest die Abfrage `usage_rollup_hour` (≈ 60× weniger Zeilen), für die
//!   angebrochenen Stunden am Rand `usage_rollup`; die Teilergebnisse werden hier addiert.
//! - [`Store::list_request_logs`]: Rohlogs mit Filtern und Cursor (absteigend nach Zeit).
//! - [`histogram_quantile`]: Perzentile aus den festen Latenz-/TTFT-Buckets.
//!
//! Berechtigungen kennt der Store nicht: Die Admin-API übergibt den erlaubten Bereich als
//! Filter (z. B. erlaubte Team-IDs).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use sqlx::{AssertSqlSafe, Row as _};

use crate::observe::{
    HOUR_MS, LATENCY_BUCKET_COUNT, LATENCY_BUCKETS_MS, MINUTE_MS, RequestLogRow, RollupCounts,
    rollup_count_columns, rollup_counts_from_row,
};
use crate::{Store, StoreError};

/// Schlüsselspalte der Rollups, nach der gefiltert bzw. gruppiert werden kann.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RollupDim {
    /// `api_key_id`.
    ApiKey,
    /// `team_id`.
    Team,
    /// `organization_id`.
    Organization,
    /// `model_group` (Name).
    ModelGroup,
    /// `deployment_id`.
    Deployment,
}

impl RollupDim {
    fn column(self) -> &'static str {
        match self {
            Self::ApiKey => "api_key_id",
            Self::Team => "team_id",
            Self::Organization => "organization_id",
            Self::ModelGroup => "model_group",
            Self::Deployment => "deployment_id",
        }
    }
}

/// Filter auf Schlüsselspalten. `Some(liste)` = Wert muss in der Liste stehen (leere Liste =
/// nichts); mehrere Felder gelten zusammen (UND). In Rollups steht `''` für „kein Wert“, im
/// Rohlog `NULL` (passt dort zu keinem Filter).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyFilter {
    /// API-Key-IDs.
    pub api_keys: Option<Vec<String>>,
    /// Team-IDs (auch für den Geltungsbereich eingeschränkter Rollen).
    pub teams: Option<Vec<String>>,
    /// Organisations-IDs.
    pub organizations: Option<Vec<String>>,
    /// Modellgruppen (Namen).
    pub model_groups: Option<Vec<String>>,
    /// Deployment-IDs.
    pub deployments: Option<Vec<String>>,
}

impl KeyFilter {
    fn lists(&self) -> [(&'static str, Option<&Vec<String>>); 5] {
        [
            ("api_key_id", self.api_keys.as_ref()),
            ("team_id", self.teams.as_ref()),
            ("organization_id", self.organizations.as_ref()),
            ("model_group", self.model_groups.as_ref()),
            ("deployment_id", self.deployments.as_ref()),
        ]
    }

    /// `true`, wenn ein Filter eine leere Liste hat (Ergebnis ist sicher leer).
    pub fn is_empty_set(&self) -> bool {
        self.lists()
            .iter()
            .any(|(_, l)| l.is_some_and(Vec::is_empty))
    }
}

/// Gruppierung einer Rollup-Abfrage.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Grouping {
    /// Keine Gruppe (Gruppe = `""`).
    #[default]
    None,
    /// Nach den Rohwerten einer Spalte.
    Raw(RollupDim),
    /// Rohwerte auf Bezeichner abbilden (Top-N, Anbieter aus Deployments); alle übrigen Werte
    /// landen unter `other`.
    Mapped {
        /// Spalte.
        dim: RollupDim,
        /// Bezeichner → zugehörige Rohwerte.
        groups: Vec<(String, Vec<String>)>,
        /// Bezeichner für alle übrigen Werte.
        other: String,
    },
}

/// Rollup-Abfrage. `from_ms`/`to_ms` sollten auf Minuten ausgerichtet sein (Minuten-Rollups
/// werden ganz gezählt, sobald ihr Beginn im Bereich liegt).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RollupQuery {
    /// Beginn (inklusive, Unix-ms).
    pub from_ms: i64,
    /// Ende (exklusive, Unix-ms).
    pub to_ms: i64,
    /// Bucket-Breite (ms, Vielfaches einer Minute, an der Epoche ausgerichtet); `None` = ein
    /// Bucket für den ganzen Bereich (`bucket_ms` = 0).
    pub step_ms: Option<i64>,
    /// Gruppierung.
    pub grouping: Grouping,
    /// Filter.
    pub filter: KeyFilter,
    /// Nur diese Zählspalten summieren (z. B. `requests`, `lat_le_25` …); die übrigen sind 0.
    /// Leer = alle. Spart bei vielen Minuten-Zeilen deutlich Zeit.
    pub columns: Vec<&'static str>,
}

/// Ein Ergebnis von [`Store::query_rollups`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollupBucket {
    /// Bucket-Beginn (Unix-ms) bzw. 0 ohne `step_ms`.
    pub bucket_ms: i64,
    /// Gruppe (Rohwert bzw. Bezeichner, `""` ohne Gruppierung).
    pub group: String,
    /// Summen.
    pub counts: RollupCounts,
}

/// Tabelle eines Abfrageabschnitts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resolution {
    Minute,
    Hour,
}

/// Zerlegt `[from, to)` in Abschnitte: ganze Stunden aus den Stunden-Rollups, Ränder aus den
/// Minuten-Rollups. Mit einer Bucket-Breite, die kein Vielfaches einer Stunde ist, nur Minuten.
fn segments(from: i64, to: i64, step: Option<i64>) -> Vec<(Resolution, i64, i64)> {
    if from >= to {
        return Vec::new();
    }
    let hourly_ok = step.is_none_or(|s| s % HOUR_MS == 0);
    let h1 = from.div_euclid(HOUR_MS) * HOUR_MS + if from % HOUR_MS == 0 { 0 } else { HOUR_MS };
    let h2 = to.div_euclid(HOUR_MS) * HOUR_MS;
    if !hourly_ok || h1 >= h2 {
        return vec![(Resolution::Minute, from, to)];
    }
    let mut out = Vec::with_capacity(3);
    if from < h1 {
        out.push((Resolution::Minute, from, h1));
    }
    out.push((Resolution::Hour, h1, h2));
    if h2 < to {
        out.push((Resolution::Minute, h2, to));
    }
    out
}

/// Eine Bindung (Text oder Zahl).
enum Bind {
    Text(String),
    Int(i64),
}

/// Baut Platzhalter `$n` in Reihenfolge und merkt sich die Bindungen.
#[derive(Default)]
struct Binds(Vec<Bind>);

impl Binds {
    fn push(&mut self, v: &str) -> String {
        self.0.push(Bind::Text(v.to_owned()));
        format!("${}", self.0.len())
    }

    fn int(&mut self, v: i64) -> String {
        self.0.push(Bind::Int(v));
        format!("${}", self.0.len())
    }

    fn list(&mut self, vs: &[String]) -> String {
        vs.iter()
            .map(|v| self.push(v))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn apply<'q>(
        self,
        mut q: sqlx::query::Query<'q, sqlx::Any, sqlx::any::AnyArguments>,
    ) -> sqlx::query::Query<'q, sqlx::Any, sqlx::any::AnyArguments> {
        for b in self.0 {
            q = match b {
                Bind::Text(t) => q.bind(t),
                Bind::Int(i) => q.bind(i),
            };
        }
        q
    }
}

fn key_filter_sql(f: &KeyFilter, binds: &mut Binds, sql: &mut String) {
    for (col, list) in f.lists() {
        if let Some(l) = list {
            let _ = write!(sql, " AND {col} IN ({})", binds.list(l));
        }
    }
}

impl Store {
    /// Summen aus den Rollups je Bucket und Gruppe, sortiert nach (Bucket, Gruppe).
    pub async fn query_rollups(&self, q: &RollupQuery) -> Result<Vec<RollupBucket>, StoreError> {
        if let Some(s) = q.step_ms
            && (s <= 0 || s % MINUTE_MS != 0)
        {
            return Err(StoreError::Invalid(format!(
                "Bucket-Breite {s} ms ist kein Vielfaches einer Minute"
            )));
        }
        if let Some(c) = q
            .columns
            .iter()
            .find(|c| !rollup_count_columns().any(|k| k == *c))
        {
            return Err(StoreError::Invalid(format!("unbekannte Rollup-Spalte {c}")));
        }
        let mut acc: BTreeMap<(i64, String), RollupCounts> = BTreeMap::new();
        if q.filter.is_empty_set() {
            return Ok(Vec::new());
        }
        for (res, from, to) in segments(q.from_ms, q.to_ms, q.step_ms) {
            let table = match res {
                Resolution::Minute => "usage_rollup",
                Resolution::Hour => "usage_rollup_hour",
            };
            self.rollup_segment(table, from, to, q, &mut acc).await?;
        }
        Ok(acc
            .into_iter()
            .map(|((bucket_ms, group), counts)| RollupBucket {
                bucket_ms,
                group,
                counts,
            })
            .collect())
    }

    async fn rollup_segment(
        &self,
        table: &str,
        from: i64,
        to: i64,
        q: &RollupQuery,
        acc: &mut BTreeMap<(i64, String), RollupCounts>,
    ) -> Result<(), StoreError> {
        let mut binds = Binds::default();
        let p_from = binds.int(from);
        let p_to = binds.int(to);
        let bucket = match q.step_ms {
            Some(s) => format!("(bucket_start / {s}) * {s}"),
            None => "CAST(0 AS BIGINT)".to_owned(),
        };
        let group = match &q.grouping {
            Grouping::None => "''".to_owned(),
            Grouping::Raw(d) => d.column().to_owned(),
            Grouping::Mapped { dim, groups, other } => {
                let mut e = String::from("CASE");
                for (label, values) in groups {
                    if values.is_empty() {
                        continue;
                    }
                    let vals = binds.list(values);
                    let l = binds.push(label);
                    let _ = write!(e, " WHEN {} IN ({vals}) THEN {l}", dim.column());
                }
                let o = binds.push(other);
                if e == "CASE" {
                    o
                } else {
                    let _ = write!(e, " ELSE {o} END");
                    e
                }
            }
        };
        let mut sql = format!("SELECT {bucket} AS b, {group} AS g");
        for c in rollup_count_columns() {
            if q.columns.is_empty() || q.columns.contains(c) {
                let _ = write!(sql, ", CAST(SUM({c}) AS BIGINT) AS {c}");
            } else {
                let _ = write!(sql, ", CAST(0 AS BIGINT) AS {c}");
            }
        }
        let _ = write!(
            sql,
            " FROM {table} WHERE bucket_start >= {p_from} AND bucket_start < {p_to}"
        );
        key_filter_sql(&q.filter, &mut binds, &mut sql);
        sql.push_str(" GROUP BY 1, 2");
        let rows = binds
            .apply(sqlx::query(AssertSqlSafe(sql)))
            .fetch_all(&self.pool)
            .await?;
        for row in &rows {
            let b: i64 = row.try_get("b")?;
            let g: String = row.try_get("g")?;
            let counts = rollup_counts_from_row(row)?;
            acc.entry((b, g)).or_default().merge(&counts);
        }
        Ok(())
    }

    /// Rohlogs nach [`LogQuery`], absteigend nach (`at`, `id`), höchstens `limit` Einträge.
    pub async fn list_request_logs(&self, q: &LogQuery) -> Result<Vec<LogListItem>, StoreError> {
        if q.keys.is_empty_set() || q.providers.as_ref().is_some_and(Vec::is_empty) {
            return Ok(Vec::new());
        }
        let mut binds = Binds::default();
        let mut sql = format!(
            "SELECT {}, CASE WHEN EXISTS (SELECT 1 FROM request_payload p WHERE p.id = \
             request_log.id) THEN CAST(1 AS BIGINT) ELSE CAST(0 AS BIGINT) END AS has_payload \
             FROM request_log WHERE 1 = 1",
            crate::observe::REQUEST_LOG_COLUMNS.join(", ")
        );
        if let Some(f) = q.from_ms {
            let p = binds.int(f);
            let _ = write!(sql, " AND at >= {p}");
        }
        if let Some(t) = q.to_ms {
            let p = binds.int(t);
            let _ = write!(sql, " AND at < {p}");
        }
        key_filter_sql(&q.keys, &mut binds, &mut sql);
        if let Some(p) = &q.providers {
            let _ = write!(sql, " AND provider IN ({})", binds.list(p));
        }
        match q.status {
            None => {}
            Some(StatusFilter::Code(c)) => {
                let p = binds.int(c);
                let _ = write!(sql, " AND status = {p}");
            }
            Some(StatusFilter::Class(c)) => {
                let lo = binds.int(c * 100);
                let hi = binds.int(c * 100 + 100);
                let _ = write!(sql, " AND status >= {lo} AND status < {hi}");
            }
            Some(StatusFilter::Error) => {
                sql.push_str(" AND (status >= 400 OR error_code IS NOT NULL)");
            }
            Some(StatusFilter::Success) => {
                sql.push_str(" AND status < 400 AND error_code IS NULL");
            }
        }
        if let Some(e) = &q.error_codes {
            let _ = write!(sql, " AND error_code IN ({})", binds.list(e));
        }
        if let Some(id) = &q.request_id {
            let _ = write!(sql, " AND id = {}", binds.push(id));
        }
        if let Some(id) = &q.client_request_id {
            let _ = write!(sql, " AND client_request_id = {}", binds.push(id));
        }
        if let Some(s) = q.stream {
            let p = binds.int(i64::from(s));
            let _ = write!(sql, " AND stream = {p}");
        }
        match &q.cache {
            None => {}
            Some(CacheFilter::Hit) => sql.push_str(" AND cache LIKE 'hit%'"),
            Some(CacheFilter::Miss) => sql.push_str(" AND cache = 'miss'"),
            Some(CacheFilter::None) => sql.push_str(" AND cache IS NULL"),
            Some(CacheFilter::Value(v)) => {
                let _ = write!(sql, " AND cache = {}", binds.push(v));
            }
        }
        if let Some((at, id)) = &q.before {
            let a1 = binds.int(*at);
            let a2 = binds.int(*at);
            let i = binds.push(id);
            let _ = write!(sql, " AND (at < {a1} OR (at = {a2} AND id < {i}))");
        }
        let _ = write!(sql, " ORDER BY at DESC, id DESC LIMIT {}", q.limit.max(1));

        let query = binds.apply(sqlx::query(AssertSqlSafe(sql)));
        let rows = query.fetch_all(&self.pool).await?;
        rows.iter()
            .map(|r| {
                Ok(LogListItem {
                    row: crate::observe::request_log_from_row(r)?,
                    has_payload: r.try_get::<i64, _>("has_payload")? != 0,
                })
            })
            .collect()
    }

    /// Aktualisiert die Planer-Statistiken (`ANALYZE`), z. B. nach einem Massenimport.
    pub async fn analyze(&self) -> Result<(), StoreError> {
        sqlx::query("ANALYZE").execute(&self.pool).await?;
        Ok(())
    }

    /// `true`, wenn zu der Anfrage ein Payload gespeichert ist.
    pub async fn has_request_payload(&self, id: &str) -> Result<bool, StoreError> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_payload WHERE id = $1")
            .bind(id.to_owned())
            .fetch_one(&self.pool)
            .await?;
        Ok(n > 0)
    }
}

/// Filter auf den HTTP-Status im Log-Explorer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusFilter {
    /// Genau dieser Status.
    Code(i64),
    /// Statusklasse (`2` = 2xx …).
    Class(i64),
    /// Fehler: Status ≥ 400 oder Fehlerklasse gesetzt (wie `usage_rollup.errors`).
    Error,
    /// Erfolg: Status < 400 und keine Fehlerklasse.
    Success,
}

/// Filter auf das Cache-Ergebnis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheFilter {
    /// Treffer (exakt oder semantisch).
    Hit,
    /// Kein Treffer.
    Miss,
    /// Kein Cache beteiligt.
    None,
    /// Genau dieser Wert (`hit-exact`, `hit-semantic`).
    Value(String),
}

/// Abfrage des Log-Explorers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogQuery {
    /// Ab (inklusive, Unix-ms).
    pub from_ms: Option<i64>,
    /// Bis (exklusive, Unix-ms).
    pub to_ms: Option<i64>,
    /// Filter auf Key/Team/Organisation/Modellgruppe/Deployment (inkl. Geltungsbereich).
    pub keys: KeyFilter,
    /// Anbieter (`provider`).
    pub providers: Option<Vec<String>>,
    /// HTTP-Status.
    pub status: Option<StatusFilter>,
    /// Fehlerklassen (`error_code`).
    pub error_codes: Option<Vec<String>>,
    /// Genau diese Request-ID.
    pub request_id: Option<String>,
    /// Genau diese Client-Request-ID.
    pub client_request_id: Option<String>,
    /// Streaming ja/nein.
    pub stream: Option<bool>,
    /// Cache-Ergebnis.
    pub cache: Option<CacheFilter>,
    /// Cursor: nur Einträge echt vor (`at`, `id`).
    pub before: Option<(i64, String)>,
    /// Höchstzahl.
    pub limit: u32,
}

/// Ein Eintrag des Log-Explorers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogListItem {
    /// Rohlog-Zeile.
    pub row: RequestLogRow,
    /// Payload gespeichert.
    pub has_payload: bool,
}

/// Quantil `q` (0–1) aus einem nicht kumulativen Histogramm mit den Grenzen
/// [`LATENCY_BUCKETS_MS`] (+Inf), wie Prometheus' `histogram_quantile`:
///
/// - Rang `q · n`; gesucht ist der erste Bucket, in dem die kumulierte Anzahl den Rang erreicht.
/// - Innerhalb des Buckets linear interpoliert zwischen Unter- und Obergrenze (Annahme:
///   gleichverteilt). Der erste Bucket beginnt bei 0.
/// - Fällt der Rang in den +Inf-Bucket, ist das Ergebnis dessen Untergrenze (60 000 ms).
/// - `None` ohne Beobachtungen.
pub fn histogram_quantile(hist: &[i64; LATENCY_BUCKET_COUNT], q: f64) -> Option<f64> {
    let total: i64 = hist.iter().map(|v| (*v).max(0)).sum();
    if total == 0 {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    let rank = q.clamp(0.0, 1.0) * total as f64;
    let mut cum = 0.0_f64;
    let mut lower = 0.0_f64;
    for (i, &count) in hist.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let c = count.max(0) as f64;
        let Some(&upper) = LATENCY_BUCKETS_MS.get(i) else {
            return Some(lower);
        };
        #[allow(clippy::cast_precision_loss)]
        let upper = upper as f64;
        if c > 0.0 && cum + c >= rank {
            return Some(lower + (upper - lower) * ((rank - cum) / c));
        }
        cum += c;
        lower = upper;
    }
    Some(lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_split_hours() {
        let h = HOUR_MS;
        let m = MINUTE_MS;
        // innerhalb einer Stunde → nur Minuten
        assert_eq!(
            segments(h + m, h + 30 * m, None),
            vec![(Resolution::Minute, h + m, h + 30 * m)]
        );
        // Ränder + ganze Stunden
        assert_eq!(
            segments(h - 5 * m, 4 * h + 7 * m, None),
            vec![
                (Resolution::Minute, h - 5 * m, h),
                (Resolution::Hour, h, 4 * h),
                (Resolution::Minute, 4 * h, 4 * h + 7 * m),
            ]
        );
        // ausgerichtet → nur Stunden
        assert_eq!(
            segments(2 * h, 5 * h, Some(h)),
            vec![(Resolution::Hour, 2 * h, 5 * h)]
        );
        // 5-Minuten-Schritte → nur Minuten
        assert_eq!(
            segments(2 * h, 5 * h, Some(5 * m)),
            vec![(Resolution::Minute, 2 * h, 5 * h)]
        );
        assert!(segments(5, 5, None).is_empty());
    }

    #[test]
    fn quantiles_interpolate() {
        let mut h = [0; LATENCY_BUCKET_COUNT];
        assert_eq!(histogram_quantile(&h, 0.5), None);
        // 100 Beobachtungen im Bucket (50, 100] → p50 = 75, p95 = 97,5
        h[2] = 100;
        assert_eq!(histogram_quantile(&h, 0.5), Some(75.0));
        assert_eq!(histogram_quantile(&h, 0.95), Some(97.5));
        // 90 in [0, 25], 10 in (250, 500] → p50 = 13,9; p95 = 375
        let mut h = [0; LATENCY_BUCKET_COUNT];
        h[0] = 90;
        h[4] = 10;
        let p50 = histogram_quantile(&h, 0.5).unwrap();
        assert!((p50 - 25.0 * 50.0 / 90.0).abs() < 1e-9);
        assert_eq!(histogram_quantile(&h, 0.95), Some(375.0));
        assert_eq!(histogram_quantile(&h, 0.90), Some(25.0));
        // alles in +Inf → 60 000
        let mut h = [0; LATENCY_BUCKET_COUNT];
        h[11] = 3;
        assert_eq!(histogram_quantile(&h, 0.99), Some(60_000.0));
    }
}
