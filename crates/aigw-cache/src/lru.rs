// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! RAM-LRU für exakte Treffer: Byte-Grenze, TTL je Eintrag, 16 Shards (je ein Mutex) gegen
//! Konkurrenz im Hot Path.
//!
//! Je Shard: `HashMap` Schlüssel → Eintrag und ein `BTreeMap` Zugriffszähler → Schlüssel als
//! LRU-Reihenfolge (O(log n) je Zugriff). Abgelaufene Einträge werden beim Zugriff und beim
//! Verdrängen entfernt; [`MemoryLru::purge_expired`] räumt periodisch auf.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::key::Hash32;

const SHARDS: usize = 16;

struct Slot<V> {
    value: V,
    bytes: u64,
    tick: u64,
    expires: Instant,
}

struct Shard<V> {
    map: HashMap<Hash32, Slot<V>>,
    order: BTreeMap<u64, Hash32>,
    tick: u64,
    bytes: u64,
}

impl<V> Shard<V> {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            order: BTreeMap::new(),
            tick: 0,
            bytes: 0,
        }
    }

    fn remove(&mut self, key: &Hash32) -> bool {
        match self.map.remove(key) {
            Some(slot) => {
                self.order.remove(&slot.tick);
                self.bytes -= slot.bytes;
                true
            }
            None => false,
        }
    }
}

/// Byte-begrenzter LRU mit TTL.
pub struct MemoryLru<V> {
    shards: Box<[Mutex<Shard<V>>]>,
    shard_max: u64,
    evictions: AtomicU64,
}

impl<V: Clone> std::fmt::Debug for MemoryLru<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryLru")
            .field("max_bytes", &(self.shard_max * SHARDS as u64))
            .field("bytes", &self.bytes())
            .field("entries", &self.len())
            .finish()
    }
}

impl<V: Clone> MemoryLru<V> {
    /// LRU mit insgesamt `max_bytes`.
    pub fn new(max_bytes: u64) -> Self {
        Self {
            shards: (0..SHARDS).map(|_| Mutex::new(Shard::new())).collect(),
            shard_max: (max_bytes / SHARDS as u64).max(1),
            evictions: AtomicU64::new(0),
        }
    }

    fn shard(&self, key: &Hash32) -> std::sync::MutexGuard<'_, Shard<V>> {
        self.shards[usize::from(key.0[0]) % SHARDS]
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Größter Eintrag, der hineinpasst.
    pub fn max_entry_bytes(&self) -> u64 {
        self.shard_max
    }

    /// Liest und markiert als zuletzt benutzt; abgelaufene Einträge werden entfernt.
    pub fn get(&self, key: &Hash32, now: Instant) -> Option<V> {
        let mut s = self.shard(key);
        let s = &mut *s;
        let expired = s.map.get(key)?.expires <= now;
        if expired {
            s.remove(key);
            return None;
        }
        s.tick += 1;
        let tick = s.tick;
        let slot = s.map.get_mut(key)?;
        s.order.remove(&slot.tick);
        slot.tick = tick;
        s.order.insert(tick, *key);
        Some(slot.value.clone())
    }

    /// Fügt ein bzw. ersetzt; verdrängt dafür die am längsten unbenutzten Einträge.
    /// `false`, wenn der Eintrag größer als ein Shard ist.
    pub fn insert(&self, key: Hash32, value: V, bytes: u64, ttl: Duration, now: Instant) -> bool {
        if bytes > self.shard_max {
            return false;
        }
        let mut s = self.shard(&key);
        s.remove(&key);
        while s.bytes + bytes > self.shard_max {
            let Some((_, victim)) = s.order.pop_first() else {
                break;
            };
            if let Some(slot) = s.map.remove(&victim) {
                s.bytes -= slot.bytes;
                if slot.expires > now {
                    self.evictions.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        s.tick += 1;
        let tick = s.tick;
        s.order.insert(tick, key);
        s.bytes += bytes;
        s.map.insert(
            key,
            Slot {
                value,
                bytes,
                tick,
                expires: now + ttl,
            },
        );
        true
    }

    /// Entfernt einen Eintrag.
    pub fn remove(&self, key: &Hash32) -> bool {
        self.shard(key).remove(key)
    }

    /// Entfernt alle abgelaufenen Einträge; liefert die Anzahl.
    pub fn purge_expired(&self, now: Instant) -> usize {
        let mut n = 0;
        for shard in &*self.shards {
            let mut s = shard.lock().unwrap_or_else(PoisonError::into_inner);
            let dead: Vec<Hash32> = s
                .map
                .iter()
                .filter(|(_, slot)| slot.expires <= now)
                .map(|(k, _)| *k)
                .collect();
            for k in dead {
                s.remove(&k);
                n += 1;
            }
        }
        n
    }

    /// Belegte Bytes.
    pub fn bytes(&self) -> u64 {
        self.shards
            .iter()
            .map(|s| s.lock().unwrap_or_else(PoisonError::into_inner).bytes)
            .sum()
    }

    /// Anzahl Einträge.
    pub fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|s| s.lock().unwrap_or_else(PoisonError::into_inner).map.len())
            .sum()
    }

    /// Leer?
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Wegen der Byte-Grenze verdrängte (nicht abgelaufene) Einträge.
    pub fn evictions(&self) -> u64 {
        self.evictions.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(i: u8, shard: u8) -> Hash32 {
        let mut b = [0u8; 32];
        b[0] = shard;
        b[1] = i;
        Hash32(b)
    }

    #[test]
    fn lru_byte_limit_and_ttl() {
        let lru = MemoryLru::<u32>::new(16 * 100); // 100 Bytes je Shard
        let now = Instant::now();
        let ttl = Duration::from_secs(60);
        assert!(lru.insert(k(1, 0), 1, 40, ttl, now));
        assert!(lru.insert(k(2, 0), 2, 40, ttl, now));
        assert_eq!(lru.get(&k(1, 0), now), Some(1)); // 1 jetzt jünger als 2
        assert!(lru.insert(k(3, 0), 3, 40, ttl, now)); // verdrängt 2
        assert_eq!(lru.get(&k(2, 0), now), None);
        assert_eq!(lru.get(&k(1, 0), now), Some(1));
        assert_eq!(lru.get(&k(3, 0), now), Some(3));
        assert_eq!(lru.evictions(), 1);
        assert_eq!(lru.bytes(), 80);
        // Anderer Shard unberührt.
        assert!(lru.insert(k(1, 1), 9, 100, ttl, now));
        assert!(!lru.insert(k(2, 1), 9, 101, ttl, now));
        // Ersetzen zählt Bytes neu.
        assert!(lru.insert(k(1, 0), 10, 10, ttl, now));
        assert_eq!(lru.get(&k(1, 0), now), Some(10));
        assert_eq!(lru.bytes(), 150);
        // TTL.
        assert!(lru.insert(k(5, 2), 5, 1, Duration::from_millis(10), now));
        assert_eq!(lru.get(&k(5, 2), now + Duration::from_millis(20)), None);
        assert!(lru.insert(k(6, 2), 6, 1, Duration::from_millis(10), now));
        assert_eq!(lru.purge_expired(now + Duration::from_secs(1)), 1);
        assert_eq!(lru.len(), 3);
    }
}
