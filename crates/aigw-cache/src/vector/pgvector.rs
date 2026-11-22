// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Backend `pgvector`: gemeinsamer semantischer Cache aller Instanzen in Postgres mit der
//! Erweiterung `vector` (HNSW-Index in Postgres, Kosinus-Distanz `<=>`).
//!
//! Das Schema legt das Backend beim ersten Zugriff selbst an (nicht über die portablen
//! Migrationen, weil SQLite und Postgres ohne Erweiterung es nicht ausführen können):
//!
//! ```sql
//! CREATE EXTENSION IF NOT EXISTS vector;            -- braucht ggf. Superuser/Owner-Rechte
//! CREATE TABLE IF NOT EXISTS aigw_semantic_cache_<dim> (
//!   id TEXT PRIMARY KEY, partition_key TEXT NOT NULL, embedding vector(<dim>) NOT NULL,
//!   payload BYTEA NOT NULL, created_at BIGINT NOT NULL, expires_at BIGINT NOT NULL);
//! CREATE INDEX … USING hnsw (embedding vector_cosine_ops) WITH (m = …, ef_construction = …);
//! CREATE INDEX … (partition_key, expires_at);
//! ```
//!
//! Suche in einer Transaktion mit `SET LOCAL hnsw.ef_search` und – ab pgvector 0.8 –
//! `hnsw.iterative_scan = relaxed_order`, damit der Partitionsfilter nach dem HNSW-Scan nicht
//! zu wenige Treffer übrig lässt. Die Verbindung wird verzögert aufgebaut; ist Postgres nicht
//! erreichbar, liefert jede Operation einen Fehler (im Cache: Miss).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::{AssertSqlSafe, Row as _};

use super::{Evict, IndexError, VectorEntry, VectorHit, VectorIndex, VectorQuery, now_ms};
use crate::hnsw::{HnswParams, normalize};
use crate::key::Hash32;

const NAME: &str = "pgvector";

/// Vektor-Backend über Postgres/pgvector.
#[derive(Debug)]
pub struct PgVectorIndex {
    pool: PgPool,
    dim: usize,
    hnsw: HnswParams,
    table: String,
    timeout: Duration,
    ready: AtomicBool,
    iterative_scan: AtomicBool,
    schema_lock: tokio::sync::Mutex<()>,
}

impl PgVectorIndex {
    /// Backend für Vektoren der Länge `dim` (Verbindung wird verzögert aufgebaut).
    pub fn connect_lazy(
        url: &str,
        dim: usize,
        hnsw: HnswParams,
        timeout: Duration,
    ) -> Result<Self, IndexError> {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(timeout.max(Duration::from_millis(500)))
            .connect_lazy(url)
            .map_err(|e| IndexError::backend(NAME, e))?;
        Ok(Self {
            pool,
            dim,
            hnsw,
            table: format!("aigw_semantic_cache_{dim}"),
            timeout,
            ready: AtomicBool::new(false),
            iterative_scan: AtomicBool::new(false),
            schema_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Tabellenname.
    pub fn table(&self) -> &str {
        &self.table
    }

    fn err(e: impl std::fmt::Display) -> IndexError {
        IndexError::backend(NAME, e)
    }

    async fn timed<T>(
        &self,
        fut: impl Future<Output = Result<T, sqlx::Error>>,
    ) -> Result<T, IndexError> {
        tokio::time::timeout(self.timeout, fut)
            .await
            .map_err(|_| Self::err("Zeitüberschreitung"))?
            .map_err(Self::err)
    }

    /// Legt Erweiterung, Tabelle und Indizes an (idempotent).
    pub async fn ensure_schema(&self) -> Result<(), IndexError> {
        if self.ready.load(Ordering::Acquire) {
            return Ok(());
        }
        let _g = self.schema_lock.lock().await;
        if self.ready.load(Ordering::Acquire) {
            return Ok(());
        }
        let t = &self.table;
        let stmts = [
            "CREATE EXTENSION IF NOT EXISTS vector".to_owned(),
            format!(
                "CREATE TABLE IF NOT EXISTS {t} (id TEXT PRIMARY KEY, partition_key TEXT NOT NULL, \
                 embedding vector({}) NOT NULL, payload BYTEA NOT NULL, created_at BIGINT NOT NULL, \
                 expires_at BIGINT NOT NULL)",
                self.dim
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS {t}_hnsw ON {t} USING hnsw (embedding vector_cosine_ops) \
                 WITH (m = {}, ef_construction = {})",
                self.hnsw.m, self.hnsw.ef_construction
            ),
            format!("CREATE INDEX IF NOT EXISTS {t}_part ON {t} (partition_key, expires_at)"),
        ];
        // Schema-Anlage darf länger dauern als ein Hot-Path-Zugriff.
        let slow = self.timeout.max(Duration::from_secs(10));
        for s in stmts {
            tokio::time::timeout(slow, sqlx::query(AssertSqlSafe(s)).execute(&self.pool))
                .await
                .map_err(|_| Self::err("Zeitüberschreitung beim Anlegen des Schemas"))?
                .map_err(Self::err)?;
        }
        let version: Option<String> = tokio::time::timeout(
            slow,
            sqlx::query_scalar("SELECT extversion FROM pg_extension WHERE extname = 'vector'")
                .fetch_optional(&self.pool),
        )
        .await
        .map_err(|_| Self::err("Zeitüberschreitung"))?
        .map_err(Self::err)?;
        let iterative = version.as_deref().is_some_and(supports_iterative_scan);
        self.iterative_scan.store(iterative, Ordering::Relaxed);
        tracing::info!(
            table = %self.table,
            pgvector = version.as_deref().unwrap_or("?"),
            iterative_scan = iterative,
            "semantischer Cache: pgvector bereit"
        );
        self.ready.store(true, Ordering::Release);
        Ok(())
    }

    fn literal(&self, v: &[f32]) -> Result<String, IndexError> {
        if v.len() != self.dim {
            return Err(IndexError::InvalidVector(format!(
                "Dimension {} statt {}",
                v.len(),
                self.dim
            )));
        }
        let mut n = v.to_vec();
        if !normalize(&mut n) {
            return Err(IndexError::InvalidVector("NaN, ∞ oder Nullvektor".into()));
        }
        let mut s = String::with_capacity(n.len() * 12 + 2);
        s.push('[');
        for (i, x) in n.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&x.to_string());
        }
        s.push(']');
        Ok(s)
    }
}

/// pgvector ≥ 0.8 kennt `hnsw.iterative_scan`.
fn supports_iterative_scan(version: &str) -> bool {
    let mut it = version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor) = (it.next().unwrap_or(0), it.next().unwrap_or(0));
    major > 0 || minor >= 8
}

#[async_trait]
impl VectorIndex for PgVectorIndex {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn insert(&self, e: VectorEntry) -> Result<(), IndexError> {
        let lit = self.literal(&e.vector)?;
        self.ensure_schema().await?;
        let sql = format!(
            "INSERT INTO {t} (id, partition_key, embedding, payload, created_at, expires_at) \
             VALUES ($1, $2, $3::vector, $4, $5, $6) ON CONFLICT (id) DO UPDATE SET \
             partition_key = excluded.partition_key, embedding = excluded.embedding, \
             payload = excluded.payload, created_at = excluded.created_at, \
             expires_at = excluded.expires_at",
            t = self.table
        );
        self.timed(
            sqlx::query(AssertSqlSafe(sql))
                .bind(e.id.to_hex())
                .bind(e.partition.to_hex())
                .bind(lit)
                .bind(e.payload.as_ref())
                .bind(now_ms())
                .bind(e.expires_at_ms)
                .execute(&self.pool),
        )
        .await
        .map(drop)
    }

    async fn search(&self, q: VectorQuery<'_>) -> Result<Vec<VectorHit>, IndexError> {
        let lit = self.literal(q.vector)?;
        self.ensure_schema().await?;
        let k = q.k.max(1);
        let ef = self.hnsw.ef_search.max(k);
        let iterative = self.iterative_scan.load(Ordering::Relaxed);
        let sql = format!(
            "SELECT id, 1 - (embedding <=> $1::vector) AS score, payload FROM {t} \
             WHERE partition_key = $2 AND expires_at > $3 \
             ORDER BY embedding <=> $1::vector LIMIT $4",
            t = self.table
        );
        let pool = &self.pool;
        let rows = self
            .timed(async move {
                let mut tx = pool.begin().await?;
                sqlx::query(AssertSqlSafe(format!("SET LOCAL hnsw.ef_search = {ef}")))
                    .execute(&mut *tx)
                    .await?;
                if iterative {
                    sqlx::query("SET LOCAL hnsw.iterative_scan = relaxed_order")
                        .execute(&mut *tx)
                        .await?;
                }
                let rows = sqlx::query(AssertSqlSafe(sql))
                    .bind(lit)
                    .bind(q.partition.to_hex())
                    .bind(now_ms())
                    .bind(i64::try_from(k).unwrap_or(1))
                    .fetch_all(&mut *tx)
                    .await?;
                tx.commit().await?;
                Ok(rows)
            })
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let id: String = row.try_get("id").map_err(Self::err)?;
            let score: f64 = row.try_get("score").map_err(Self::err)?;
            let payload: Vec<u8> = row.try_get("payload").map_err(Self::err)?;
            let score = score as f32;
            if score < q.threshold {
                continue;
            }
            if let Some(id) = Hash32::from_hex(&id) {
                out.push(VectorHit {
                    id,
                    score,
                    payload: Bytes::from(payload),
                });
            }
        }
        Ok(out)
    }

    async fn evict(&self, what: Evict) -> Result<u64, IndexError> {
        self.ensure_schema().await?;
        let t = &self.table;
        let res = match what {
            Evict::Expired => {
                self.timed(
                    sqlx::query(AssertSqlSafe(format!(
                        "DELETE FROM {t} WHERE expires_at <= $1"
                    )))
                    .bind(now_ms())
                    .execute(&self.pool),
                )
                .await?
            }
            Evict::Id(id) => {
                self.timed(
                    sqlx::query(AssertSqlSafe(format!("DELETE FROM {t} WHERE id = $1")))
                        .bind(id.to_hex())
                        .execute(&self.pool),
                )
                .await?
            }
            Evict::Partition(p) => {
                self.timed(
                    sqlx::query(AssertSqlSafe(format!(
                        "DELETE FROM {t} WHERE partition_key = $1"
                    )))
                    .bind(p.to_hex())
                    .execute(&self.pool),
                )
                .await?
            }
        };
        Ok(res.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iterative_scan_version() {
        assert!(supports_iterative_scan("0.8.0"));
        assert!(supports_iterative_scan("0.10.1"));
        assert!(supports_iterative_scan("1.0"));
        assert!(!supports_iterative_scan("0.7.4"));
        assert!(!supports_iterative_scan("x"));
    }
}
