// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Warmstart des Backends `memory`: Einträge verschlüsselt in der Datenbank
//! (Tabelle `cache_vector`, Migration 0007).
//!
//! Der Hot Path schreibt nie selbst: [`crate::vector::memory::MemoryIndex`] legt Aufträge in
//! eine begrenzte Warteschlange (voll → verwerfen + zählen), ein Hintergrund-Task schreibt sie
//! gebündelt (bis 256 je Transaktion). Klartext = Länge des Vektors (u32 LE) ‖ Vektor
//! (`f32` LE) ‖ Antwort-JSON; AES-256-GCM mit dem Master-Key, AAD = `aigw-cache:` ‖ ID.

use std::sync::Arc;

use aigw_store::cache::CacheVectorRow;
use aigw_store::{Keyring, Store};
use bytes::Bytes;
use tokio::sync::mpsc;

use crate::key::Hash32;
use crate::vector::{VectorEntry, now_ms, vector_from_le_bytes, vector_to_le_bytes};

/// Auftrag an den Schreib-Task.
#[derive(Debug, Clone)]
pub enum PersistOp {
    /// Eintrag schreiben bzw. ersetzen.
    Upsert(VectorEntry),
    /// Eintrag löschen.
    Delete(Hash32),
    /// Abgelaufene Einträge löschen.
    PurgeExpired,
}

/// Größe der Warteschlange.
pub const QUEUE: usize = 4096;
const BATCH: usize = 256;

fn aad(id: &str) -> Vec<u8> {
    let mut a = b"aigw-cache:".to_vec();
    a.extend_from_slice(id.as_bytes());
    a
}

fn seal(keyring: &Keyring, e: &VectorEntry) -> Option<CacheVectorRow> {
    let id = e.id.to_hex();
    let dims = u32::try_from(e.vector.len()).ok()?;
    let mut plain = Vec::with_capacity(4 + e.vector.len() * 4 + e.payload.len());
    plain.extend_from_slice(&dims.to_le_bytes());
    plain.extend_from_slice(&vector_to_le_bytes(&e.vector));
    plain.extend_from_slice(&e.payload);
    let sealed = keyring.seal(&aad(&id), &plain).ok()?;
    Some(CacheVectorRow {
        id,
        partition_key: e.partition.to_hex(),
        dims: i64::from(dims),
        created_at: now_ms(),
        expires_at: e.expires_at_ms,
        sealed,
    })
}

fn open(keyring: &Keyring, row: &CacheVectorRow) -> Option<VectorEntry> {
    let plain = keyring.open(&aad(&row.id), &row.sealed).ok()?;
    let dims = u32::from_le_bytes(plain.get(..4)?.try_into().ok()?) as usize;
    let end = 4 + dims * 4;
    let vector = vector_from_le_bytes(plain.get(4..end)?)?;
    Some(VectorEntry {
        id: Hash32::from_hex(&row.id)?,
        partition: Hash32::from_hex(&row.partition_key)?,
        vector,
        payload: Bytes::copy_from_slice(plain.get(end..)?),
        expires_at_ms: row.expires_at,
    })
}

/// Lädt bis zu `limit` nicht abgelaufene Einträge (jüngste zuerst). Nicht entschlüsselbare
/// Zeilen (z. B. nach Wechsel des Master-Keys ohne `previous_key_files`) werden übersprungen.
pub async fn load(
    store: &Store,
    keyring: &Keyring,
    limit: u64,
) -> Result<Vec<VectorEntry>, aigw_store::StoreError> {
    let rows = store
        .cache_vectors_load(now_ms(), i64::try_from(limit).unwrap_or(i64::MAX))
        .await?;
    let total = rows.len();
    let out: Vec<VectorEntry> = rows.iter().filter_map(|r| open(keyring, r)).collect();
    if out.len() < total {
        tracing::warn!(
            skipped = total - out.len(),
            "semantischer Cache: Einträge beim Warmstart nicht entschlüsselbar – übersprungen"
        );
    }
    Ok(out)
}

/// Startet den Schreib-Task; liefert den Sender für [`crate::vector::memory::MemoryIndex::with_persist`].
pub fn spawn_writer(
    store: Store,
    keyring: Arc<Keyring>,
) -> (mpsc::Sender<PersistOp>, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<PersistOp>(QUEUE);
    let task = tokio::spawn(async move {
        let mut ops = Vec::with_capacity(BATCH);
        while rx.recv_many(&mut ops, BATCH).await > 0 {
            let mut upserts = Vec::new();
            let mut deletes = Vec::new();
            let mut purge = false;
            for op in ops.drain(..) {
                match op {
                    PersistOp::Upsert(e) => {
                        if let Some(r) = seal(&keyring, &e) {
                            deletes.retain(|d: &String| d != &r.id);
                            upserts.retain(|u: &CacheVectorRow| u.id != r.id);
                            upserts.push(r);
                        }
                    }
                    PersistOp::Delete(id) => {
                        let id = id.to_hex();
                        upserts.retain(|u: &CacheVectorRow| u.id != id);
                        deletes.push(id);
                    }
                    PersistOp::PurgeExpired => purge = true,
                }
            }
            if let Err(e) = store.cache_vectors_upsert(&upserts).await {
                tracing::warn!(error = %e, n = upserts.len(), "semantischer Cache: Schreiben in die Datenbank fehlgeschlagen");
            }
            if let Err(e) = store.cache_vectors_delete(&deletes).await {
                tracing::warn!(error = %e, "semantischer Cache: Löschen in der Datenbank fehlgeschlagen");
            }
            if purge && let Err(e) = store.cache_vectors_purge(now_ms()).await {
                tracing::warn!(error = %e, "semantischer Cache: Aufräumen in der Datenbank fehlgeschlagen");
            }
        }
    });
    (tx, task)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use aigw_store::MasterKey;

    use super::*;

    #[test]
    fn seal_open_roundtrip_and_wrong_key() {
        let (k, _) = MasterKey::generate().unwrap();
        let ring = Keyring::new(k);
        let e = VectorEntry {
            id: Hash32([1; 32]),
            partition: Hash32([2; 32]),
            vector: vec![0.5, -1.25, 3.0],
            payload: Bytes::from_static(b"{\"x\":1}"),
            expires_at_ms: 123,
        };
        let row = seal(&ring, &e).unwrap();
        assert!(!row.sealed.ciphertext.contains("7b22")); // kein Klartext-JSON
        let back = open(&ring, &row).unwrap();
        assert_eq!(back.vector, e.vector);
        assert_eq!(back.payload, e.payload);
        assert_eq!(back.partition, e.partition);
        let (other, _) = MasterKey::generate().unwrap();
        assert!(open(&Keyring::new(other), &row).is_none());
        let mut moved = row.clone();
        moved.id = Hash32([3; 32]).to_hex(); // AAD bindet an die ID
        assert!(open(&ring, &moved).is_none());
    }
}
