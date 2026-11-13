// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Backend `memory`: eigener HNSW-Index ([`crate::hnsw`]) je Partition im RAM.
//!
//! - **Ersetzen/Löschen:** Grabsteine (der Knoten bleibt als Wegpunkt im Graphen, wird bei
//!   der Suche übersprungen). Übersteigen die Grabsteine einer Partition die lebenden Knoten
//!   (und 64), wird ihr Graph aus den lebenden Vektoren neu aufgebaut.
//! - **TTL:** abgelaufene Einträge zählen bei der Suche nicht; [`Evict::Expired`] räumt sie
//!   periodisch ab (Hintergrund-Task des Caches).
//! - **Speichergrenze:** Höchstzahl Einträge und Bytes (Vektor + Kanten + Antwort) über alle
//!   Partitionen; verdrängt wird der älteste Eintrag (FIFO nach Einfügezeit).
//! - **Warmstart:** optional schreibt ein Hintergrund-Task jede Änderung verschlüsselt in die
//!   Datenbank ([`crate::persist`]); beim Start lädt [`MemoryIndex::warm`] sie wieder.
//!
//! Sperren: Partitions-Map (`RwLock`), je Partition ein `RwLock` (Suchen parallel), FIFO
//! (`Mutex`). Nie werden Partition und FIFO gleichzeitig gehalten.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::mpsc;

use super::{Evict, IndexError, VectorEntry, VectorHit, VectorIndex, VectorQuery, now_ms};
use crate::hnsw::{Hnsw, HnswParams};
use crate::key::Hash32;
use crate::persist::PersistOp;

/// Einstellungen.
#[derive(Debug, Clone, Copy)]
pub struct MemoryOptions {
    /// HNSW-Parameter.
    pub hnsw: HnswParams,
    /// Feste Dimension (sonst aus dem ersten Vektor).
    pub dims: Option<usize>,
    /// Höchstzahl Einträge.
    pub max_entries: u64,
    /// Obergrenze in Bytes.
    pub max_bytes: u64,
}

impl Default for MemoryOptions {
    fn default() -> Self {
        Self {
            hnsw: HnswParams::default(),
            dims: None,
            max_entries: 100_000,
            max_bytes: 512 * 1024 * 1024,
        }
    }
}

#[derive(Debug)]
struct Node {
    id: Hash32,
    payload: Bytes,
    expires_ms: i64,
    seq: u64,
    bytes: u64,
    deleted: bool,
}

#[derive(Debug)]
struct Part {
    hnsw: Hnsw,
    nodes: Vec<Node>,
    by_id: HashMap<Hash32, u32>,
    deleted: usize,
}

impl Part {
    fn live(&self) -> usize {
        self.nodes.len() - self.deleted
    }

    /// Markiert Knoten `i` als gelöscht; liefert die freigegebenen Bytes.
    fn tombstone(&mut self, i: u32) -> Option<u64> {
        let n = self.nodes.get_mut(i as usize)?;
        if n.deleted {
            return None;
        }
        n.deleted = true;
        n.payload = Bytes::new();
        if self.by_id.get(&n.id) == Some(&i) {
            self.by_id.remove(&n.id);
        }
        self.deleted += 1;
        Some(n.bytes)
    }

    fn maybe_rebuild(&mut self) {
        if self.deleted <= 64 || self.deleted <= self.live() {
            return;
        }
        let mut hnsw = Hnsw::new(self.hnsw.dim(), self.hnsw.params());
        let mut nodes = Vec::with_capacity(self.live());
        let mut by_id = HashMap::with_capacity(self.live());
        for (i, n) in std::mem::take(&mut self.nodes).into_iter().enumerate() {
            if n.deleted {
                continue;
            }
            let Some(idx) = u32::try_from(i).ok() else {
                continue;
            };
            if let Some(new) = hnsw.insert(self.hnsw.vector(idx)) {
                by_id.insert(n.id, new);
                nodes.push(n);
            }
        }
        self.hnsw = hnsw;
        self.nodes = nodes;
        self.by_id = by_id;
        self.deleted = 0;
    }
}

/// HNSW im RAM je Partition.
pub struct MemoryIndex {
    opts: MemoryOptions,
    dim: Mutex<Option<usize>>,
    parts: RwLock<HashMap<Hash32, Arc<RwLock<Part>>>>,
    fifo: Mutex<BTreeMap<u64, (Hash32, Hash32)>>,
    seq: AtomicU64,
    entries: AtomicU64,
    bytes: AtomicU64,
    evicted: AtomicU64,
    persist: Option<mpsc::Sender<PersistOp>>,
    persist_dropped: AtomicU64,
}

impl std::fmt::Debug for MemoryIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryIndex")
            .field("opts", &self.opts)
            .field("entries", &self.entries.load(Ordering::Relaxed))
            .field("bytes", &self.bytes.load(Ordering::Relaxed))
            .field("persist", &self.persist.is_some())
            .finish_non_exhaustive()
    }
}

impl MemoryIndex {
    /// Leerer Index.
    pub fn new(opts: MemoryOptions) -> Self {
        Self {
            opts,
            dim: Mutex::new(opts.dims),
            parts: RwLock::new(HashMap::new()),
            fifo: Mutex::new(BTreeMap::new()),
            seq: AtomicU64::new(0),
            entries: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            evicted: AtomicU64::new(0),
            persist: None,
            persist_dropped: AtomicU64::new(0),
        }
    }

    /// Schreibt Änderungen über `tx` in die Datenbank (Warmstart, [`crate::persist`]).
    #[must_use]
    pub fn with_persist(mut self, tx: mpsc::Sender<PersistOp>) -> Self {
        self.persist = Some(tx);
        self
    }

    /// Lädt Einträge (Warmstart) ohne sie erneut zu persistieren. Liefert die Anzahl.
    pub fn warm(&self, entries: impl IntoIterator<Item = VectorEntry>) -> usize {
        let now = now_ms();
        let mut n = 0;
        for e in entries {
            if e.expires_at_ms > now && self.insert_sync(e, false).is_ok() {
                n += 1;
            }
        }
        n
    }

    /// Belegte Bytes (Schätzung).
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// Wegen der Grenzen verdrängte Einträge.
    pub fn evicted(&self) -> u64 {
        self.evicted.load(Ordering::Relaxed)
    }

    /// Verworfene Persistenz-Aufträge (Warteschlange voll).
    pub fn persist_dropped(&self) -> u64 {
        self.persist_dropped.load(Ordering::Relaxed)
    }

    /// Anzahl Partitionen.
    pub fn partitions(&self) -> usize {
        self.parts
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    fn send(&self, op: PersistOp) {
        if let Some(tx) = &self.persist
            && tx.try_send(op).is_err()
        {
            self.persist_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn part(&self, p: &Hash32) -> Option<Arc<RwLock<Part>>> {
        self.parts
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(p)
            .cloned()
    }

    fn part_or_create(&self, p: Hash32, dim: usize) -> Arc<RwLock<Part>> {
        if let Some(part) = self.part(&p) {
            return part;
        }
        let mut map = self.parts.write().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(map.entry(p).or_insert_with(|| {
            Arc::new(RwLock::new(Part {
                hnsw: Hnsw::new(dim, self.opts.hnsw),
                nodes: Vec::new(),
                by_id: HashMap::new(),
                deleted: 0,
            }))
        }))
    }

    fn release(&self, freed: u64) {
        self.entries.fetch_sub(1, Ordering::Relaxed);
        self.bytes.fetch_sub(freed, Ordering::Relaxed);
    }

    fn insert_sync(&self, e: VectorEntry, persist: bool) -> Result<(), IndexError> {
        let dim = {
            let mut d = self.dim.lock().unwrap_or_else(PoisonError::into_inner);
            *d.get_or_insert(e.vector.len())
        };
        if e.vector.len() != dim {
            return Err(IndexError::InvalidVector(format!(
                "Dimension {} statt {dim}",
                e.vector.len()
            )));
        }
        let part = self.part_or_create(e.partition, dim);
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let replaced_seq;
        {
            let mut p = part.write().unwrap_or_else(PoisonError::into_inner);
            let bytes = (p.hnsw.node_bytes() + e.payload.len() + 96) as u64;
            let Some(idx) = p.hnsw.insert(&e.vector) else {
                return Err(IndexError::InvalidVector("NaN, ∞ oder Nullvektor".into()));
            };
            replaced_seq = match p.by_id.get(&e.id).copied() {
                Some(old) => {
                    let s = p.nodes.get(old as usize).map(|n| n.seq);
                    if let Some(freed) = p.tombstone(old) {
                        self.release(freed);
                    }
                    s
                }
                None => None,
            };
            p.nodes.push(Node {
                id: e.id,
                payload: e.payload.clone(),
                expires_ms: e.expires_at_ms,
                seq,
                bytes,
                deleted: false,
            });
            p.by_id.insert(e.id, idx);
            self.entries.fetch_add(1, Ordering::Relaxed);
            self.bytes.fetch_add(bytes, Ordering::Relaxed);
            p.maybe_rebuild();
        }
        {
            let mut fifo = self.fifo.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(s) = replaced_seq {
                fifo.remove(&s);
            }
            fifo.insert(seq, (e.partition, e.id));
        }
        if persist {
            self.send(PersistOp::Upsert(e));
        }
        self.enforce_limits();
        Ok(())
    }

    fn enforce_limits(&self) {
        while self.entries.load(Ordering::Relaxed) > self.opts.max_entries
            || self.bytes.load(Ordering::Relaxed) > self.opts.max_bytes
        {
            let victim = self
                .fifo
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_first();
            let Some((seq, (pk, id))) = victim else {
                break;
            };
            if self.remove_entry(&pk, &id, Some(seq)) {
                self.evicted.fetch_add(1, Ordering::Relaxed);
                self.send(PersistOp::Delete(id));
            }
        }
    }

    /// Entfernt einen Eintrag (nur, wenn `seq` passt bzw. `None`).
    fn remove_entry(&self, pk: &Hash32, id: &Hash32, seq: Option<u64>) -> bool {
        let Some(part) = self.part(pk) else {
            return false;
        };
        let (removed, empty) = {
            let mut p = part.write().unwrap_or_else(PoisonError::into_inner);
            let removed = match p.by_id.get(id).copied() {
                Some(i) if seq.is_none_or(|s| p.nodes.get(i as usize).map(|n| n.seq) == Some(s)) => {
                    match p.tombstone(i) {
                        Some(freed) => {
                            self.release(freed);
                            true
                        }
                        None => false,
                    }
                }
                _ => false,
            };
            p.maybe_rebuild();
            (removed, p.live() == 0)
        };
        if empty {
            self.drop_partition_if_empty(pk);
        }
        removed
    }

    fn drop_partition_if_empty(&self, pk: &Hash32) {
        let mut map = self.parts.write().unwrap_or_else(PoisonError::into_inner);
        let empty = map
            .get(pk)
            .is_some_and(|p| p.read().unwrap_or_else(PoisonError::into_inner).live() == 0);
        if empty {
            map.remove(pk);
        }
    }

    fn evict_expired(&self) -> u64 {
        let now = now_ms();
        let parts: Vec<(Hash32, Arc<RwLock<Part>>)> = self
            .parts
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(k, v)| (*k, Arc::clone(v)))
            .collect();
        let mut gone = Vec::new();
        let mut n = 0;
        for (pk, part) in parts {
            let mut p = part.write().unwrap_or_else(PoisonError::into_inner);
            let expired: Vec<u32> = p
                .nodes
                .iter()
                .enumerate()
                .filter(|(_, x)| !x.deleted && x.expires_ms <= now)
                .filter_map(|(i, _)| u32::try_from(i).ok())
                .collect();
            for i in expired {
                let seq = p.nodes.get(i as usize).map(|x| x.seq);
                if let Some(freed) = p.tombstone(i) {
                    self.release(freed);
                    n += 1;
                    if let Some(s) = seq {
                        gone.push(s);
                    }
                }
            }
            p.maybe_rebuild();
            let empty = p.live() == 0;
            drop(p);
            if empty {
                self.drop_partition_if_empty(&pk);
            }
        }
        let mut fifo = self.fifo.lock().unwrap_or_else(PoisonError::into_inner);
        for s in gone {
            fifo.remove(&s);
        }
        n
    }

    fn search_sync(&self, q: &VectorQuery<'_>) -> Vec<VectorHit> {
        let Some(part) = self.part(&q.partition) else {
            return Vec::new();
        };
        let now = now_ms();
        let p = part.read().unwrap_or_else(PoisonError::into_inner);
        let found = p.hnsw.search(q.vector, q.k, None, |i| {
            p.nodes
                .get(i as usize)
                .is_some_and(|n| !n.deleted && n.expires_ms > now)
        });
        found
            .into_iter()
            .filter(|(_, s)| *s >= q.threshold)
            .filter_map(|(i, score)| {
                let n = p.nodes.get(i as usize)?;
                Some(VectorHit {
                    id: n.id,
                    score,
                    payload: n.payload.clone(),
                })
            })
            .collect()
    }
}

#[async_trait]
impl VectorIndex for MemoryIndex {
    fn name(&self) -> &'static str {
        "memory"
    }

    async fn insert(&self, entry: VectorEntry) -> Result<(), IndexError> {
        self.insert_sync(entry, true)
    }

    async fn search(&self, q: VectorQuery<'_>) -> Result<Vec<VectorHit>, IndexError> {
        Ok(self.search_sync(&q))
    }

    async fn evict(&self, what: Evict) -> Result<u64, IndexError> {
        match what {
            Evict::Expired => {
                let n = self.evict_expired();
                if self.persist.is_some() {
                    self.send(PersistOp::PurgeExpired);
                }
                Ok(n)
            }
            Evict::Id(id) => {
                let parts: Vec<Hash32> = self
                    .parts
                    .read()
                    .unwrap_or_else(PoisonError::into_inner)
                    .keys()
                    .copied()
                    .collect();
                for pk in parts {
                    if self.remove_entry(&pk, &id, None) {
                        self.send(PersistOp::Delete(id));
                        return Ok(1);
                    }
                }
                Ok(0)
            }
            Evict::Partition(pk) => {
                let part = self
                    .parts
                    .write()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&pk);
                let Some(part) = part else {
                    return Ok(0);
                };
                let p = part.read().unwrap_or_else(PoisonError::into_inner);
                let mut n = 0;
                for node in p.nodes.iter().filter(|n| !n.deleted) {
                    self.release(node.bytes);
                    self.send(PersistOp::Delete(node.id));
                    n += 1;
                }
                Ok(n)
            }
        }
    }

    fn len_hint(&self) -> Option<u64> {
        Some(self.entries.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::hnsw::TestRng;

    fn h(i: u64) -> Hash32 {
        let mut b = [0u8; 32];
        b[..8].copy_from_slice(&i.to_le_bytes());
        Hash32(b)
    }

    fn entry(id: u64, part: u64, v: Vec<f32>, ttl_ms: i64) -> VectorEntry {
        VectorEntry {
            id: h(id),
            partition: h(1000 + part),
            vector: v,
            payload: Bytes::from(format!("p{id}")),
            expires_at_ms: now_ms() + ttl_ms,
        }
    }

    fn q(part: u64, v: &[f32], threshold: f32) -> VectorQuery<'_> {
        VectorQuery {
            partition: h(1000 + part),
            vector: v,
            k: 1,
            threshold,
        }
    }

    #[tokio::test]
    async fn insert_search_replace_partition_threshold() {
        let idx = MemoryIndex::new(MemoryOptions::default());
        idx.insert(entry(1, 0, vec![1.0, 0.0, 0.0], 60_000))
            .await
            .unwrap();
        idx.insert(entry(2, 0, vec![0.0, 1.0, 0.0], 60_000))
            .await
            .unwrap();
        idx.insert(entry(3, 1, vec![1.0, 0.0, 0.0], 60_000))
            .await
            .unwrap();
        let hits = idx.search(q(0, &[0.9, 0.1, 0.0], 0.9)).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, h(1));
        assert_eq!(hits[0].payload, Bytes::from("p1"));
        // Schwelle.
        assert!(idx.search(q(0, &[0.5, 0.5, 0.7], 0.9)).await.unwrap().is_empty());
        // Partition ist harter Filter.
        assert!(idx.search(q(2, &[1.0, 0.0, 0.0], 0.1)).await.unwrap().is_empty());
        // Ersetzen: gleiche ID, neuer Vektor.
        idx.insert(entry(1, 0, vec![0.0, 0.0, 1.0], 60_000))
            .await
            .unwrap();
        let hits = idx.search(q(0, &[1.0, 0.0, 0.0], 0.9)).await.unwrap();
        assert!(hits.is_empty(), "{hits:?}");
        assert_eq!(idx.len_hint(), Some(3));
        // Löschen.
        assert_eq!(idx.evict(Evict::Id(h(2))).await.unwrap(), 1);
        assert!(idx.search(q(0, &[0.0, 1.0, 0.0], 0.5)).await.unwrap().is_empty());
        assert_eq!(idx.evict(Evict::Partition(h(1001))).await.unwrap(), 1);
        assert_eq!(idx.len_hint(), Some(1));
        // Falsche Dimension.
        assert!(idx.insert(entry(9, 0, vec![1.0; 4], 1000)).await.is_err());
    }

    #[tokio::test]
    async fn ttl_and_expiry_sweep() {
        let idx = MemoryIndex::new(MemoryOptions::default());
        idx.insert(entry(1, 0, vec![1.0, 0.0], -1)).await.unwrap();
        assert!(idx.search(q(0, &[1.0, 0.0], 0.5)).await.unwrap().is_empty());
        assert_eq!(idx.evict(Evict::Expired).await.unwrap(), 1);
        assert_eq!(idx.len_hint(), Some(0));
        assert_eq!(idx.partitions(), 0);
    }

    #[tokio::test]
    async fn limits_evict_oldest_and_rebuild_keeps_results() {
        let idx = MemoryIndex::new(MemoryOptions {
            max_entries: 200,
            ..MemoryOptions::default()
        });
        let mut rng = TestRng::new(3);
        let data = rng.clustered(1000, 16, 8, 0.4);
        for (i, v) in data.iter().enumerate() {
            idx.insert(entry(i as u64, 0, v.clone(), 60_000))
                .await
                .unwrap();
        }
        assert_eq!(idx.len_hint(), Some(200));
        assert_eq!(idx.evicted(), 800);
        // Die jüngsten 200 sind exakt auffindbar, die ältesten nicht mehr.
        for (i, v) in data.iter().enumerate().skip(800) {
            let hits = idx.search(q(0, v, 0.999)).await.unwrap();
            assert_eq!(hits.first().map(|x| x.id), Some(h(i as u64)), "{i}");
        }
        let hits = idx.search(q(0, &data[0], 0.9999)).await.unwrap();
        assert!(hits.iter().all(|x| x.id != h(0)));
        // Nach Neuaufbau (Grabsteine > lebende) sind die Bytes nur noch für 200 Einträge.
        let per = idx.bytes() / 200;
        assert!(per > 0 && per < 10_000);
    }
}
