// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Zeitquelle der Zähler (austauschbar für Tests).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Liefert die aktuelle Zeit in Millisekunden seit der Unix-Epoche.
///
/// Buckets werden an Epochengrenzen ausgerichtet, damit mehrere Instanzen (postgres-sync)
/// dieselben Bucket-Nummern verwenden. Uhrenabweichungen zwischen Instanzen verschieben die
/// Grenzen entsprechend; `redis` verwendet deshalb die Serverzeit von Redis.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// Millisekunden seit 1970-01-01 UTC.
    fn now_ms(&self) -> u64;
}

/// Systemuhr.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }
}

/// Von Hand gestellte Uhr für Tests.
#[derive(Debug, Default)]
pub struct ManualClock(AtomicU64);

impl ManualClock {
    /// Uhr auf `now_ms`.
    pub fn new(now_ms: u64) -> Self {
        Self(AtomicU64::new(now_ms))
    }

    /// Stellt die Uhr.
    pub fn set(&self, now_ms: u64) {
        self.0.store(now_ms, Ordering::SeqCst);
    }

    /// Stellt die Uhr um `ms` vor.
    pub fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}
