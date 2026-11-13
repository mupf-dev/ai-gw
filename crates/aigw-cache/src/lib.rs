// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Antwort-Cache von ai-gw (AIGW-39 exakt, AIGW-40 semantisch; Architektur §10,
//! Doku `docs/konzept/14-cache.md`).
//!
//! - **Exakt:** Schlüssel = BLAKE3 über Scope, Modellgruppe und die normalisierte IR-Anfrage
//!   ([`key`]); Ablage im RAM-LRU mit Byte-Grenze und TTL ([`lru`]), optional zusätzlich in
//!   Redis/Valkey (gemeinsam für alle Instanzen).
//! - **Semantisch:** Embedding der letzten Nutzernachricht über eine Embedding-Modellgruppe
//!   des Gateways ([`Embedder`], vom Proxy über das interne Routing bereitgestellt), Suche im
//!   [`vector::VectorIndex`] innerhalb der Partition (Scope, Modellgruppe, Systemprompt,
//!   Tools, Parameter, Verlauf) mit Mindest-Ähnlichkeit je Modellgruppe.
//! - **Ausfälle** externer Backends (Redis, pgvector, Embedding) sind ein Cache-Miss, nie ein
//!   Fehler der Anfrage.
//!
//! Wann gecacht wird (Richtlinie, Client-Steuerung, Temperatur, Tool-Aufrufe), entscheidet die
//! Pipeline in `aigw-proxy`; diese Crate stellt Nachschlagen ([`ResponseCache::lookup`]) und
//! Speichern ([`ResponseCache::store`]) bereit.

pub mod hnsw;
pub mod key;
pub mod lru;
pub mod metrics;
pub mod persist;
pub mod redis_conn;
pub mod replay;
pub mod vector;

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use aigw_core::config::{CacheConfig, Config, VectorBackendKind};
use aigw_core::ir::{ChatRequest, ChatResponse};
use async_trait::async_trait;
use bytes::Bytes;

pub use key::{Hash32, Scope};
pub use metrics::{CacheMetrics, SkipReason};
pub use replay::{StreamCollector, chunk_events};

use crate::hnsw::HnswParams;
use crate::lru::MemoryLru;
use crate::redis_conn::RedisConn;
use crate::vector::memory::{MemoryIndex, MemoryOptions};
use crate::vector::{VectorEntry, VectorIndex, VectorQuery, now_ms};

/// Auslöser eines Embedding-Aufrufs (F169): Der Proxy legt hier seinen Anfragekontext ab,
/// damit der Embedder die Kosten dem anfragenden Key berechnen kann. Für den Cache
/// undurchsichtig.
#[derive(Clone, Default)]
pub struct EmbedContext {
    /// Request-ID der auslösenden Anfrage.
    pub request_id: String,
    /// Anfragekontext des Aufrufers (z. B. Key), nur vom Embedder ausgewertet.
    pub caller: Option<Arc<dyn std::any::Any + Send + Sync>>,
}

impl std::fmt::Debug for EmbedContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbedContext")
            .field("request_id", &self.request_id)
            .field("caller", &self.caller.is_some())
            .finish()
    }
}

/// Erzeugt Embeddings über eine Modellgruppe des Gateways (internes Routing, keine externen
/// Direktaufrufe). Der Proxy stellt die Implementierung bereit.
#[async_trait]
pub trait Embedder: Send + Sync + std::fmt::Debug + 'static {
    /// Embedding von `text` mit der Modellgruppe `model`.
    async fn embed(&self, model: &str, text: &str) -> Result<Vec<f32>, String>;

    /// Wie [`Embedder::embed`], mit dem Auslöser (Abrechnung auf den anfragenden Key, F169).
    /// Standard: ohne Abrechnung.
    async fn embed_for(
        &self,
        model: &str,
        text: &str,
        ctx: &EmbedContext,
    ) -> Result<Vec<f32>, String> {
        let _ = ctx;
        self.embed(model, text).await
    }
}

/// Art eines Treffers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HitKind {
    /// Exakter Treffer.
    Exact,
    /// Semantischer Treffer (Kosinus-Ähnlichkeit).
    Semantic(f32),
}

impl HitKind {
    /// Ergebnis für Header und Request-Log.
    pub fn outcome(self) -> aigw_core::observe::CacheOutcome {
        match self {
            Self::Exact => aigw_core::observe::CacheOutcome::HitExact,
            Self::Semantic(_) => aigw_core::observe::CacheOutcome::HitSemantic,
        }
    }
}

/// Semantischer Teil eines Nachschlagens.
#[derive(Debug, Clone)]
pub struct SemanticLookup<'a> {
    /// Embedding-Modellgruppe.
    pub embedding_model: &'a str,
    /// Mindest-Kosinus-Ähnlichkeit.
    pub threshold: f32,
    /// Längere Nutzernachrichten nur exakt.
    pub max_chars: usize,
}

/// Nachschlagen.
#[derive(Debug, Clone)]
pub struct LookupRequest<'a> {
    /// Scope (aufgelöst).
    pub scope: Scope,
    /// Kanonische Modellgruppe.
    pub model_group: &'a str,
    /// Anfrage nach allen Pre-Plugins.
    pub request: &'a ChatRequest,
    /// Semantisch suchen (sonst nur exakt).
    pub semantic: Option<SemanticLookup<'a>>,
    /// Nachschlagen überspringen (Client `no-cache`), nur Ticket zum Speichern bilden.
    pub skip_lookup: bool,
    /// Auslöser für den Embedding-Aufruf des semantischen Caches (Abrechnung, F169).
    pub embed_ctx: Option<&'a EmbedContext>,
}

/// Was zum Speichern nach einem Miss nötig ist.
#[derive(Debug, Clone)]
pub struct StoreTicket {
    key: Hash32,
    semantic: Option<(Hash32, Vec<f32>)>,
}

impl StoreTicket {
    /// Exakter Schlüssel.
    pub fn key(&self) -> Hash32 {
        self.key
    }

    /// Wird auch semantisch gespeichert?
    pub fn has_semantic(&self) -> bool {
        self.semantic.is_some()
    }
}

/// Ergebnis des Nachschlagens.
#[derive(Debug)]
pub enum Lookup {
    /// Treffer.
    Hit {
        /// Art.
        kind: HitKind,
        /// Gecachte Antwort.
        response: Box<ChatResponse>,
    },
    /// Kein Treffer; Ticket für [`ResponseCache::store`].
    Miss(StoreTicket),
}

/// Der Antwort-Cache einer Instanz.
pub struct ResponseCache {
    cfg: CacheConfig,
    memory: MemoryLru<Arc<ChatResponse>>,
    redis: Option<Arc<RedisConn>>,
    vectors: Option<Arc<dyn VectorIndex>>,
    memory_index: Option<Arc<MemoryIndex>>,
    embedder: OnceLock<Arc<dyn Embedder>>,
    metrics: CacheMetrics,
}

impl std::fmt::Debug for ResponseCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseCache")
            .field("memory", &self.memory)
            .field("redis", &self.redis.is_some())
            .field("vectors", &self.vectors.as_ref().map(|v| v.name()))
            .field("embedder", &self.embedder.get().is_some())
            .finish_non_exhaustive()
    }
}

impl ResponseCache {
    /// Cache nur mit RAM-LRU (ohne Redis, ohne semantischen Teil).
    pub fn new(cfg: &CacheConfig) -> Self {
        Self {
            cfg: cfg.clone(),
            memory: MemoryLru::new(cfg.memory_max_bytes),
            redis: None,
            vectors: None,
            memory_index: None,
            embedder: OnceLock::new(),
            metrics: CacheMetrics::default(),
        }
    }

    /// Mit Redis als zweiter Ebene für exakte Treffer.
    #[must_use]
    pub fn with_redis(mut self, conn: Arc<RedisConn>) -> Self {
        self.redis = Some(conn);
        self
    }

    /// Mit Vektor-Backend für den semantischen Cache.
    #[must_use]
    pub fn with_vectors(mut self, index: Arc<dyn VectorIndex>) -> Self {
        self.vectors = Some(index);
        self
    }

    /// Mit dem eigenen HNSW-Backend (Kennzahlen zusätzlich).
    #[must_use]
    pub fn with_memory_index(mut self, index: Arc<MemoryIndex>) -> Self {
        self.vectors = Some(Arc::clone(&index) as Arc<dyn VectorIndex>);
        self.memory_index = Some(index);
        self
    }

    /// Setzt den Embedder (einmalig; der Proxy verdrahtet ihn nach dem Routing).
    pub fn set_embedder(&self, embedder: Arc<dyn Embedder>) -> bool {
        self.embedder.set(embedder).is_ok()
    }

    /// Konfiguration.
    pub fn config(&self) -> &CacheConfig {
        &self.cfg
    }

    /// Kennzahlen.
    pub fn metrics(&self) -> &CacheMetrics {
        &self.metrics
    }

    /// Ist der semantische Cache nutzbar (Vektor-Backend und Embedder vorhanden)?
    pub fn semantic_available(&self) -> bool {
        self.vectors.is_some() && self.embedder.get().is_some()
    }

    /// Vektor-Backend.
    pub fn vectors(&self) -> Option<&Arc<dyn VectorIndex>> {
        self.vectors.as_ref()
    }

    /// RAM-LRU (Kennzahlen, Tests).
    pub fn memory(&self) -> &MemoryLru<Arc<ChatResponse>> {
        &self.memory
    }

    fn redis_key(&self, key: &Hash32) -> String {
        format!("{}x:{}", self.cfg.redis_key_prefix, key.to_hex())
    }

    /// Schlägt nach: exakt (RAM, dann Redis), dann – falls angefragt – semantisch.
    pub async fn lookup(&self, q: &LookupRequest<'_>) -> Lookup {
        let key = key::exact_key(&q.scope, q.model_group, q.request);
        if !q.skip_lookup {
            if let Some(resp) = self.memory.get(&key, Instant::now()) {
                return Lookup::Hit {
                    kind: HitKind::Exact,
                    response: Box::new((*resp).clone()),
                };
            }
            if let Some(resp) = self.redis_get(&key).await {
                return Lookup::Hit {
                    kind: HitKind::Exact,
                    response: Box::new(resp),
                };
            }
        }
        let semantic = match &q.semantic {
            Some(s) => self.semantic_lookup(q, s).await,
            None => None,
        };
        match semantic {
            Some(SemanticResult::Hit(score, resp)) => Lookup::Hit {
                kind: HitKind::Semantic(score),
                response: resp,
            },
            Some(SemanticResult::Miss(p, v)) => Lookup::Miss(StoreTicket {
                key,
                semantic: Some((p, v)),
            }),
            None => Lookup::Miss(StoreTicket {
                key,
                semantic: None,
            }),
        }
    }

    async fn redis_get(&self, key: &Hash32) -> Option<ChatResponse> {
        let conn = self.redis.as_ref()?;
        let mut cmd = redis::cmd("GET");
        cmd.arg(self.redis_key(key));
        match conn.query::<Option<Vec<u8>>>(&cmd).await {
            Ok(Some(b)) => {
                let resp: ChatResponse = serde_json::from_slice(&b).ok()?;
                // In den RAM übernehmen (Rest-TTL unbekannt: kurz halten).
                let ttl = Duration::from_secs(60.min(self.cfg.max_ttl_secs.max(1)));
                let arc = Arc::new(resp.clone());
                self.memory
                    .insert(*key, arc, b.len() as u64, ttl, Instant::now());
                Some(resp)
            }
            Ok(None) => None,
            Err(e) => {
                self.metrics.redis_error();
                tracing::debug!(error = %e, "Cache: Redis nicht verfügbar (Miss)");
                None
            }
        }
    }

    async fn semantic_lookup(
        &self,
        q: &LookupRequest<'_>,
        s: &SemanticLookup<'_>,
    ) -> Option<SemanticResult> {
        let index = self.vectors.as_ref()?;
        let embedder = self.embedder.get()?;
        let text = key::semantic_text(q.request)?;
        if text.chars().count() > s.max_chars {
            return None;
        }
        let started = Instant::now();
        let timeout = Duration::from_millis(self.cfg.semantic.embed_timeout_ms.max(1));
        let no_ctx = EmbedContext::default();
        let ctx = q.embed_ctx.unwrap_or(&no_ctx);
        let vector = match tokio::time::timeout(
            timeout,
            embedder.embed_for(s.embedding_model, &text, ctx),
        )
        .await
        {
            Ok(Ok(v)) if !v.is_empty() => {
                self.metrics.embedded(true, started.elapsed());
                v
            }
            Ok(Ok(_)) => {
                self.metrics.embedded(false, started.elapsed());
                return None;
            }
            Ok(Err(e)) => {
                self.metrics.embedded(false, started.elapsed());
                tracing::debug!(error = %e, model = s.embedding_model, "Cache: Embedding fehlgeschlagen (Miss)");
                return None;
            }
            Err(_) => {
                self.metrics.embedded(false, started.elapsed());
                tracing::debug!(
                    model = s.embedding_model,
                    "Cache: Embedding-Zeitlimit (Miss)"
                );
                return None;
            }
        };
        let partition = key::partition(&q.scope, q.model_group, q.request);
        if q.skip_lookup {
            return Some(SemanticResult::Miss(partition, vector));
        }
        let query = VectorQuery {
            partition,
            vector: &vector,
            k: 1,
            threshold: s.threshold,
        };
        match index.search(query).await {
            Ok(hits) => {
                for h in hits {
                    if let Ok(resp) = serde_json::from_slice::<ChatResponse>(&h.payload) {
                        return Some(SemanticResult::Hit(h.score, Box::new(resp)));
                    }
                }
            }
            Err(e) => {
                self.metrics.vector_error();
                tracing::debug!(backend = index.name(), error = %e, "Cache: Vektor-Backend nicht verfügbar (Miss)");
            }
        }
        Some(SemanticResult::Miss(partition, vector))
    }

    /// Speichert eine Antwort (RAM sofort, Redis und Vektor-Backend im Hintergrund).
    /// `false`, wenn sie zu groß ist.
    pub fn store(
        self: &Arc<Self>,
        ticket: StoreTicket,
        resp: &ChatResponse,
        ttl: Duration,
    ) -> bool {
        let ttl = ttl.min(Duration::from_secs(self.cfg.max_ttl_secs.max(1)));
        let Ok(json) = serde_json::to_vec(resp) else {
            return false;
        };
        let size = json.len() as u64;
        if size > self.cfg.max_entry_bytes || size > self.memory.max_entry_bytes() {
            self.metrics.skipped(SkipReason::TooLarge);
            return false;
        }
        if self.memory.insert(
            ticket.key,
            Arc::new(resp.clone()),
            size,
            ttl,
            Instant::now(),
        ) {
            self.metrics.stored_memory();
        }
        if self.redis.is_none() && ticket.semantic.is_none() {
            return true;
        }
        let this = Arc::clone(self);
        let json = Bytes::from(json);
        tokio::spawn(async move {
            this.store_external(ticket, json, ttl).await;
        });
        true
    }

    async fn store_external(&self, ticket: StoreTicket, json: Bytes, ttl: Duration) {
        if let Some(conn) = &self.redis {
            let mut cmd = redis::cmd("SET");
            cmd.arg(self.redis_key(&ticket.key))
                .arg(json.as_ref())
                .arg("PX")
                .arg(u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX).max(1));
            match conn.query::<()>(&cmd).await {
                Ok(()) => self.metrics.stored_redis(),
                Err(e) => {
                    self.metrics.redis_error();
                    tracing::debug!(error = %e, "Cache: Speichern in Redis fehlgeschlagen");
                }
            }
        }
        if let (Some((partition, vector)), Some(index)) = (ticket.semantic, &self.vectors) {
            let entry = VectorEntry {
                id: ticket.key,
                partition,
                vector,
                payload: json,
                expires_at_ms: now_ms()
                    .saturating_add(i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX)),
            };
            match index.insert(entry).await {
                Ok(()) => self.metrics.stored_vector(),
                Err(e) => {
                    self.metrics.vector_error();
                    tracing::debug!(backend = index.name(), error = %e, "Cache: Speichern im Vektor-Backend fehlgeschlagen");
                }
            }
        }
    }

    /// Räumt Abgelaufenes auf (RAM-LRU und Vektor-Backend).
    pub async fn sweep(&self) {
        self.memory.purge_expired(Instant::now());
        if let Some(index) = &self.vectors
            && let Err(e) = index.evict(vector::Evict::Expired).await
        {
            tracing::debug!(backend = index.name(), error = %e, "Cache: Aufräumen fehlgeschlagen");
        }
    }

    /// Prometheus-Text.
    pub fn render_prometheus(&self, out: &mut String) {
        self.metrics.render(
            out,
            (
                self.memory.bytes(),
                self.memory.len() as u64,
                self.memory.evictions(),
                self.memory_index.as_ref().and_then(|i| i.len_hint()),
            ),
        );
    }
}

enum SemanticResult {
    Hit(f32, Box<ChatResponse>),
    Miss(Hash32, Vec<f32>),
}

/// Laufender Cache mit Hintergrund-Tasks (Aufräumen, Persistenz); beim Verwerfen beendet.
#[derive(Debug)]
pub struct CacheRuntime {
    /// Der Cache.
    pub cache: Arc<ResponseCache>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for CacheRuntime {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// Persistenz für den Warmstart des Backends `memory`.
#[derive(Debug, Clone)]
pub struct Persistence {
    /// Datenbank.
    pub store: aigw_store::Store,
    /// Master-Keys (Verschlüsselung der Einträge).
    pub keyring: Arc<aigw_store::Keyring>,
}

/// Baut den Cache laut Konfiguration und startet die Hintergrund-Tasks.
///
/// Fehler externer Backends beim Start sind keine Startfehler (Ausfall = Miss); Fehler sind
/// nur ungültige Konfigurationen (z. B. `pgvector` ohne Dimension).
pub async fn start(
    config: &Config,
    persistence: Option<Persistence>,
) -> Result<CacheRuntime, String> {
    let cfg = &config.cache;
    let timeout = Duration::from_millis(cfg.backend_timeout_ms.max(1));
    let mut cache = ResponseCache::new(cfg);
    let redis_conn = |url: &str| RedisConn::new(url, timeout).map(Arc::new);
    if let Some(url) = &cfg.redis_url {
        cache = cache.with_redis(redis_conn(url).map_err(|e| format!("cache.redis_url: {e}"))?);
    }
    let sem = &cfg.semantic;
    let hnsw = HnswParams {
        m: sem.hnsw.m.max(2),
        ef_construction: sem.hnsw.ef_construction.max(1),
        ef_search: sem.hnsw.ef_search.max(1),
    };
    let dims = sem.dimensions.map(|d| d as usize);
    let mut tasks = Vec::new();
    match sem.backend {
        VectorBackendKind::Memory => {
            let mut index = MemoryIndex::new(MemoryOptions {
                hnsw,
                dims,
                max_entries: sem.max_entries,
                max_bytes: sem.max_bytes,
            });
            if sem.persist
                && let Some(p) = &persistence
            {
                match persist::load(&p.store, &p.keyring, sem.max_entries).await {
                    Ok(entries) => {
                        let n = index.warm(entries);
                        tracing::info!(
                            entries = n,
                            "semantischer Cache: Warmstart aus der Datenbank"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "semantischer Cache: Warmstart fehlgeschlagen")
                    }
                }
                let (tx, task) = persist::spawn_writer(p.store.clone(), Arc::clone(&p.keyring));
                index = index.with_persist(tx);
                tasks.push(task);
            }
            cache = cache.with_memory_index(Arc::new(index));
        }
        VectorBackendKind::Pgvector => {
            let dim = dims.ok_or("cache.semantic.dimensions ist für pgvector Pflicht")?;
            let url = sem
                .pgvector_url
                .clone()
                .or_else(|| {
                    (config.store.url.starts_with("postgres")).then(|| config.store.url.clone())
                })
                .ok_or("cache.semantic.pgvector_url fehlt (store.url ist kein Postgres)")?;
            let index = vector::pgvector::PgVectorIndex::connect_lazy(&url, dim, hnsw, timeout)
                .map_err(|e| format!("cache.semantic.pgvector_url: {e}"))?;
            if let Err(e) = index.ensure_schema().await {
                tracing::warn!(error = %e, "semantischer Cache: pgvector nicht bereit (Miss bis erreichbar)");
            }
            cache = cache.with_vectors(Arc::new(index));
        }
        VectorBackendKind::Redis => {
            let dim = dims.ok_or("cache.semantic.dimensions ist für redis Pflicht")?;
            let url = sem
                .redis_url
                .as_deref()
                .or(cfg.redis_url.as_deref())
                .ok_or("cache.semantic.redis_url bzw. cache.redis_url fehlt")?;
            let conn = redis_conn(url).map_err(|e| format!("cache.semantic.redis_url: {e}"))?;
            cache = cache.with_vectors(Arc::new(vector::redis::RedisVectorIndex::new(
                conn,
                dim,
                hnsw,
                &cfg.redis_key_prefix,
            )));
        }
    }
    let cache = Arc::new(cache);
    let weak = Arc::downgrade(&cache);
    tasks.push(tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.tick().await;
        loop {
            tick.tick().await;
            let Some(c) = weak.upgrade() else { break };
            c.sweep().await;
        }
    }));
    tracing::info!(
        enabled = cfg.enabled,
        memory_max_bytes = cfg.memory_max_bytes,
        redis = cfg.redis_url.is_some(),
        semantic_backend = ?sem.backend,
        "Antwort-Cache bereit"
    );
    Ok(CacheRuntime { cache, tasks })
}
