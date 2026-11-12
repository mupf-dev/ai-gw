// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Persistenz (Stub).
//!
//! SQLite ist Standard, Postgres optional für HA (E4). Im Hot Path wird nicht auf die Datenbank
//! zugegriffen; sie dient als Quelle der Konfiguration und als Ziel gebündelter Logs.
//! Schema, Migrationen und LISTEN/NOTIFY folgen mit dem Architekturkonzept.

use sqlx::postgres::PgPoolOptions;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::{PgPool, SqlitePool};

/// Fehler der Persistenzschicht.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// Nicht unterstütztes URL-Schema.
    #[error("nicht unterstützte Datenbank-URL (erwartet sqlite: oder postgres:): {0}")]
    UnsupportedUrl(String),
    /// Fehler des Datenbanktreibers.
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Unterstützte Datenbanken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// SQLite (Standard).
    Sqlite,
    /// Postgres (HA).
    Postgres,
}

impl Backend {
    /// Bestimmt das Backend aus einer Datenbank-URL.
    pub fn from_url(url: &str) -> Result<Self, StoreError> {
        if url.starts_with("sqlite:") {
            Ok(Self::Sqlite)
        } else if url.starts_with("postgres:") || url.starts_with("postgresql:") {
            Ok(Self::Postgres)
        } else {
            Err(StoreError::UnsupportedUrl(url.to_owned()))
        }
    }
}

/// Verbindungspool zur gewählten Datenbank.
#[derive(Debug, Clone)]
pub enum Store {
    /// SQLite-Pool.
    Sqlite(SqlitePool),
    /// Postgres-Pool.
    Postgres(PgPool),
}

impl Store {
    /// Verbindet sich mit der Datenbank hinter `url`.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        Ok(match Backend::from_url(url)? {
            Backend::Sqlite => Self::Sqlite(SqlitePoolOptions::new().connect(url).await?),
            Backend::Postgres => Self::Postgres(PgPoolOptions::new().connect(url).await?),
        })
    }

    /// Einfache Erreichbarkeitsprüfung (`SELECT 1`).
    pub async fn ping(&self) -> Result<(), StoreError> {
        match self {
            Self::Sqlite(p) => sqlx::query("SELECT 1").execute(p).await.map(drop)?,
            Self::Postgres(p) => sqlx::query("SELECT 1").execute(p).await.map(drop)?,
        }
        Ok(())
    }

    /// Verwendetes Backend.
    pub fn backend(&self) -> Backend {
        match self {
            Self::Sqlite(_) => Backend::Sqlite,
            Self::Postgres(_) => Backend::Postgres,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_from_url() {
        assert_eq!(
            Backend::from_url("sqlite::memory:").unwrap(),
            Backend::Sqlite
        );
        assert_eq!(
            Backend::from_url("postgres://x/y").unwrap(),
            Backend::Postgres
        );
        assert!(Backend::from_url("mysql://x").is_err());
    }

    #[tokio::test]
    async fn sqlite_in_memory_ping() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        assert_eq!(store.backend(), Backend::Sqlite);
        store.ping().await.unwrap();
    }
}
