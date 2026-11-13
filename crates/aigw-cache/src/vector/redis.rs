// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Backend `redis`: Redis Stack bzw. Valkey mit Search-Modul (`FT.CREATE … VECTOR HNSW`,
//! `FT.SEARCH … KNN`), gemeinsamer semantischer Cache aller Instanzen.
//!
//! - Index `<prefix>vidx:<dim>` über Hashes `<prefix>v:<dim>:<id>` mit den Feldern
//!   `p` (TAG, Partition hex), `v` (VECTOR HNSW, FLOAT32, COSINE) und `d` (Antwort).
//! - TTL nativ über `PEXPIREAT` (abgelaufene Einträge verschwinden selbst; [`Evict::Expired`]
//!   ist ein No-op).
//! - Suche: `(@p:{<hex>})=>[KNN k @v $B EF_RUNTIME ef AS score]`; `score` ist die
//!   Kosinus-**Distanz** (1 − Ähnlichkeit).
//! - Der Index wird beim ersten Zugriff angelegt (`Index already exists` wird ignoriert).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use redis::Value;

use super::{Evict, IndexError, VectorEntry, VectorHit, VectorIndex, VectorQuery};
use crate::hnsw::{HnswParams, normalize};
use crate::key::Hash32;
use crate::redis_conn::RedisConn;

const NAME: &str = "redis";

/// Vektor-Backend über Redis Stack / Valkey Search.
#[derive(Debug)]
pub struct RedisVectorIndex {
    conn: Arc<RedisConn>,
    dim: usize,
    hnsw: HnswParams,
    index: String,
    key_prefix: String,
    ready: AtomicBool,
}

impl RedisVectorIndex {
    /// Backend über `conn` für Vektoren der Länge `dim`; `prefix` z. B. `aigw:cache:`.
    pub fn new(conn: Arc<RedisConn>, dim: usize, hnsw: HnswParams, prefix: &str) -> Self {
        Self {
            conn,
            dim,
            hnsw,
            index: format!("{prefix}vidx:{dim}"),
            key_prefix: format!("{prefix}v:{dim}:"),
            ready: AtomicBool::new(false),
        }
    }

    fn err(e: impl std::fmt::Display) -> IndexError {
        IndexError::backend(NAME, e)
    }

    fn key(&self, id: &Hash32) -> String {
        format!("{}{}", self.key_prefix, id.to_hex())
    }

    async fn ensure_index(&self) -> Result<(), IndexError> {
        if self.ready.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut cmd = redis::cmd("FT.CREATE");
        cmd.arg(&self.index)
            .arg("ON")
            .arg("HASH")
            .arg("PREFIX")
            .arg(1)
            .arg(&self.key_prefix)
            .arg("SCHEMA")
            .arg("p")
            .arg("TAG")
            .arg("v")
            .arg("VECTOR")
            .arg("HNSW")
            .arg(10)
            .arg("TYPE")
            .arg("FLOAT32")
            .arg("DIM")
            .arg(self.dim)
            .arg("DISTANCE_METRIC")
            .arg("COSINE")
            .arg("M")
            .arg(self.hnsw.m)
            .arg("EF_CONSTRUCTION")
            .arg(self.hnsw.ef_construction);
        match self.conn.query::<Value>(&cmd).await {
            Ok(_) => {}
            Err(e) if e.to_ascii_lowercase().contains("already exists") => {}
            Err(e) => return Err(Self::err(e)),
        }
        self.ready.store(true, Ordering::Release);
        Ok(())
    }

    fn blob(&self, v: &[f32]) -> Result<Vec<u8>, IndexError> {
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
        Ok(super::vector_to_le_bytes(&n))
    }

    async fn search_keys(&self, query: &str, limit: usize) -> Result<Vec<String>, IndexError> {
        let mut cmd = redis::cmd("FT.SEARCH");
        cmd.arg(&self.index)
            .arg(query)
            .arg("NOCONTENT")
            .arg("LIMIT")
            .arg(0)
            .arg(limit)
            .arg("DIALECT")
            .arg(2);
        let v: Value = self.conn.query(&cmd).await.map_err(Self::err)?;
        let Value::Array(items) = v else {
            return Ok(Vec::new());
        };
        Ok(items.iter().skip(1).filter_map(text).collect())
    }
}

fn text(v: &Value) -> Option<String> {
    match v {
        Value::BulkString(b) => String::from_utf8(b.clone()).ok(),
        Value::SimpleString(s) => Some(s.clone()),
        _ => None,
    }
}

fn bytes(v: &Value) -> Option<Bytes> {
    match v {
        Value::BulkString(b) => Some(Bytes::copy_from_slice(b)),
        Value::SimpleString(s) => Some(Bytes::copy_from_slice(s.as_bytes())),
        _ => None,
    }
}

/// Felder eines Treffers (RESP2: flache Liste Name, Wert, …; RESP3: Map).
fn fields(v: &Value) -> Vec<(String, &Value)> {
    match v {
        Value::Array(a) => a
            .as_chunks::<2>()
            .0
            .iter()
            .filter_map(|[k, v]| Some((text(k)?, v)))
            .collect(),
        Value::Map(m) => m.iter().filter_map(|(k, v)| Some((text(k)?, v))).collect(),
        _ => Vec::new(),
    }
}

#[async_trait]
impl VectorIndex for RedisVectorIndex {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn insert(&self, e: VectorEntry) -> Result<(), IndexError> {
        let blob = self.blob(&e.vector)?;
        self.ensure_index().await?;
        let key = self.key(&e.id);
        let mut pipe = redis::pipe();
        pipe.atomic()
            .cmd("HSET")
            .arg(&key)
            .arg("p")
            .arg(e.partition.to_hex())
            .arg("v")
            .arg(blob)
            .arg("d")
            .arg(e.payload.as_ref())
            .ignore()
            .cmd("PEXPIREAT")
            .arg(&key)
            .arg(e.expires_at_ms)
            .ignore();
        self.conn.pipe::<()>(&pipe).await.map_err(Self::err)
    }

    async fn search(&self, q: VectorQuery<'_>) -> Result<Vec<VectorHit>, IndexError> {
        let blob = self.blob(q.vector)?;
        self.ensure_index().await?;
        let k = q.k.max(1);
        let query = format!(
            "(@p:{{{}}})=>[KNN {k} @v $B EF_RUNTIME {} AS score]",
            q.partition.to_hex(),
            self.hnsw.ef_search.max(k)
        );
        let mut cmd = redis::cmd("FT.SEARCH");
        cmd.arg(&self.index)
            .arg(query)
            .arg("PARAMS")
            .arg(2)
            .arg("B")
            .arg(blob)
            .arg("SORTBY")
            .arg("score")
            .arg("RETURN")
            .arg(2)
            .arg("score")
            .arg("d")
            .arg("LIMIT")
            .arg(0)
            .arg(k)
            .arg("DIALECT")
            .arg(2);
        let v: Value = self.conn.query(&cmd).await.map_err(Self::err)?;
        let Value::Array(items) = v else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        // [total, key1, fields1, key2, fields2, …]
        for [key, fields_value] in items.get(1..).unwrap_or_default().as_chunks::<2>().0 {
            let Some(key) = text(key) else { continue };
            let Some(id) = key
                .strip_prefix(&self.key_prefix)
                .and_then(Hash32::from_hex)
            else {
                continue;
            };
            let mut score = None;
            let mut payload = None;
            for (name, val) in fields(fields_value) {
                match name.as_str() {
                    "score" => {
                        score = text(val)
                            .and_then(|s| s.parse::<f32>().ok())
                            .map(|d| 1.0 - d);
                    }
                    "d" => payload = bytes(val),
                    _ => {}
                }
            }
            if let (Some(score), Some(payload)) = (score, payload)
                && score >= q.threshold
            {
                out.push(VectorHit { id, score, payload });
            }
        }
        Ok(out)
    }

    async fn evict(&self, what: Evict) -> Result<u64, IndexError> {
        match what {
            // TTL nativ in Redis.
            Evict::Expired => Ok(0),
            Evict::Id(id) => {
                let mut cmd = redis::cmd("DEL");
                cmd.arg(self.key(&id));
                let n: u64 = self.conn.query(&cmd).await.map_err(Self::err)?;
                Ok(n)
            }
            Evict::Partition(p) => {
                self.ensure_index().await?;
                let mut total = 0;
                loop {
                    let keys = self
                        .search_keys(&format!("@p:{{{}}}", p.to_hex()), 1000)
                        .await?;
                    if keys.is_empty() {
                        break;
                    }
                    let mut cmd = redis::cmd("DEL");
                    cmd.arg(&keys);
                    let n: u64 = self.conn.query(&cmd).await.map_err(Self::err)?;
                    total += n;
                    if n == 0 {
                        break;
                    }
                }
                Ok(total)
            }
        }
    }
}
