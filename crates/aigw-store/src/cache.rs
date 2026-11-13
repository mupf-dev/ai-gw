// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Ablage des semantischen Caches für den Warmstart (AIGW-40, Tabelle `cache_vector`).
//!
//! Nur das Backend `memory` (eigener HNSW im RAM) nutzt diese Tabelle: Einträge werden
//! asynchron geschrieben und beim Start geladen. Vektor und Antwort liegen gemeinsam
//! verschlüsselt (AES-256-GCM mit dem Master-Key, AAD = `id`); der Aufrufer liefert bzw.
//! erhält das Chiffrat ([`crate::crypto::Sealed`]).

use crate::crypto::Sealed;
use crate::db::{Q, Val};
use crate::{Store, StoreError};

/// Eine Zeile von `cache_vector`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheVectorRow {
    /// BLAKE3 (hex) des exakten Cache-Schlüssels.
    pub id: String,
    /// Partition (hex): Scope, Modellgruppe, Systemprompt/Tools …
    pub partition_key: String,
    /// Länge des Vektors.
    pub dims: i64,
    /// Unix-Millisekunden.
    pub created_at: i64,
    /// Unix-Millisekunden.
    pub expires_at: i64,
    /// Verschlüsselter Vektor ‖ Antwort.
    pub sealed: Sealed,
}

impl Store {
    /// Schreibt bzw. ersetzt Einträge (eine Transaktion).
    pub async fn cache_vectors_upsert(&self, rows: &[CacheVectorRow]) -> Result<(), StoreError> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        for r in rows {
            let (nonce, ciphertext) = r.sealed.to_bytes()?;
            Q::new(
                "INSERT INTO cache_vector \
                 (id, partition_key, dims, created_at, expires_at, key_id, nonce, ciphertext) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                 ON CONFLICT (id) DO UPDATE SET partition_key = excluded.partition_key, \
                 dims = excluded.dims, created_at = excluded.created_at, \
                 expires_at = excluded.expires_at, key_id = excluded.key_id, \
                 nonce = excluded.nonce, ciphertext = excluded.ciphertext",
            )
            .bind(Val::text(r.id.clone()))
            .bind(Val::text(r.partition_key.clone()))
            .bind(Val::int(r.dims))
            .bind(Val::ms(r.created_at))
            .bind(Val::ms(r.expires_at))
            .bind(Val::text(r.sealed.key_id.clone()))
            .bind(Val::Bytes(Some(nonce)))
            .bind(Val::Bytes(Some(ciphertext)))
            .execute(&mut tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Löscht Einträge nach ID.
    pub async fn cache_vectors_delete(&self, ids: &[String]) -> Result<(), StoreError> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        for id in ids {
            Q::new("DELETE FROM cache_vector WHERE id = $1")
                .bind(Val::text(id.clone()))
                .execute(&mut tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Löscht abgelaufene Einträge; liefert die Anzahl.
    pub async fn cache_vectors_purge(&self, now_ms: i64) -> Result<u64, StoreError> {
        Ok(Q::new("DELETE FROM cache_vector WHERE expires_at <= $1")
            .bind(Val::ms(now_ms))
            .execute(&self.pool)
            .await?)
    }

    /// Lädt nicht abgelaufene Einträge, jüngste zuerst, höchstens `limit`.
    pub async fn cache_vectors_load(
        &self,
        now_ms: i64,
        limit: i64,
    ) -> Result<Vec<CacheVectorRow>, StoreError> {
        let rows = Q::new(
            "SELECT id, partition_key, dims, created_at, expires_at, key_id, nonce, ciphertext \
             FROM cache_vector WHERE expires_at > $1 ORDER BY created_at DESC LIMIT $2",
        )
        .bind(Val::ms(now_ms))
        .bind(Val::int(limit))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(CacheVectorRow {
                    id: row.text("id")?,
                    partition_key: row.text("partition_key")?,
                    dims: row.i64("dims")?,
                    created_at: row.ms("created_at")?,
                    expires_at: row.ms("expires_at")?,
                    sealed: Sealed::from_bytes(
                        row.text("key_id")?,
                        &row.bytes("nonce")?,
                        &row.bytes("ciphertext")?,
                    ),
                })
            })
            .collect()
    }
}
