// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Kennzahlen des Caches (Prometheus-Textformat, `/metrics` der Steuerebene).
//!
//! | Metrik | Bedeutung |
//! |---|---|
//! | `aigw_cache_lookups_total{result}` | `hit-exact`, `hit-semantic`, `miss`, `bypass` |
//! | `aigw_cache_stores_total{tier}` | gespeicherte Antworten (`memory`, `redis`, `vector`) |
//! | `aigw_cache_store_skipped_total{reason}` | nicht gespeichert: `too_large`, `tool_calls`, `stop_reason`, `incomplete`, `no_store` |
//! | `aigw_cache_backend_errors_total{backend}` | Fehler externer Backends (→ Miss) |
//! | `aigw_cache_embeddings_total{result}` | Embedding-Aufrufe `ok`/`error` |
//! | `aigw_cache_embedding_seconds_sum`/`_count` | Dauer der Embedding-Aufrufe |
//! | `aigw_cache_memory_bytes`, `aigw_cache_memory_entries` | RAM-LRU |
//! | `aigw_cache_memory_evictions_total` | wegen der Byte-Grenze verdrängt |
//! | `aigw_cache_vector_entries` | Einträge im Vektor-Backend `memory` |

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use aigw_core::observe::CacheOutcome;

/// Grund, warum eine Antwort nicht gespeichert wurde.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Größer als `max_entry_bytes`.
    TooLarge,
    /// Tool-Aufrufe (Richtlinie `cacheToolCalls: false`).
    ToolCalls,
    /// Inhaltsfilter, Ablehnung, Pause o. Ä.
    StopReason,
    /// Stream abgebrochen bzw. mit Fehler beendet.
    Incomplete,
    /// Client `Cache-Control: no-store` bzw. `x-aigw-cache-ttl: 0`.
    NoStore,
}

const SKIP: [(&str, SkipReason); 5] = [
    ("too_large", SkipReason::TooLarge),
    ("tool_calls", SkipReason::ToolCalls),
    ("stop_reason", SkipReason::StopReason),
    ("incomplete", SkipReason::Incomplete),
    ("no_store", SkipReason::NoStore),
];

/// Zähler des Caches.
#[derive(Debug, Default)]
pub struct CacheMetrics {
    lookups: [AtomicU64; 4],
    stores_memory: AtomicU64,
    stores_redis: AtomicU64,
    stores_vector: AtomicU64,
    skipped: [AtomicU64; 5],
    errors_redis: AtomicU64,
    errors_vector: AtomicU64,
    embed_ok: AtomicU64,
    embed_err: AtomicU64,
    embed_us: AtomicU64,
}

fn outcome_index(o: CacheOutcome) -> usize {
    match o {
        CacheOutcome::HitExact => 0,
        CacheOutcome::HitSemantic => 1,
        CacheOutcome::Miss => 2,
        CacheOutcome::Bypass => 3,
    }
}

impl CacheMetrics {
    /// Ergebnis eines Nachschlagens (vom Proxy gezählt, auch `bypass`).
    pub fn lookup(&self, o: CacheOutcome) {
        self.lookups[outcome_index(o)].fetch_add(1, Ordering::Relaxed);
    }

    /// Anzahl der Nachschlagen mit Ergebnis `o`.
    pub fn lookups(&self, o: CacheOutcome) -> u64 {
        self.lookups[outcome_index(o)].load(Ordering::Relaxed)
    }

    /// Nicht gespeichert.
    pub fn skipped(&self, r: SkipReason) {
        if let Some(i) = SKIP.iter().position(|(_, x)| *x == r) {
            self.skipped[i].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Anzahl nicht gespeicherter Antworten aus Grund `r`.
    pub fn skipped_count(&self, r: SkipReason) -> u64 {
        SKIP.iter()
            .position(|(_, x)| *x == r)
            .map_or(0, |i| self.skipped[i].load(Ordering::Relaxed))
    }

    pub(crate) fn stored_memory(&self) {
        self.stores_memory.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn stored_redis(&self) {
        self.stores_redis.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn stored_vector(&self) {
        self.stores_vector.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn redis_error(&self) {
        self.errors_redis.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn vector_error(&self) {
        self.errors_vector.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn embedded(&self, ok: bool, took: Duration) {
        if ok {
            self.embed_ok.fetch_add(1, Ordering::Relaxed);
        } else {
            self.embed_err.fetch_add(1, Ordering::Relaxed);
        }
        self.embed_us.fetch_add(
            u64::try_from(took.as_micros()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    /// Fehler des Vektor-Backends.
    pub fn vector_errors(&self) -> u64 {
        self.errors_vector.load(Ordering::Relaxed)
    }

    /// Fehler der Redis-Ebene (exakt).
    pub fn redis_errors(&self) -> u64 {
        self.errors_redis.load(Ordering::Relaxed)
    }

    /// Gespeicherte Antworten im Vektor-Backend.
    pub fn vector_stores(&self) -> u64 {
        self.stores_vector.load(Ordering::Relaxed)
    }

    /// Gespeicherte Antworten im RAM-LRU.
    pub fn memory_stores(&self) -> u64 {
        self.stores_memory.load(Ordering::Relaxed)
    }

    /// Prometheus-Text; `gauges` = (Bytes, Einträge, Verdrängungen, Vektor-Einträge).
    pub(crate) fn render(&self, out: &mut String, gauges: (u64, u64, u64, Option<u64>)) {
        let _ = writeln!(
            out,
            "# HELP aigw_cache_lookups_total Ergebnisse des Antwort-Caches.\n# TYPE aigw_cache_lookups_total counter"
        );
        for (i, name) in ["hit-exact", "hit-semantic", "miss", "bypass"]
            .iter()
            .enumerate()
        {
            let _ = writeln!(
                out,
                "aigw_cache_lookups_total{{result=\"{name}\"}} {}",
                self.lookups[i].load(Ordering::Relaxed)
            );
        }
        let _ = writeln!(
            out,
            "# HELP aigw_cache_stores_total Gespeicherte Antworten je Ebene.\n# TYPE aigw_cache_stores_total counter"
        );
        for (tier, v) in [
            ("memory", &self.stores_memory),
            ("redis", &self.stores_redis),
            ("vector", &self.stores_vector),
        ] {
            let _ = writeln!(
                out,
                "aigw_cache_stores_total{{tier=\"{tier}\"}} {}",
                v.load(Ordering::Relaxed)
            );
        }
        let _ = writeln!(
            out,
            "# HELP aigw_cache_store_skipped_total Nicht gespeicherte Antworten.\n# TYPE aigw_cache_store_skipped_total counter"
        );
        for (i, (name, _)) in SKIP.iter().enumerate() {
            let _ = writeln!(
                out,
                "aigw_cache_store_skipped_total{{reason=\"{name}\"}} {}",
                self.skipped[i].load(Ordering::Relaxed)
            );
        }
        let _ = writeln!(
            out,
            "# HELP aigw_cache_backend_errors_total Fehler externer Cache-Backends (gelten als Miss).\n# TYPE aigw_cache_backend_errors_total counter"
        );
        for (b, v) in [
            ("redis", &self.errors_redis),
            ("vector", &self.errors_vector),
        ] {
            let _ = writeln!(
                out,
                "aigw_cache_backend_errors_total{{backend=\"{b}\"}} {}",
                v.load(Ordering::Relaxed)
            );
        }
        let ok = self.embed_ok.load(Ordering::Relaxed);
        let err = self.embed_err.load(Ordering::Relaxed);
        let _ = writeln!(
            out,
            "# HELP aigw_cache_embeddings_total Embedding-Aufrufe des semantischen Caches.\n# TYPE aigw_cache_embeddings_total counter\naigw_cache_embeddings_total{{result=\"ok\"}} {ok}\naigw_cache_embeddings_total{{result=\"error\"}} {err}"
        );
        let _ = writeln!(
            out,
            "# HELP aigw_cache_embedding_seconds Dauer der Embedding-Aufrufe.\n# TYPE aigw_cache_embedding_seconds summary\naigw_cache_embedding_seconds_sum {}\naigw_cache_embedding_seconds_count {}",
            self.embed_us.load(Ordering::Relaxed) as f64 / 1e6,
            ok + err
        );
        let (bytes, entries, evictions, vectors) = gauges;
        let _ = writeln!(
            out,
            "# HELP aigw_cache_memory_bytes Belegte Bytes des RAM-LRU.\n# TYPE aigw_cache_memory_bytes gauge\naigw_cache_memory_bytes {bytes}\n# HELP aigw_cache_memory_entries Einträge im RAM-LRU.\n# TYPE aigw_cache_memory_entries gauge\naigw_cache_memory_entries {entries}\n# HELP aigw_cache_memory_evictions_total Wegen der Byte-Grenze verdrängte Einträge.\n# TYPE aigw_cache_memory_evictions_total counter\naigw_cache_memory_evictions_total {evictions}"
        );
        if let Some(v) = vectors {
            let _ = writeln!(
                out,
                "# HELP aigw_cache_vector_entries Einträge im Vektor-Backend memory.\n# TYPE aigw_cache_vector_entries gauge\naigw_cache_vector_entries {v}"
            );
        }
    }
}
