// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Request-Log, Payloads und Minuten-Rollups (AIGW-43).
//!
//! Geschrieben wird ausschließlich gebündelt vom Writer in `aigw-observe`: ein Batch = eine
//! Transaktion mit mehrzeiligen `INSERT`s (Rohlog, Payload) und additiven `UPSERT`s
//! (`ON CONFLICT … DO UPDATE SET x = usage_rollup.x + excluded.x`) für die Rollups. Das läuft
//! auf SQLite und Postgres und verträgt mehrere Instanzen ohne Read-Modify-Write. Jeder Batch
//! trägt eine ID (`observe_batch`, F34): Wiederholungen nach unklarem Commit-Ausgang wenden ihn
//! nicht doppelt an.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use aigw_core::resource::{Id, new_id};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::db::{DbRow, Q, Tx, Val};
use crate::{Backend, Store, StoreError};

/// Obergrenzen (inklusive, Millisekunden) der Latenz-Buckets; danach folgt `+Inf`.
pub const LATENCY_BUCKETS_MS: [i64; 11] = [
    25, 50, 100, 250, 500, 1000, 2500, 5000, 10_000, 30_000, 60_000,
];

/// Anzahl der Latenz-Buckets inkl. `+Inf`.
pub const LATENCY_BUCKET_COUNT: usize = LATENCY_BUCKETS_MS.len() + 1;

/// Spalten der Latenz-Buckets (in der Reihenfolge von [`LATENCY_BUCKETS_MS`], dann `+Inf`).
pub(crate) const LATENCY_COLUMNS: [&str; LATENCY_BUCKET_COUNT] = [
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

/// Spalten des TTFT-Histogramms (gleiche Grenzen wie [`LATENCY_BUCKETS_MS`], AIGW-46).
pub(crate) const TTFT_COLUMNS: [&str; LATENCY_BUCKET_COUNT] = [
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

/// Länge eines Minuten-Buckets (ms), Tabelle `usage_rollup`.
pub const MINUTE_MS: i64 = 60_000;
/// Länge eines Stunden-Buckets (ms), Tabelle `usage_rollup_hour` (AIGW-46).
pub const HOUR_MS: i64 = 3_600_000;

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
    /// Request-ID des Clients (`x-request-id` / `x-client-request-id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_request_id: Option<String>,
    /// Upstream-Versuche (Retries, Fallbacks) in Reihenfolge; leer = keine aufgezeichnet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<AttemptRow>,
    /// Auslösende Anfrage (F191, z. B. das Embedding des semantischen Caches gehört zur
    /// Chat-Anfrage, die es ausgelöst hat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_request_id: Option<String>,
    /// Feine Fehlerklasse (F47, `snake_case` wie `ErrorClass`: `timeout`, `rate_limit` …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_class: Option<String>,
    /// Trace-ID (32 Hex-Zeichen, F100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Erkennungen der Guardrail-Plugins (F161; Zähler, nie Inhalte).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub guardrails: Vec<GuardrailRow>,
    /// Workload-/Agent-Identität bei Anmeldung mit Access-Token (AIGW-115): Aussteller,
    /// Kennung, Art, ggf. Nutzer und Blueprint – nie das Token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload: Option<aigw_core::workload::WorkloadPrincipal>,
}

/// Erkennungen eines Guardrail-Plugins in einer Anfrage (Spalte `request_log.guardrails`,
/// JSON-Array; F161).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GuardrailRow {
    /// Plugin (`pii`, `prompt-filter`, `content-safety` …).
    pub plugin: String,
    /// Kategorie (z. B. `email`, `injection`).
    pub category: String,
    /// Aktion (`mask`, `redact`, `reject`, `pseudonymize`, `log-only`, `flag`).
    pub action: String,
    /// Anzahl der Erkennungen.
    pub count: u32,
}

/// Ein Upstream-Versuch einer Anfrage (Spalte `request_log.attempts`, JSON-Array; AIGW-46).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AttemptRow {
    /// Modellgruppe des Deployments (bei Fallbacks die Ziel-Gruppe).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_group: Option<String>,
    /// Name des Deployments.
    pub deployment: String,
    /// ID des Deployments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<String>,
    /// Fehlerklasse (`snake_case` wie `ErrorClass`); fehlt = erfolgreich.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// HTTP-Status des Anbieters bei Fehlern.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_status: Option<u16>,
    /// Dauer bis zur Antwort, zum ersten Stream-Ereignis bzw. zum Fehler (ms).
    pub duration_ms: i64,
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
    /// Fehlerklasse (F47) oder `''` (kein Fehler).
    #[serde(default)]
    pub error_class: String,
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
    /// Anzahl je TTFT-Bucket (nicht kumulativ, nur Anfragen mit TTFT), gleiche Grenzen.
    #[serde(default)]
    pub ttft_hist: [i64; LATENCY_BUCKET_COUNT],
    /// Kosten der Eingabe-Tokens ohne Anbieter-Cache (Nano, AIGW-117).
    #[serde(default)]
    pub cost_input: i64,
    /// Kosten der aus dem Anbieter-Cache gelesenen Tokens (Nano).
    #[serde(default)]
    pub cost_cache_read: i64,
    /// Kosten der in den Anbieter-Cache geschriebenen Tokens (Nano).
    #[serde(default)]
    pub cost_cache_write: i64,
    /// Kosten der Ausgabe-Tokens inkl. Reasoning (Nano).
    #[serde(default)]
    pub cost_output: i64,
    /// Ersparnis durch Prompt-Caching des Anbieters (Nano).
    #[serde(default)]
    pub cache_read_saved: i64,
    /// Kosten der Plugin-Modellaufrufe (Nano; in `cost` enthalten).
    #[serde(default)]
    pub plugin_cost: i64,
    /// Eingabe-Tokens der Plugin-Modellaufrufe (nicht in `input_tokens`).
    #[serde(default)]
    pub plugin_input_tokens: i64,
    /// Ausgabe-Tokens der Plugin-Modellaufrufe (nicht in `output_tokens`).
    #[serde(default)]
    pub plugin_output_tokens: i64,
}

impl RollupCounts {
    /// Werte ohne Histogramm, in Spaltenreihenfolge von [`ROLLUP_SCALAR_COLUMNS`].
    fn scalars(&self) -> [i64; 22] {
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
            self.cost_input,
            self.cost_cache_read,
            self.cost_cache_write,
            self.cost_output,
            self.cache_read_saved,
            self.plugin_cost,
            self.plugin_input_tokens,
            self.plugin_output_tokens,
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
        add(&mut self.cost_input, other.cost_input);
        add(&mut self.cost_cache_read, other.cost_cache_read);
        add(&mut self.cost_cache_write, other.cost_cache_write);
        add(&mut self.cost_output, other.cost_output);
        add(&mut self.cache_read_saved, other.cache_read_saved);
        add(&mut self.plugin_cost, other.plugin_cost);
        add(&mut self.plugin_input_tokens, other.plugin_input_tokens);
        add(&mut self.plugin_output_tokens, other.plugin_output_tokens);
        for (a, b) in self.latency_hist.iter_mut().zip(other.latency_hist) {
            add(a, b);
        }
        for (a, b) in self.ttft_hist.iter_mut().zip(other.ttft_hist) {
            add(a, b);
        }
    }
}

pub(crate) const REQUEST_LOG_COLUMNS: [&str; 34] = [
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
    "attempts",
    "client_request_id",
    "parent_request_id",
    "error_class",
    "trace_id",
    "guardrails",
    "workload",
];

pub(crate) const ROLLUP_KEY_COLUMNS: [&str; 7] = [
    "bucket_start",
    "api_key_id",
    "team_id",
    "organization_id",
    "model_group",
    "deployment_id",
    "error_class",
];

pub(crate) const ROLLUP_SCALAR_COLUMNS: [&str; 22] = [
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
    "cost_input",
    "cost_cache_read",
    "cost_cache_write",
    "cost_output",
    "cache_read_saved",
    "plugin_cost",
    "plugin_input_tokens",
    "plugin_output_tokens",
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

impl RequestLogRow {
    /// Werte in Spaltenreihenfolge von [`REQUEST_LOG_COLUMNS`].
    fn vals(&self) -> Result<[Val; 34], StoreError> {
        let id = |s: Option<&str>, what: &str| Val::uuid_text(s, what);
        Ok([
            id(Some(&self.id), "request_log.id")?,
            Val::ms(self.at),
            Val::text(self.endpoint.clone()),
            id(self.api_key_id.as_deref(), "request_log.api_key_id")?,
            id(self.team_id.as_deref(), "request_log.team_id")?,
            id(
                self.organization_id.as_deref(),
                "request_log.organization_id",
            )?,
            Val::Text(self.model_requested.clone()),
            Val::Text(self.model_group.clone()),
            id(self.deployment_id.as_deref(), "request_log.deployment_id")?,
            Val::Text(self.provider.clone()),
            Val::Text(self.provider_model.clone()),
            Val::int(self.status),
            Val::Text(self.error_code.clone()),
            Val::int(self.input_tokens),
            Val::int(self.output_tokens),
            Val::int(self.cached_tokens),
            Val::int(self.cache_write_tokens),
            Val::int(self.reasoning_tokens),
            Val::bool(self.usage_estimated),
            Val::int(self.cost),
            Val::int(self.latency_ms),
            Val::Int(self.ttft_ms),
            Val::Text(self.cache.clone()),
            Val::bool(self.stream),
            Val::bool(self.client_aborted),
            Val::Text(self.client_ip.clone()),
            Val::Json(Some(serde_json::Value::Object(serde_json::Map::new()))),
            attempts_json(&self.attempts)?,
            Val::Text(self.client_request_id.clone()),
            id(
                self.parent_request_id.as_deref(),
                "request_log.parent_request_id",
            )?,
            Val::Text(self.error_class.clone()),
            Val::Text(self.trace_id.clone()),
            if self.guardrails.is_empty() {
                Val::Json(None)
            } else {
                Val::Json(Some(serde_json::to_value(&self.guardrails)?))
            },
            Val::Json(
                self.workload
                    .as_ref()
                    .map(serde_json::to_value)
                    .transpose()?,
            ),
        ])
    }
}

impl Store {
    /// Schreibt einen Batch in **einer** Transaktion: Rohlogs (doppelte IDs werden
    /// ignoriert), Payloads und additive Rollup-Deltas. Jeder Aufruf ist ein neuer Batch –
    /// für Wiederholungen [`Store::write_request_batch_once`] verwenden.
    ///
    /// `rollups` muss je Schlüssel höchstens einen Eintrag enthalten (Postgres lehnt
    /// doppelte Schlüssel in einem `UPSERT` ab).
    pub async fn write_request_batch(
        &self,
        logs: &[&RequestLogRow],
        payloads: &[&PayloadRow],
        rollups: &[(RollupKey, RollupCounts)],
    ) -> Result<(), StoreError> {
        self.write_request_batch_once(new_id(), logs, payloads, rollups)
            .await
            .map(drop)
    }

    /// Wie [`Store::write_request_batch`], aber **idempotent** je `batch_id` (F34): Die ID
    /// wird in derselben Transaktion in `observe_batch` eingetragen; ist sie schon vorhanden
    /// (Wiederholung nach unklarem Commit-Ausgang), wird nichts geschrieben und `false`
    /// geliefert. So zählen Rollups auch bei Wiederholungen exakt einmal.
    pub async fn write_request_batch_once(
        &self,
        batch_id: Id,
        logs: &[&RequestLogRow],
        payloads: &[&PayloadRow],
        rollups: &[(RollupKey, RollupCounts)],
    ) -> Result<bool, StoreError> {
        if logs.is_empty() && payloads.is_empty() && rollups.is_empty() {
            return Ok(true);
        }
        // Werte vorab umwandeln: ungültige IDs scheitern vor dem Öffnen der Transaktion.
        let log_vals = logs
            .iter()
            .map(|r| r.vals())
            .collect::<Result<Vec<_>, _>>()?;
        let backend = self.backend();
        let mut tx = self.pool.begin().await?;

        let fresh = Q::new(
            "INSERT INTO observe_batch (batch_id, applied_at) VALUES ($1, $2) \
             ON CONFLICT (batch_id) DO NOTHING",
        )
        .bind(Val::uuid(batch_id))
        .bind(Val::time(OffsetDateTime::now_utc()))
        .execute(&mut tx)
        .await?;
        if fresh == 0 {
            // Bereits angewendet; Transaktion verwerfen (Drop = Rollback).
            return Ok(false);
        }

        for chunk in log_vals.chunks(ROWS_PER_STATEMENT) {
            let mut q = Q::new(
                insert_sql(backend, "request_log", &REQUEST_LOG_COLUMNS, chunk.len())
                    + " ON CONFLICT (id) DO NOTHING",
            );
            for vals in chunk {
                q.args.extend(vals.iter().cloned());
            }
            q.execute(&mut tx).await?;
        }

        for chunk in payloads.chunks(ROWS_PER_STATEMENT) {
            let mut q = Q::new(
                insert_sql(
                    backend,
                    "request_payload",
                    &["id", "at", "mode", "payload"],
                    chunk.len(),
                ) + " ON CONFLICT (id) DO NOTHING",
            );
            for p in chunk {
                q.args
                    .push(Val::uuid_text(Some(&p.id), "request_payload.id")?);
                q.args.push(Val::ms(p.at));
                q.args.push(Val::text(p.mode.clone()));
                q.args.push(Val::Json(Some(p.payload.clone())));
            }
            q.execute(&mut tx).await?;
        }

        if !rollups.is_empty() {
            upsert_rollups(&mut tx, backend, "usage_rollup", rollups).await?;
            let hourly = to_hourly(rollups);
            upsert_rollups(&mut tx, backend, "usage_rollup_hour", &hourly).await?;
        }

        tx.commit().await?;
        Ok(true)
    }

    /// Löscht Einträge in `observe_batch` mit `applied_at < before_ms` (F34). Liefert die
    /// Anzahl.
    pub async fn purge_observe_batches(&self, before_ms: i64) -> Result<u64, StoreError> {
        Ok(Q::new("DELETE FROM observe_batch WHERE applied_at < $1")
            .bind(Val::ms(before_ms))
            .execute(&self.pool)
            .await?)
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
            let n = Q::new(sql.clone())
                .bind(Val::ms(before_ms))
                .bind(Val::int(PURGE_CHUNK))
                .execute(&self.pool)
                .await?;
            total += n;
            if n < PURGE_CHUNK.unsigned_abs() {
                return Ok(total);
            }
        }
    }

    /// Löscht Minuten-Rollups mit `bucket_start < before_ms` und Stunden-Rollups, deren
    /// Stunde vollständig davor liegt. Liefert die Anzahl der Minuten-Rollups.
    pub async fn purge_rollups(&self, before_ms: i64) -> Result<u64, StoreError> {
        let n = Q::new("DELETE FROM usage_rollup WHERE bucket_start < $1")
            .bind(Val::ms(before_ms))
            .execute(&self.pool)
            .await?;
        Q::new("DELETE FROM usage_rollup_hour WHERE bucket_start < $1")
            .bind(Val::ms(before_ms.div_euclid(HOUR_MS) * HOUR_MS))
            .execute(&self.pool)
            .await?;
        Ok(n)
    }

    /// Rollups mit `from_ms <= bucket_start < to_ms`, sortiert nach Schlüssel.
    pub async fn list_rollups(
        &self,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<Vec<(RollupKey, RollupCounts)>, StoreError> {
        self.list_rollups_in("usage_rollup", from_ms, to_ms).await
    }

    /// Stunden-Rollups mit `from_ms <= bucket_start < to_ms`, sortiert nach Schlüssel.
    pub async fn list_hourly_rollups(
        &self,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<Vec<(RollupKey, RollupCounts)>, StoreError> {
        self.list_rollups_in("usage_rollup_hour", from_ms, to_ms)
            .await
    }

    async fn list_rollups_in(
        &self,
        table: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<Vec<(RollupKey, RollupCounts)>, StoreError> {
        let cols: Vec<&str> = ROLLUP_KEY_COLUMNS
            .iter()
            .chain(rollup_count_columns())
            .copied()
            .collect();
        let sql = format!(
            "SELECT {} FROM {table} WHERE bucket_start >= $1 AND bucket_start < $2 \
             ORDER BY {}",
            cols.join(", "),
            ROLLUP_KEY_COLUMNS.join(", ")
        );
        let rows = Q::new(sql)
            .bind(Val::ms(from_ms))
            .bind(Val::ms(to_ms))
            .fetch_all(&self.pool)
            .await?;
        let mut out = rows
            .iter()
            .map(rollup_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        // Sortierung wie der Schlüssel (Text-IDs): Postgres sortiert UUIDs als Bytes.
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Ein Rohlog-Eintrag (`None` auch bei einer ID, die keine UUID ist).
    pub async fn get_request_log(&self, id: &str) -> Result<Option<RequestLogRow>, StoreError> {
        let Ok(id) = Id::parse_str(id) else {
            return Ok(None);
        };
        let sql = format!(
            "SELECT {} FROM request_log WHERE id = $1",
            REQUEST_LOG_COLUMNS.join(", ")
        );
        let row = Q::new(sql)
            .bind(Val::uuid(id))
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(request_log_from_row).transpose()
    }

    /// Payload zu einer Anfrage.
    pub async fn get_request_payload(&self, id: &str) -> Result<Option<PayloadRow>, StoreError> {
        let Ok(id) = Id::parse_str(id) else {
            return Ok(None);
        };
        let row = Q::new("SELECT id, at, mode, payload FROM request_payload WHERE id = $1")
            .bind(Val::uuid(id))
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else { return Ok(None) };
        Ok(Some(PayloadRow {
            id: row.uuid("id")?.hyphenated().to_string(),
            at: row.ms("at")?,
            mode: row.text("mode")?,
            payload: row
                .opt_json_value("payload")?
                .unwrap_or(serde_json::Value::Null),
        }))
    }

    /// Anzahl Rohlogs bzw. Payloads (Tests, Diagnose).
    pub async fn count_request_logs(&self) -> Result<(i64, i64), StoreError> {
        let logs = Q::new("SELECT COUNT(*) FROM request_log")
            .fetch_i64(&self.pool)
            .await?;
        let payloads = Q::new("SELECT COUNT(*) FROM request_payload")
            .fetch_i64(&self.pool)
            .await?;
        Ok((logs, payloads))
    }
}

/// Zählspalten eines Rollups in Bindungsreihenfolge: Skalare, Latenz-, TTFT-Histogramm.
pub(crate) fn rollup_count_columns() -> impl Iterator<Item = &'static &'static str> {
    ROLLUP_SCALAR_COLUMNS
        .iter()
        .chain(&LATENCY_COLUMNS)
        .chain(&TTFT_COLUMNS)
}

/// Fasst Minuten-Rollups zu Stunden-Rollups zusammen (je Schlüssel ein Eintrag, sortiert).
fn to_hourly(rollups: &[(RollupKey, RollupCounts)]) -> Vec<(RollupKey, RollupCounts)> {
    let mut map: BTreeMap<RollupKey, RollupCounts> = BTreeMap::new();
    for (k, c) in rollups {
        let mut key = k.clone();
        key.bucket_start = k.bucket_start.div_euclid(HOUR_MS) * HOUR_MS;
        map.entry(key).or_default().merge(c);
    }
    map.into_iter().collect()
}

/// Additiver UPSERT von Rollup-Deltas in `table` (`usage_rollup` oder `usage_rollup_hour`).
async fn upsert_rollups(
    tx: &mut Tx,
    backend: Backend,
    table: &str,
    rollups: &[(RollupKey, RollupCounts)],
) -> Result<(), StoreError> {
    let cols: Vec<&str> = ROLLUP_KEY_COLUMNS
        .iter()
        .chain(rollup_count_columns())
        .copied()
        .collect();
    let mut suffix = format!(
        " ON CONFLICT ({}) DO UPDATE SET ",
        ROLLUP_KEY_COLUMNS.join(", ")
    );
    for (i, c) in rollup_count_columns().enumerate() {
        if i > 0 {
            suffix.push_str(", ");
        }
        let _ = write!(suffix, "{c} = {table}.{c} + excluded.{c}");
    }
    for chunk in rollups.chunks(ROWS_PER_STATEMENT) {
        let mut q = Q::new(insert_sql(backend, table, &cols, chunk.len()) + &suffix);
        q.args.reserve(chunk.len() * cols.len());
        for (k, c) in chunk {
            q.args.push(Val::ms(k.bucket_start));
            q.args.push(Val::dim_text(&k.api_key_id, "api_key_id")?);
            q.args.push(Val::dim_text(&k.team_id, "team_id")?);
            q.args
                .push(Val::dim_text(&k.organization_id, "organization_id")?);
            q.args.push(Val::text(k.model_group.clone()));
            q.args
                .push(Val::dim_text(&k.deployment_id, "deployment_id")?);
            q.args.push(Val::text(k.error_class.clone()));
            q.args.extend(
                c.scalars()
                    .into_iter()
                    .chain(c.latency_hist)
                    .chain(c.ttft_hist)
                    .map(Val::int),
            );
        }
        q.execute(&mut *tx).await?;
    }
    Ok(())
}

fn attempts_json(a: &[AttemptRow]) -> Result<Val, StoreError> {
    if a.is_empty() {
        Ok(Val::Json(None))
    } else {
        Ok(Val::Json(Some(serde_json::to_value(a)?)))
    }
}

pub(crate) fn request_log_from_row(row: &DbRow) -> Result<RequestLogRow, StoreError> {
    Ok(RequestLogRow {
        id: row.uuid("id")?.hyphenated().to_string(),
        at: row.ms("at")?,
        endpoint: row.text("endpoint")?,
        api_key_id: row.opt_uuid_text("api_key_id")?,
        team_id: row.opt_uuid_text("team_id")?,
        organization_id: row.opt_uuid_text("organization_id")?,
        model_requested: row.opt_text("model_requested")?,
        model_group: row.opt_text("model_group")?,
        deployment_id: row.opt_uuid_text("deployment_id")?,
        provider: row.opt_text("provider")?,
        provider_model: row.opt_text("provider_model")?,
        status: row.i64("status")?,
        error_code: row.opt_text("error_code")?,
        input_tokens: row.i64("input_tokens")?,
        output_tokens: row.i64("output_tokens")?,
        cached_tokens: row.i64("cached_tokens")?,
        cache_write_tokens: row.i64("cache_write_tokens")?,
        reasoning_tokens: row.i64("reasoning_tokens")?,
        usage_estimated: row.bool("usage_estimated")?,
        cost: row.i64("cost")?,
        latency_ms: row.i64("latency_ms")?,
        ttft_ms: row.opt_i64("ttft_ms")?,
        cache: row.opt_text("cache")?,
        stream: row.bool("stream")?,
        client_aborted: row.bool("client_aborted")?,
        client_ip: row.opt_text("client_ip")?,
        client_request_id: row.opt_text("client_request_id")?,
        parent_request_id: row.opt_uuid_text("parent_request_id")?,
        attempts: match row.opt_json_value("attempts")? {
            Some(v) if !v.is_null() => serde_json::from_value(v)?,
            _ => Vec::new(),
        },
        error_class: row.opt_text("error_class")?,
        trace_id: row.opt_text("trace_id")?,
        guardrails: match row.opt_json_value("guardrails")? {
            Some(v) if !v.is_null() => serde_json::from_value(v)?,
            _ => Vec::new(),
        },
        workload: match row.opt_json_value("workload")? {
            Some(v) if !v.is_null() => serde_json::from_value(v)?,
            _ => None,
        },
    })
}

fn rollup_from_row(row: &DbRow) -> Result<(RollupKey, RollupCounts), StoreError> {
    let key = RollupKey {
        bucket_start: row.ms("bucket_start")?,
        api_key_id: row.dim("api_key_id")?,
        team_id: row.dim("team_id")?,
        organization_id: row.dim("organization_id")?,
        model_group: row.dim("model_group")?,
        deployment_id: row.dim("deployment_id")?,
        error_class: row.dim("error_class")?,
    };
    Ok((key, rollup_counts_from_row(row)?))
}

/// Zählspalten (Namen wie in der Tabelle) aus einer Ergebniszeile.
pub(crate) fn rollup_counts_from_row(row: &DbRow) -> Result<RollupCounts, StoreError> {
    let mut hist = [0; LATENCY_BUCKET_COUNT];
    for (slot, col) in hist.iter_mut().zip(LATENCY_COLUMNS) {
        *slot = row.i64(col)?;
    }
    let mut ttft_hist = [0; LATENCY_BUCKET_COUNT];
    for (slot, col) in ttft_hist.iter_mut().zip(TTFT_COLUMNS) {
        *slot = row.i64(col)?;
    }
    Ok(RollupCounts {
        requests: row.i64("requests")?,
        errors: row.i64("errors")?,
        cache_hits: row.i64("cache_hits")?,
        input_tokens: row.i64("input_tokens")?,
        output_tokens: row.i64("output_tokens")?,
        cached_tokens: row.i64("cached_tokens")?,
        cache_write_tokens: row.i64("cache_write_tokens")?,
        reasoning_tokens: row.i64("reasoning_tokens")?,
        cost: row.i64("cost")?,
        latency_ms_sum: row.i64("latency_ms_sum")?,
        estimated: row.i64("estimated")?,
        aborted: row.i64("aborted")?,
        ttft_ms_sum: row.i64("ttft_ms_sum")?,
        ttft_count: row.i64("ttft_count")?,
        latency_hist: hist,
        ttft_hist,
        cost_input: row.i64("cost_input")?,
        cost_cache_read: row.i64("cost_cache_read")?,
        cost_cache_write: row.i64("cost_cache_write")?,
        cost_output: row.i64("cost_output")?,
        cache_read_saved: row.i64("cache_read_saved")?,
        plugin_cost: row.i64("plugin_cost")?,
        plugin_input_tokens: row.i64("plugin_input_tokens")?,
        plugin_output_tokens: row.i64("plugin_output_tokens")?,
    })
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
