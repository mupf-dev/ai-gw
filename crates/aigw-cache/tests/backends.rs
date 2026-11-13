// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Cache und Vektor-Backends: exakt (RAM, Redis), semantisch (eigener HNSW, pgvector, Redis
//! Stack), Ausfälle als Miss, Warmstart aus der Datenbank.
//!
//! Externe Backends nur mit Umgebungsvariablen (sonst übersprungen):
//! - `AIGW_TEST_PGVECTOR_URL=postgres://…` – Postgres mit Erweiterung `vector`
//!   (`pgvector/pgvector:pg17`; legt je Lauf eine Wegwerf-Datenbank an),
//! - `AIGW_TEST_REDIS_STACK_URL=redis://…` – Redis Stack bzw. Valkey mit Search-Modul
//!   (`redis/redis-stack-server`).
//!
//! `scripts/check.sh` setzt beide mit `AIGW_TEST_POSTGRES=docker` bzw. `AIGW_TEST_REDIS=docker`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::sync::Arc;
use std::time::Duration;

use aigw_cache::key::Hash32;
use aigw_cache::redis_conn::RedisConn;
use aigw_cache::vector::memory::{MemoryIndex, MemoryOptions};
use aigw_cache::vector::pgvector::PgVectorIndex;
use aigw_cache::vector::redis::RedisVectorIndex;
use aigw_cache::vector::{Evict, VectorEntry, VectorIndex, VectorQuery, now_ms};
use aigw_cache::{
    Embedder, HitKind, Lookup, LookupRequest, ResponseCache, Scope, SemanticLookup, persist,
};
use aigw_core::config::CacheConfig;
use aigw_core::ir::{ChatRequest, ChatResponse, Choice, Content, Message, Role, StopReason};
use async_trait::async_trait;
use bytes::Bytes;

// ---------------------------------------------------------------------------------------------
// Hilfen

/// Deterministisches Test-Embedding: Wortzähler über 64 Hash-Buckets (ähnliche Sätze → hohe
/// Kosinus-Ähnlichkeit). Kein externer Aufruf.
#[derive(Debug, Default)]
struct WordEmbedder {
    fail: bool,
}

#[async_trait]
impl Embedder for WordEmbedder {
    async fn embed(&self, model: &str, text: &str) -> Result<Vec<f32>, String> {
        assert_eq!(model, "embed");
        if self.fail {
            return Err("Embedding-Dienst aus".into());
        }
        let mut v = vec![0f32; 64];
        for w in text
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
        {
            let h = blake3_bucket(w);
            v[h] += 1.0;
        }
        Ok(v)
    }
}

fn blake3_bucket(w: &str) -> usize {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in w.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    (h % 64) as usize
}

fn req(system: &str, user: &str) -> ChatRequest {
    let mut r = ChatRequest {
        model: "m".into(),
        messages: vec![
            Message::text(Role::System, system),
            Message::text(Role::User, user),
        ],
        ..ChatRequest::default()
    };
    r.sampling.temperature = Some(0.0);
    r
}

fn resp(text: &str) -> ChatResponse {
    ChatResponse {
        id: "r".into(),
        model: "m".into(),
        choices: vec![Choice {
            index: 0,
            message: Message {
                role: Role::Assistant,
                content: Content::Text(text.into()),
                name: None,
                extra: Default::default(),
            },
            stop_reason: Some(StopReason::EndTurn),
            stop_sequence: None,
            extra: Default::default(),
        }],
        ..ChatResponse::default()
    }
}

fn team(t: &str) -> Scope {
    Scope::Team(t.into())
}

async fn lookup(c: &ResponseCache, scope: Scope, r: &ChatRequest, semantic: bool) -> Lookup {
    c.lookup(&LookupRequest {
        scope,
        model_group: "g",
        request: r,
        semantic: semantic.then_some(SemanticLookup {
            embedding_model: "embed",
            threshold: 0.8,
            max_chars: 8000,
        }),
        skip_lookup: false,
        embed_ctx: None,
    })
    .await
}

fn hit_text(l: &Lookup) -> Option<(HitKind, String)> {
    match l {
        Lookup::Hit { kind, response } => Some((*kind, response.choices[0].message.content.text())),
        Lookup::Miss(_) => None,
    }
}

async fn wait_for(mut f: impl FnMut() -> bool) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("Bedingung nicht erfüllt");
}

// ---------------------------------------------------------------------------------------------
// Exakter Cache

#[tokio::test]
async fn exact_hit_is_scoped_and_expires() {
    let c = Arc::new(ResponseCache::new(&CacheConfig::default()));
    let r = req("sys", "Hallo");
    let Lookup::Miss(ticket) = lookup(&c, team("a"), &r, false).await else {
        panic!("Miss erwartet")
    };
    assert!(!ticket.has_semantic());
    assert!(c.store(ticket, &resp("Antwort"), Duration::from_secs(60)));
    assert_eq!(
        hit_text(&lookup(&c, team("a"), &r, false).await),
        Some((HitKind::Exact, "Antwort".into()))
    );
    // Anderes Team, anderer Key: nie mandantenübergreifend.
    assert!(hit_text(&lookup(&c, team("b"), &r, false).await).is_none());
    assert!(hit_text(&lookup(&c, Scope::Key("a".into()), &r, false).await).is_none());
    // TTL.
    let r2 = req("sys", "kurz");
    let Lookup::Miss(t2) = lookup(&c, team("a"), &r2, false).await else {
        panic!()
    };
    c.store(t2, &resp("x"), Duration::from_millis(30));
    assert!(hit_text(&lookup(&c, team("a"), &r2, false).await).is_some());
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(hit_text(&lookup(&c, team("a"), &r2, false).await).is_none());
}

#[tokio::test]
async fn too_large_entries_are_skipped() {
    let c = Arc::new(ResponseCache::new(&CacheConfig {
        max_entry_bytes: 100,
        ..CacheConfig::default()
    }));
    let r = req("sys", "groß");
    let Lookup::Miss(t) = lookup(&c, team("a"), &r, false).await else {
        panic!()
    };
    assert!(!c.store(t, &resp(&"x".repeat(500)), Duration::from_secs(60)));
    assert_eq!(
        c.metrics().skipped_count(aigw_cache::SkipReason::TooLarge),
        1
    );
}

#[tokio::test]
async fn redis_outage_is_a_miss() {
    // Port 1: keine Verbindung möglich.
    let conn =
        Arc::new(RedisConn::new("redis://127.0.0.1:1/0", Duration::from_millis(100)).unwrap());
    let c = Arc::new(ResponseCache::new(&CacheConfig::default()).with_redis(conn));
    let r = req("sys", "x");
    let Lookup::Miss(t) = lookup(&c, team("a"), &r, false).await else {
        panic!()
    };
    assert!(c.store(t, &resp("y"), Duration::from_secs(60)));
    // RAM-Ebene trifft trotzdem.
    assert!(hit_text(&lookup(&c, team("a"), &r, false).await).is_some());
    let r2 = req("sys", "anderes");
    assert!(hit_text(&lookup(&c, team("a"), &r2, false).await).is_none());
    wait_for(|| c.metrics().redis_errors() >= 1).await;
}

// ---------------------------------------------------------------------------------------------
// Semantischer Cache (eigener HNSW)

#[tokio::test]
async fn semantic_hit_threshold_partition_and_embedder_outage() {
    let index = Arc::new(MemoryIndex::new(MemoryOptions::default()));
    let c = Arc::new(ResponseCache::new(&CacheConfig::default()).with_memory_index(index));
    assert!(!c.semantic_available());
    assert!(c.set_embedder(Arc::new(WordEmbedder::default())));
    assert!(c.semantic_available());

    let r = req("Du bist hilfreich.", "Wie spät ist es gerade in Berlin");
    let Lookup::Miss(t) = lookup(&c, team("a"), &r, true).await else {
        panic!()
    };
    assert!(t.has_semantic());
    c.store(t, &resp("12 Uhr"), Duration::from_secs(60));
    wait_for(|| c.metrics().vector_stores() == 1).await;

    // Leicht anders formuliert → semantischer Treffer.
    let similar = req("Du bist hilfreich.", "wie spät ist es in Berlin gerade?");
    match hit_text(&lookup(&c, team("a"), &similar, true).await) {
        Some((HitKind::Semantic(score), text)) => {
            assert!(score >= 0.8, "{score}");
            assert_eq!(text, "12 Uhr");
        }
        other => panic!("semantischer Treffer erwartet: {other:?}"),
    }
    // Ohne semantischen Modus: Miss.
    assert!(hit_text(&lookup(&c, team("a"), &similar, false).await).is_none());
    // Anderer Systemprompt (harter Filter), anderes Team, anderes Thema: Miss.
    let other_sys = req(
        "Antworte nur auf Englisch.",
        "wie spät ist es in Berlin gerade?",
    );
    assert!(hit_text(&lookup(&c, team("a"), &other_sys, true).await).is_none());
    assert!(hit_text(&lookup(&c, team("b"), &similar, true).await).is_none());
    let unrelated = req("Du bist hilfreich.", "Erkläre mir die Relativitätstheorie");
    assert!(hit_text(&lookup(&c, team("a"), &unrelated, true).await).is_none());

    // Embedding-Ausfall → Miss ohne Semantik, kein Fehler.
    let c2 = Arc::new(
        ResponseCache::new(&CacheConfig::default())
            .with_memory_index(Arc::new(MemoryIndex::new(MemoryOptions::default()))),
    );
    c2.set_embedder(Arc::new(WordEmbedder { fail: true }));
    match lookup(&c2, team("a"), &similar, true).await {
        Lookup::Miss(t) => assert!(!t.has_semantic()),
        Lookup::Hit { .. } => panic!("Miss erwartet"),
    }
}

#[tokio::test]
async fn memory_index_warm_start_from_database() {
    let store = aigw_store::Store::connect("sqlite::memory:").await.unwrap();
    store.migrate().await.unwrap();
    let (key, _) = aigw_store::MasterKey::generate().unwrap();
    let keyring = Arc::new(aigw_store::Keyring::new(key));
    let (tx, _task) = persist::spawn_writer(store.clone(), Arc::clone(&keyring));
    let index = MemoryIndex::new(MemoryOptions::default()).with_persist(tx);
    let e = entry(1, 1, unit(0), 60_000);
    index.insert(e.clone()).await.unwrap();
    index.insert(entry(2, 1, unit(1), -1)).await.unwrap(); // abgelaufen
    let mut loaded = Vec::new();
    for _ in 0..200 {
        loaded = persist::load(&store, &keyring, 100).await.unwrap();
        if !loaded.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].payload, e.payload);
    let warm = MemoryIndex::new(MemoryOptions::default());
    assert_eq!(warm.warm(loaded), 1);
    let hits = warm.search(query(1, &unit(0), 0.99)).await.unwrap();
    assert_eq!(hits[0].id, hid(1));
    // Löschen kommt ebenfalls in der Datenbank an.
    index.evict(Evict::Id(hid(1))).await.unwrap();
    for _ in 0..200 {
        if persist::load(&store, &keyring, 100)
            .await
            .unwrap()
            .is_empty()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("Eintrag nicht gelöscht");
}

// ---------------------------------------------------------------------------------------------
// Gemeinsame Prüfung aller Vektor-Backends

const DIM: usize = 8;

fn hid(i: u64) -> Hash32 {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&i.to_le_bytes());
    b[31] = 0xab;
    Hash32(b)
}

fn unit(i: usize) -> Vec<f32> {
    let mut v = vec![0f32; DIM];
    v[i] = 1.0;
    v
}

fn entry(id: u64, part: u64, vector: Vec<f32>, ttl_ms: i64) -> VectorEntry {
    VectorEntry {
        id: hid(id),
        partition: hid(1000 + part),
        vector,
        payload: Bytes::from(format!("payload-{id}")),
        expires_at_ms: now_ms() + ttl_ms,
    }
}

fn query(part: u64, v: &[f32], threshold: f32) -> VectorQuery<'_> {
    VectorQuery {
        partition: hid(1000 + part),
        vector: v,
        k: 1,
        threshold,
    }
}

async fn exercise(index: &dyn VectorIndex) {
    index.insert(entry(1, 1, unit(0), 60_000)).await.unwrap();
    index.insert(entry(2, 1, unit(1), 60_000)).await.unwrap();
    index.insert(entry(3, 2, unit(0), 60_000)).await.unwrap();
    let mut near = unit(0);
    near[1] = 0.2;
    let hits = index.search(query(1, &near, 0.9)).await.unwrap();
    assert_eq!(hits.len(), 1, "{}", index.name());
    assert_eq!(hits[0].id, hid(1));
    assert_eq!(hits[0].payload, Bytes::from("payload-1"));
    assert!(
        hits[0].score > 0.95 && hits[0].score <= 1.0001,
        "{}",
        hits[0].score
    );
    // Schwelle.
    let mid: Vec<f32> = (0..DIM).map(|i| if i < 3 { 1.0 } else { 0.0 }).collect();
    assert!(index.search(query(1, &mid, 0.9)).await.unwrap().is_empty());
    // Partition ist harter Filter.
    assert!(
        index
            .search(query(3, &unit(0), 0.1))
            .await
            .unwrap()
            .is_empty()
    );
    // Ersetzen.
    index.insert(entry(1, 1, unit(2), 60_000)).await.unwrap();
    assert!(
        index
            .search(query(1, &unit(0), 0.9))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        index.search(query(1, &unit(2), 0.9)).await.unwrap()[0].id,
        hid(1)
    );
    // Abgelaufen.
    index.insert(entry(4, 1, unit(3), -1)).await.unwrap();
    assert!(
        index
            .search(query(1, &unit(3), 0.9))
            .await
            .unwrap()
            .is_empty()
    );
    // Löschen.
    assert_eq!(index.evict(Evict::Id(hid(2))).await.unwrap(), 1);
    assert!(
        index
            .search(query(1, &unit(1), 0.9))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(index.evict(Evict::Partition(hid(1002))).await.unwrap(), 1);
    assert!(
        index
            .search(query(2, &unit(0), 0.1))
            .await
            .unwrap()
            .is_empty()
    );
    index.evict(Evict::Expired).await.unwrap();
    // Falsche Dimension.
    assert!(
        index
            .insert(entry(9, 1, vec![1.0; DIM + 1], 1000))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn memory_backend() {
    exercise(&MemoryIndex::new(MemoryOptions::default())).await;
}

fn unique() -> String {
    format!(
        "{}{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

#[tokio::test]
async fn redis_stack_backend_and_exact_second_tier() {
    let Ok(url) = std::env::var("AIGW_TEST_REDIS_STACK_URL") else {
        eprintln!("AIGW_TEST_REDIS_STACK_URL nicht gesetzt – Redis-Stack-Test übersprungen");
        return;
    };
    let prefix = format!("aigw-test-{}:", unique());
    let conn = Arc::new(RedisConn::new(&url, Duration::from_secs(2)).unwrap());
    let index = RedisVectorIndex::new(
        Arc::clone(&conn),
        DIM,
        aigw_cache::hnsw::HnswParams::default(),
        &prefix,
    );
    exercise(&index).await;

    // Exakte zweite Ebene: zweite Instanz (leerer RAM) trifft über Redis.
    let cfg = CacheConfig {
        redis_key_prefix: prefix.clone(),
        ..CacheConfig::default()
    };
    let a = Arc::new(ResponseCache::new(&cfg).with_redis(Arc::clone(&conn)));
    let b = Arc::new(ResponseCache::new(&cfg).with_redis(Arc::clone(&conn)));
    let r = req("sys", "geteilt");
    let Lookup::Miss(t) = lookup(&a, team("a"), &r, false).await else {
        panic!()
    };
    a.store(t, &resp("aus Redis"), Duration::from_secs(60));
    let mut got = None;
    for _ in 0..100 {
        got = hit_text(&lookup(&b, team("a"), &r, false).await);
        if got.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(got, Some((HitKind::Exact, "aus Redis".into())));
    assert!(hit_text(&lookup(&b, team("b"), &r, false).await).is_none());

    // Semantisch über Redis mit Test-Embedder.
    let c = Arc::new(
        ResponseCache::new(&cfg).with_vectors(Arc::new(RedisVectorIndex::new(
            Arc::clone(&conn),
            64,
            aigw_cache::hnsw::HnswParams::default(),
            &prefix,
        ))),
    );
    c.set_embedder(Arc::new(WordEmbedder::default()));
    let r = req("sys", "Wie spät ist es gerade in Berlin");
    let Lookup::Miss(t) = lookup(&c, team("a"), &r, true).await else {
        panic!()
    };
    c.store(t, &resp("12 Uhr"), Duration::from_secs(60));
    wait_for(|| c.metrics().vector_stores() == 1).await;
    let similar = req("sys", "wie spät ist es in Berlin gerade?");
    assert!(matches!(
        hit_text(&lookup(&c, team("a"), &similar, true).await),
        Some((HitKind::Semantic(_), _))
    ));
}

#[tokio::test]
async fn pgvector_backend() {
    let Ok(base) = std::env::var("AIGW_TEST_PGVECTOR_URL") else {
        eprintln!("AIGW_TEST_PGVECTOR_URL nicht gesetzt – pgvector-Test übersprungen");
        return;
    };
    let db = format!("aigw_cache_test_{}", unique());
    let admin = sqlx::PgPool::connect(&base).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {db}")))
        .execute(&admin)
        .await
        .unwrap();
    let (prefix, _) = base.rsplit_once('/').expect("URL mit Datenbankpfad");
    let url = format!("{prefix}/{db}");
    {
        let index = PgVectorIndex::connect_lazy(
            &url,
            DIM,
            aigw_cache::hnsw::HnswParams::default(),
            Duration::from_secs(5),
        )
        .unwrap();
        index.ensure_schema().await.unwrap();
        index.ensure_schema().await.unwrap(); // idempotent
        exercise(&index).await;
        // Der HNSW-Index existiert in Postgres.
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_indexes WHERE indexdef ILIKE '%USING hnsw%'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 1);
        pool.close().await;
    }
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE {db} WITH (FORCE)"
    )))
    .execute(&admin)
    .await
    .unwrap();
}

#[tokio::test]
async fn pgvector_outage_is_an_error_not_a_hang() {
    let index = PgVectorIndex::connect_lazy(
        "postgres://u:p@127.0.0.1:1/x",
        DIM,
        aigw_cache::hnsw::HnswParams::default(),
        Duration::from_millis(200),
    )
    .unwrap();
    let started = std::time::Instant::now();
    assert!(index.search(query(1, &unit(0), 0.5)).await.is_err());
    assert!(started.elapsed() < Duration::from_secs(5));
    // Im Cache: Miss, kein Fehler.
    let c = Arc::new(ResponseCache::new(&CacheConfig::default()).with_vectors(Arc::new(index)));
    c.set_embedder(Arc::new(WordEmbedder::default()));
    let r = req("sys", "irgendwas");
    assert!(matches!(
        lookup(&c, team("a"), &r, true).await,
        Lookup::Miss(_)
    ));
}
