// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Ablage des semantischen Caches für den Warmstart (AIGW-40, Tabelle `cache_vector`).
//!
//! Nur das Backend `memory` (eigener HNSW im RAM) nutzt diese Tabelle: Einträge werden
//! asynchron geschrieben und beim Start geladen. Vektor und Antwort liegen gemeinsam
//! verschlüsselt (AES-256-GCM mit dem Master-Key, AAD = `id`); der Aufrufer liefert bzw.
//! erhält das Chiffrat ([`crate::crypto::Sealed`]).

use sqlx::Row as _;

use crate::crypto::Sealed;
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
            sqlx::query(
                "INSERT INTO cache_vector \
                 (id, partition_key, dims, created_at, expires_at, key_id, nonce, ciphertext) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                 ON CONFLICT (id) DO UPDATE SET partition_key = excluded.partition_key, \
                 dims = excluded.dims, created_at = excluded.created_at, \
                 expires_at = excluded.expires_at, key_id = excluded.key_id, \
                 nonce = excluded.nonce, ciphertext = excluded.ciphertext",
            )
            .bind(&r.id)
            .bind(&r.partition_key)
            .bind(r.dims)
            .bind(r.created_at)
            .bind(r.expires_at)
            .bind(&r.sealed.key_id)
            .bind(&r.sealed.nonce)
            .bind(&r.sealed.ciphertext)
            .execute(&mut *tx)
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
            sqlx::query("DELETE FROM cache_vector WHERE id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Löscht abgelaufene Einträge; liefert die Anzahl.
    pub async fn cache_vectors_purge(&self, now_ms: i64) -> Result<u64, StoreError> {
        let r = sqlx::query("DELETE FROM cache_vector WHERE expires_at <= $1")
            .bind(now_ms)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }

    /// Lädt nicht abgelaufene Einträge, jüngste zuerst, höchstens `limit`.
    pub async fn cache_vectors_load(
        &self,
        now_ms: i64,
        limit: i64,
    ) -> Result<Vec<CacheVectorRow>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, partition_key, dims, created_at, expires_at, key_id, nonce, ciphertext \
             FROM cache_vector WHERE expires_at > $1 ORDER BY created_at DESC LIMIT $2",
        )
        .bind(now_ms)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(CacheVectorRow {
                    id: row.try_get("id")?,
                    partition_key: row.try_get("partition_key")?,
                    dims: row.try_get("dims")?,
                    created_at: row.try_get("created_at")?,
                    expires_at: row.try_get("expires_at")?,
                    sealed: Sealed {
                        key_id: row.try_get("key_id")?,
                        nonce: row.try_get("nonce")?,
                        ciphertext: row.try_get("ciphertext")?,
                    },
                })
            })
            .collect()
    }
}
