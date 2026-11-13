// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Vektor-Backends des semantischen Caches (Architektur §10, Entscheidung 2026-11-13).
//!
//! [`VectorIndex`] (analog zum Zähler-Trait): [`VectorIndex::insert`],
//! [`VectorIndex::search`] (k nächste mit Mindest-Ähnlichkeit) und [`VectorIndex::evict`].
//! Einträge gehören zu einer **Partition** (Scope, Modellgruppe, Systemprompt/Tools …, siehe
//! [`crate::key::partition`]); gesucht wird nur innerhalb der Partition (harter Filter). Die
//! ID eines Eintrags ist der exakte Cache-Schlüssel – erneutes Einfügen ersetzt.
//!
//! - [`memory::MemoryIndex`]: eigener HNSW ([`crate::hnsw`]) je Partition im RAM,
//!   TTL, Speichergrenze, Grabsteine + Neuaufbau, optional Warmstart aus der Datenbank.
//! - [`pgvector::PgVectorIndex`]: Postgres mit Erweiterung `vector`, HNSW-Index in Postgres.
//! - [`redis::RedisVectorIndex`]: Redis Stack / Valkey Search, `FT.SEARCH` mit KNN.
//!
//! Fehler externer Backends führen im Cache zu einem Miss (keine Fehlerweitergabe).

pub mod memory;
pub mod pgvector;
pub mod redis;

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;

use crate::key::Hash32;

/// Ein Eintrag.
#[derive(Debug, Clone)]
pub struct VectorEntry {
    /// ID (= exakter Cache-Schlüssel).
    pub id: Hash32,
    /// Partition (harter Filter).
    pub partition: Hash32,
    /// Embedding (beliebige Länge; Backends normieren selbst bzw. rechnen Kosinus).
    pub vector: Vec<f32>,
    /// Gecachte Antwort (serialisierte IR).
    pub payload: Bytes,
    /// Ablauf (Unix-Millisekunden).
    pub expires_at_ms: i64,
}

/// Suchanfrage.
#[derive(Debug, Clone, Copy)]
pub struct VectorQuery<'a> {
    /// Partition.
    pub partition: Hash32,
    /// Anfragevektor.
    pub vector: &'a [f32],
    /// Höchstzahl Treffer.
    pub k: usize,
    /// Mindest-Kosinus-Ähnlichkeit.
    pub threshold: f32,
}

/// Treffer.
#[derive(Debug, Clone)]
pub struct VectorHit {
    /// ID.
    pub id: Hash32,
    /// Kosinus-Ähnlichkeit.
    pub score: f32,
    /// Gecachte Antwort.
    pub payload: Bytes,
}

/// Was [`VectorIndex::evict`] entfernt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evict {
    /// Einen Eintrag.
    Id(Hash32),
    /// Alle Einträge einer Partition.
    Partition(Hash32),
    /// Alle abgelaufenen Einträge.
    Expired,
}

/// Fehler eines Vektor-Backends.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    /// Backend nicht erreichbar bzw. Befehl fehlgeschlagen.
    #[error("{backend}: {msg}")]
    Backend {
        /// Backend-Name.
        backend: &'static str,
        /// Meldung.
        msg: String,
    },
    /// Vektor passt nicht (Dimension, NaN, Nullvektor).
    #[error("ungültiger Vektor: {0}")]
    InvalidVector(String),
}

impl IndexError {
    pub(crate) fn backend(backend: &'static str, msg: impl std::fmt::Display) -> Self {
        Self::Backend {
            backend,
            msg: msg.to_string(),
        }
    }
}

/// Vektor-Backend.
#[async_trait]
pub trait VectorIndex: Send + Sync + std::fmt::Debug + 'static {
    /// Name (`memory`, `pgvector`, `redis`).
    fn name(&self) -> &'static str;

    /// Fügt ein bzw. ersetzt (gleiche ID).
    async fn insert(&self, entry: VectorEntry) -> Result<(), IndexError>;

    /// Bis zu `k` nicht abgelaufene Einträge der Partition mit Ähnlichkeit ≥ `threshold`,
    /// absteigend nach Ähnlichkeit.
    async fn search(&self, q: VectorQuery<'_>) -> Result<Vec<VectorHit>, IndexError>;

    /// Entfernt Einträge; liefert die Anzahl (soweit bekannt).
    async fn evict(&self, what: Evict) -> Result<u64, IndexError>;

    /// Anzahl gespeicherter Einträge (soweit billig bekannt, sonst `None`).
    fn len_hint(&self) -> Option<u64> {
        None
    }
}

/// Jetzt in Unix-Millisekunden.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Vektor als Little-Endian-`f32`-Bytes.
pub fn vector_to_le_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// Little-Endian-`f32`-Bytes → Vektor (`None` bei falscher Länge).
pub fn vector_from_le_bytes(b: &[u8]) -> Option<Vec<f32>> {
    let (chunks, rest) = b.as_chunks::<4>();
    if !rest.is_empty() {
        return None;
    }
    Some(chunks.iter().map(|c| f32::from_le_bytes(*c)).collect())
}
