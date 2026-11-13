// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Deduplizierung von Ereignis-Webhooks im Cluster (AIGW-61, Tabelle `event_dedup`,
//! Migration 0006): [`EventDedup`] für SQLite und Postgres.
//!
//! Ein Ereignis mit Dedup-Schlüssel (z. B. Budget-Warnung je Zeitraum und Schwelle) wird nur
//! von der Instanz gesendet, die den Schlüssel zuerst anlegt (`INSERT … ON CONFLICT DO
//! NOTHING`, Primärschlüssel). Abgelaufene Schlüssel werden vorher gelöscht; Ereignisse sind
//! selten (Budget-Schwellen), der zusätzliche `DELETE` kostet daher nichts Merkliches.

use aigw_webhook::EventDedup;

use crate::db::{Q, Val};
use crate::{Store, StoreError};

impl Store {
    /// Beansprucht `key` bis `expires_at_ms`. `true` = diese Instanz sendet das Ereignis.
    pub async fn claim_event(
        &self,
        key: &str,
        event_type: &str,
        now_ms: i64,
        expires_at_ms: i64,
    ) -> Result<bool, StoreError> {
        Q::new("DELETE FROM event_dedup WHERE expires_at <= $1")
            .bind(Val::ms(now_ms))
            .execute(&self.pool)
            .await?;
        let n = Q::new(
            "INSERT INTO event_dedup (dedup_key, event_type, created_at, expires_at) \
             VALUES ($1, $2, $3, $4) ON CONFLICT (dedup_key) DO NOTHING",
        )
        .bind(Val::text(key))
        .bind(Val::text(event_type))
        .bind(Val::ms(now_ms))
        .bind(Val::ms(expires_at_ms))
        .execute(&self.pool)
        .await?;
        Ok(n == 1)
    }
}

#[async_trait::async_trait]
impl EventDedup for Store {
    async fn claim(
        &self,
        key: &str,
        event_type: &str,
        now_ms: i64,
        expires_at_ms: i64,
    ) -> Result<bool, String> {
        self.claim_event(key, event_type, now_ms, expires_at_ms)
            .await
            .map_err(|e| e.to_string())
    }
}
