// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Request-Log, Payloads und Minuten-Rollups (AIGW-43).
//!
//! Geschrieben wird ausschließlich gebündelt vom Writer in `aigw-observe`: ein Batch = eine
//! Transaktion mit mehrzeiligen `INSERT`s (Rohlog, Payload) und additiven `UPSERT`s
//! (`ON CONFLICT … DO UPDATE SET x = usage_rollup.x + excluded.x`) für die Rollups. Das läuft
//! unverändert auf SQLite und Postgres und verträgt mehrere Instanzen ohne Read-Modify-Write.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use sqlx::any::AnyRow;
use sqlx::{AssertSqlSafe, Row as _};

use crate::{Backend, Store, StoreError};

/// Obergrenzen (inklusive, Millisekunden) der Latenz-Buckets; danach folgt `+Inf`.
pub const LATENCY_BUCKETS_MS: [i64; 11] = [
    25, 50, 100, 250, 500, 1000, 2500, 5000, 10_000, 30_000, 60_000,
];

/// Anzahl der Latenz-Buckets inkl. `+Inf`.
pub const LATENCY_BUCKET_COUNT: usize = LATENCY_BUCKETS_MS.len() + 1;

/// Spalten der Latenz-Buckets (in der Reihenfolge von [`LATENCY_BUCKETS_MS`], dann `+Inf`).
const LATENCY_COLUMNS: [&str; LATENCY_BUCKET_COUNT] = [
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

/// Index des Latenz-Buckets für `ms`.
pub fn latency_bucket(ms: i64) -> usize {
    LATENCY_BUCKETS_MS
        .iter()
        .position(|&le| ms <= le)
        .unwrap_or(LATENCY_BUCKETS_MS.len())
}

/// Mehrzeilige Statements: höchstens so viele Zeilen je Statement (Parametergrenzen:
/// SQLite 32 766, Postgres 65 535).
const ROWS_PER_STATEMENT: usize = 500;
/// Löschläufe arbeiten in Häppchen, damit Sperren kurz bleiben.
const PURGE_CHUNK: i64 = 10_000;

/// Eine Zeile in `request_log` (zugleich das Exportformat).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestLogRow {
    /// Request-ID.
    pub id: String,
    /// Annahme der Anfrage (Unix-ms).
    pub at: i64,
    /// Eingangsformat (`openai-chat`, `anthropic-messages`).
    pub endpoint: String,
    /// API-Key-ID.
    pub api_key_id: Option<String>,
    /// Team-ID.
    pub team_id: Option<String>,
    /// Organisations-ID.
    pub organization_id: Option<String>,
    /// Angefordertes Modell (Alias).
    pub model_requested: Option<String>,
    /// Modellgruppe.
    pub model_group: Option<String>,
    /// Deployment-ID.
    pub deployment_id: Option<String>,
    /// Anbieter.
    pub provider: Option<String>,
    /// Anbieter-Modell.
    pub provider_model: Option<String>,
    /// HTTP-Status an den Client.
    pub status: i64,
    /// Fehlerklasse (`snake_case` wie `ErrorKind`).
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
    /// Kosten (Nano-Einheiten).
    pub cost: i64,
    /// Gesamtdauer (ms).
    pub latency_ms: i64,
    /// Zeit bis zum ersten Token (ms).
    pub ttft_ms: Option<i64>,
    /// Cache-Ergebnis (`hit-exact`, `hit-semantic`, `miss`).
    pub cache: Option<String>,
    /// Streaming.
    pub stream: bool,
    /// Client-Abbruch.
    pub client_aborted: bool,
    /// Client-Adresse.
    pub client_ip: Option<String>,
}

/// Eine Zeile in `request_payload`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PayloadRow {
    /// Request-ID (= `request_log.id`).
    pub id: String,
    /// = `request_log.at` (Frist).
    pub at: i64,
    /// `metadata` | `masked` | `full`.
    pub mode: String,
    /// Inhalt (bereits maskiert bzw. gefiltert).
    pub payload: serde_json::Value,
}

/// Schlüssel eines Minuten-Rollups (`''` statt `NULL`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RollupKey {
    /// Minutenbeginn (Unix-ms).
    pub bucket_start: i64,
    /// API-Key-ID oder `''`.
    pub api_key_id: String,
    /// Team-ID oder `''`.
    pub team_id: String,
    /// Organisations-ID oder `''`.
    pub organization_id: String,
    /// Modellgruppe oder `''`.
    pub model_group: String,
    /// Deployment-ID oder `''`.
    pub deployment_id: String,
}

/// Additive Zähler eines Minuten-Rollups.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RollupCounts {
    /// Anfragen.
    pub requests: i64,
    /// Fehler (Status ≥ 400 oder Fehlerklasse).
    pub errors: i64,
    /// Cache-Treffer.
    pub cache_hits: i64,
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
    /// Kosten (Nano-Einheiten).
    pub cost: i64,
    /// Summe der Dauer (ms).
    pub latency_ms_sum: i64,
    /// Anfragen mit geschätzter Usage.
    pub estimated: i64,
    /// Client-Abbrüche.
    pub aborted: i64,
    /// Summe TTFT (ms).
    pub ttft_ms_sum: i64,
    /// Anfragen mit TTFT.
    pub ttft_count: i64,
    /// Anzahl je Latenz-Bucket (nicht kumulativ), siehe [`LATENCY_BUCKETS_MS`].
    pub latency_hist: [i64; LATENCY_BUCKET_COUNT],
}

impl RollupCounts {
    /// Werte ohne Histogramm, in Spaltenreihenfolge von [`ROLLUP_SCALAR_COLUMNS`].
    fn scalars(&self) -> [i64; 14] {
        [
            self.requests,
            self.errors,
            self.cache_hits,
            self.input_tokens,
            self.output_tokens,
            self.cached_tokens,
            self.cache_write_tokens,
            self.reasoning_tokens,
            self.cost,
            self.latency_ms_sum,
            self.estimated,
            self.aborted,
            self.ttft_ms_sum,
            self.ttft_count,
        ]
    }

    /// Addiert `other` (sättigend).
    pub fn merge(&mut self, other: &Self) {
        let add = |a: &mut i64, b: i64| *a = a.saturating_add(b);
        add(&mut self.requests, other.requests);
        add(&mut self.errors, other.errors);
        add(&mut self.cache_hits, other.cache_hits);
        add(&mut self.input_tokens, other.input_tokens);
        add(&mut self.output_tokens, other.output_tokens);
        add(&mut self.cached_tokens, other.cached_tokens);
        add(&mut self.cache_write_tokens, other.cache_write_tokens);
        add(&mut self.reasoning_tokens, other.reasoning_tokens);
        add(&mut self.cost, other.cost);
        add(&mut self.latency_ms_sum, other.latency_ms_sum);
        add(&mut self.estimated, other.estimated);
        add(&mut self.aborted, other.aborted);
        add(&mut self.ttft_ms_sum, other.ttft_ms_sum);
        add(&mut self.ttft_count, other.ttft_count);
        for (a, b) in self.latency_hist.iter_mut().zip(other.latency_hist) {
            add(a, b);
        }
    }
}

const REQUEST_LOG_COLUMNS: [&str; 27] = [
    "id",
    "at",
    "endpoint",
    "api_key_id",
    "team_id",
    "organization_id",
    "model_requested",
    "model_group",
    "deployment_id",
    "provider",
    "provider_model",
    "status",
    "error_code",
    "input_tokens",
    "output_tokens",
    "cached_tokens",
    "cache_write_tokens",
    "reasoning_tokens",
    "usage_estimated",
    "cost",
    "latency_ms",
    "ttft_ms",
    "cache",
    "stream",
    "client_aborted",
    "client_ip",
    "metadata",
];

const ROLLUP_KEY_COLUMNS: [&str; 6] = [
    "bucket_start",
    "api_key_id",
    "team_id",
    "organization_id",
    "model_group",
    "deployment_id",
];

const ROLLUP_SCALAR_COLUMNS: [&str; 14] = [
    "requests",
    "errors",
    "cache_hits",
    "input_tokens",
    "output_tokens",
    "cached_tokens",
    "cache_write_tokens",
    "reasoning_tokens",
    "cost",
    "latency_ms_sum",
    "estimated",
    "aborted",
    "ttft_ms_sum",
    "ttft_count",
];

/// `INSERT INTO table (cols) VALUES ($1,…),(…)` für `rows` Zeilen.
///
/// Für SQLite mit anonymen `?`: SQLite sucht benannte Parameter (`$n`) beim Parsen linear,
/// bei Tausenden Parametern je Statement wird das quadratisch (gemessen: 500 Zeilen ×
/// 27 Spalten ≈ 0,5 s statt ≈ 5 ms). Die Reihenfolge der Bindungen ist dieselbe.
fn insert_sql(backend: Backend, table: &str, cols: &[&str], rows: usize) -> String {
    let mut sql = format!("INSERT INTO {table} ({}) VALUES ", cols.join(", "));
    let mut n = 0;
    for r in 0..rows {
        if r > 0 {
            sql.push_str(", ");
        }
        sql.push('(');
        for c in 0..cols.len() {
            n += 1;
            if c > 0 {
                sql.push_str(", ");
            }
            match backend {
                Backend::Sqlite => sql.push('?'),
                Backend::Postgres => {
                    let _ = write!(sql, "${n}");
                }
            }
        }
        sql.push(')');
    }
    sql
}

fn b(v: bool) -> i64 {
    i64::from(v)
}

impl Store {
    /// Schreibt einen Batch in **einer** Transaktion: Rohlogs (doppelte IDs werden
    /// ignoriert), Payloads und additive Rollup-Deltas.
    ///
    /// `rollups` muss je Schlüssel höchstens einen Eintrag enthalten (Postgres lehnt
    /// doppelte Schlüssel in einem `UPSERT` ab).
    pub async fn write_request_batch(
        &self,
        logs: &[&RequestLogRow],
        payloads: &[&PayloadRow],
        rollups: &[(RollupKey, RollupCounts)],
    ) -> Result<(), StoreError> {
        if logs.is_empty() && payloads.is_empty() && rollups.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;

        for chunk in logs.chunks(ROWS_PER_STATEMENT) {
            let sql = insert_sql(
                self.backend(),
                "request_log",
                &REQUEST_LOG_COLUMNS,
                chunk.len(),
            ) + " ON CONFLICT (id) DO NOTHING";
            let mut q = sqlx::query(AssertSqlSafe(sql));
            for r in chunk {
                q = q
                    .bind(r.id.clone())
                    .bind(r.at)
                    .bind(r.endpoint.clone())
                    .bind(r.api_key_id.clone())
                    .bind(r.team_id.clone())
                    .bind(r.organization_id.clone())
                    .bind(r.model_requested.clone())
                    .bind(r.model_group.clone())
                    .bind(r.deployment_id.clone())
                    .bind(r.provider.clone())
                    .bind(r.provider_model.clone())
                    .bind(r.status)
                    .bind(r.error_code.clone())
                    .bind(r.input_tokens)
                    .bind(r.output_tokens)
                    .bind(r.cached_tokens)
                    .bind(r.cache_write_tokens)
                    .bind(r.reasoning_tokens)
                    .bind(b(r.usage_estimated))
                    .bind(r.cost)
                    .bind(r.latency_ms)
                    .bind(r.ttft_ms)
                    .bind(r.cache.clone())
                    .bind(b(r.stream))
                    .bind(b(r.client_aborted))
                    .bind(r.client_ip.clone())
                    .bind("{}");
            }
            q.execute(&mut *tx).await?;
        }

        for chunk in payloads.chunks(ROWS_PER_STATEMENT) {
            let sql = insert_sql(
                self.backend(),
                "request_payload",
                &["id", "at", "mode", "payload"],
                chunk.len(),
            ) + " ON CONFLICT (id) DO NOTHING";
            let mut q = sqlx::query(AssertSqlSafe(sql));
            for p in chunk {
                q = q
                    .bind(p.id.clone())
                    .bind(p.at)
                    .bind(p.mode.clone())
                    .bind(serde_json::to_string(&p.payload)?);
            }
            q.execute(&mut *tx).await?;
        }

        if !rollups.is_empty() {
            let cols: Vec<&str> = ROLLUP_KEY_COLUMNS
                .iter()
                .chain(&ROLLUP_SCALAR_COLUMNS)
                .chain(&LATENCY_COLUMNS)
                .copied()
                .collect();
            let mut suffix = format!(
                " ON CONFLICT ({}) DO UPDATE SET ",
                ROLLUP_KEY_COLUMNS.join(", ")
            );
            for (i, c) in ROLLUP_SCALAR_COLUMNS
                .iter()
                .chain(&LATENCY_COLUMNS)
                .enumerate()
            {
                if i > 0 {
                    suffix.push_str(", ");
                }
                let _ = write!(suffix, "{c} = usage_rollup.{c} + excluded.{c}");
            }
            for chunk in rollups.chunks(ROWS_PER_STATEMENT) {
                let sql = insert_sql(self.backend(), "usage_rollup", &cols, chunk.len()) + &suffix;
                let mut q = sqlx::query(AssertSqlSafe(sql));
                for (k, c) in chunk {
                    q = q
                        .bind(k.bucket_start)
                        .bind(k.api_key_id.clone())
                        .bind(k.team_id.clone())
                        .bind(k.organization_id.clone())
                        .bind(k.model_group.clone())
                        .bind(k.deployment_id.clone());
                    for v in c.scalars() {
                        q = q.bind(v);
                    }
                    for v in c.latency_hist {
                        q = q.bind(v);
                    }
                }
                q.execute(&mut *tx).await?;
            }
        }

        tx.commit().await?;
        Ok(())
    }

    /// Löscht Rohlogs mit `at < before_ms` (in Häppchen). Liefert die Anzahl.
    pub async fn purge_request_logs(&self, before_ms: i64) -> Result<u64, StoreError> {
        self.purge_chunked("request_log", before_ms).await
    }

    /// Löscht Payloads mit `at < before_ms` (in Häppchen). Liefert die Anzahl.
    pub async fn purge_request_payloads(&self, before_ms: i64) -> Result<u64, StoreError> {
        self.purge_chunked("request_payload", before_ms).await
    }

    async fn purge_chunked(&self, table: &str, before_ms: i64) -> Result<u64, StoreError> {
        let sql = format!(
            "DELETE FROM {table} WHERE id IN (SELECT id FROM {table} WHERE at < $1 LIMIT $2)"
        );
        let mut total = 0;
        loop {
            let n = sqlx::query(AssertSqlSafe(sql.clone()))
                .bind(before_ms)
                .bind(PURGE_CHUNK)
                .execute(&self.pool)
                .await?
                .rows_affected();
            total += n;
            if n < PURGE_CHUNK.unsigned_abs() {
                return Ok(total);
            }
        }
    }

    /// Löscht Rollups mit `bucket_start < before_ms`.
    pub async fn purge_rollups(&self, before_ms: i64) -> Result<u64, StoreError> {
        Ok(
            sqlx::query("DELETE FROM usage_rollup WHERE bucket_start < $1")
                .bind(before_ms)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }

    /// Rollups mit `from_ms <= bucket_start < to_ms`, sortiert nach Schlüssel.
    pub async fn list_rollups(
        &self,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<Vec<(RollupKey, RollupCounts)>, StoreError> {
        let cols: Vec<&str> = ROLLUP_KEY_COLUMNS
            .iter()
            .chain(&ROLLUP_SCALAR_COLUMNS)
            .chain(&LATENCY_COLUMNS)
            .copied()
            .collect();
        let sql = format!(
            "SELECT {} FROM usage_rollup WHERE bucket_start >= $1 AND bucket_start < $2 \
             ORDER BY {}",
            cols.join(", "),
            ROLLUP_KEY_COLUMNS.join(", ")
        );
        let rows = sqlx::query(AssertSqlSafe(sql))
            .bind(from_ms)
            .bind(to_ms)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(rollup_from_row).collect()
    }

    /// Ein Rohlog-Eintrag.
    pub async fn get_request_log(&self, id: &str) -> Result<Option<RequestLogRow>, StoreError> {
        let sql = format!(
            "SELECT {} FROM request_log WHERE id = $1",
            REQUEST_LOG_COLUMNS.join(", ")
        );
        let row = sqlx::query(AssertSqlSafe(sql))
            .bind(id.to_owned())
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(request_log_from_row).transpose()
    }

    /// Payload zu einer Anfrage.
    pub async fn get_request_payload(&self, id: &str) -> Result<Option<PayloadRow>, StoreError> {
        let row = sqlx::query("SELECT id, at, mode, payload FROM request_payload WHERE id = $1")
            .bind(id.to_owned())
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else { return Ok(None) };
        let text: String = row.try_get("payload")?;
        Ok(Some(PayloadRow {
            id: row.try_get("id")?,
            at: row.try_get("at")?,
            mode: row.try_get("mode")?,
            payload: serde_json::from_str(&text)?,
        }))
    }

    /// Anzahl Rohlogs bzw. Payloads (Tests, Diagnose).
    pub async fn count_request_logs(&self) -> Result<(i64, i64), StoreError> {
        let logs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_log")
            .fetch_one(&self.pool)
            .await?;
        let payloads: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_payload")
            .fetch_one(&self.pool)
            .await?;
        Ok((logs, payloads))
    }
}

fn flag(row: &AnyRow, col: &str) -> Result<bool, StoreError> {
    Ok(row.try_get::<i64, _>(col)? != 0)
}

fn request_log_from_row(row: &AnyRow) -> Result<RequestLogRow, StoreError> {
    Ok(RequestLogRow {
        id: row.try_get("id")?,
        at: row.try_get("at")?,
        endpoint: row.try_get("endpoint")?,
        api_key_id: row.try_get("api_key_id")?,
        team_id: row.try_get("team_id")?,
        organization_id: row.try_get("organization_id")?,
        model_requested: row.try_get("model_requested")?,
        model_group: row.try_get("model_group")?,
        deployment_id: row.try_get("deployment_id")?,
        provider: row.try_get("provider")?,
        provider_model: row.try_get("provider_model")?,
        status: row.try_get("status")?,
        error_code: row.try_get("error_code")?,
        input_tokens: row.try_get("input_tokens")?,
        output_tokens: row.try_get("output_tokens")?,
        cached_tokens: row.try_get("cached_tokens")?,
        cache_write_tokens: row.try_get("cache_write_tokens")?,
        reasoning_tokens: row.try_get("reasoning_tokens")?,
        usage_estimated: flag(row, "usage_estimated")?,
        cost: row.try_get("cost")?,
        latency_ms: row.try_get("latency_ms")?,
        ttft_ms: row.try_get("ttft_ms")?,
        cache: row.try_get("cache")?,
        stream: flag(row, "stream")?,
        client_aborted: flag(row, "client_aborted")?,
        client_ip: row.try_get("client_ip")?,
    })
}

fn rollup_from_row(row: &AnyRow) -> Result<(RollupKey, RollupCounts), StoreError> {
    let key = RollupKey {
        bucket_start: row.try_get("bucket_start")?,
        api_key_id: row.try_get("api_key_id")?,
        team_id: row.try_get("team_id")?,
        organization_id: row.try_get("organization_id")?,
        model_group: row.try_get("model_group")?,
        deployment_id: row.try_get("deployment_id")?,
    };
    let mut hist = [0; LATENCY_BUCKET_COUNT];
    for (slot, col) in hist.iter_mut().zip(LATENCY_COLUMNS) {
        *slot = row.try_get(col)?;
    }
    let counts = RollupCounts {
        requests: row.try_get("requests")?,
        errors: row.try_get("errors")?,
        cache_hits: row.try_get("cache_hits")?,
        input_tokens: row.try_get("input_tokens")?,
        output_tokens: row.try_get("output_tokens")?,
        cached_tokens: row.try_get("cached_tokens")?,
        cache_write_tokens: row.try_get("cache_write_tokens")?,
        reasoning_tokens: row.try_get("reasoning_tokens")?,
        cost: row.try_get("cost")?,
        latency_ms_sum: row.try_get("latency_ms_sum")?,
        estimated: row.try_get("estimated")?,
        aborted: row.try_get("aborted")?,
        ttft_ms_sum: row.try_get("ttft_ms_sum")?,
        ttft_count: row.try_get("ttft_count")?,
        latency_hist: hist,
    };
    Ok((key, counts))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets() {
        assert_eq!(latency_bucket(0), 0);
        assert_eq!(latency_bucket(25), 0);
        assert_eq!(latency_bucket(26), 1);
        assert_eq!(latency_bucket(60_000), 10);
        assert_eq!(latency_bucket(60_001), 11);
        assert_eq!(LATENCY_COLUMNS.len(), LATENCY_BUCKET_COUNT);
    }

    #[test]
    fn insert_sql_placeholders() {
        assert_eq!(
            insert_sql(Backend::Postgres, "t", &["a", "b"], 2),
            "INSERT INTO t (a, b) VALUES ($1, $2), ($3, $4)"
        );
        assert_eq!(
            insert_sql(Backend::Sqlite, "t", &["a", "b"], 2),
            "INSERT INTO t (a, b) VALUES (?, ?), (?, ?)"
        );
    }
}
