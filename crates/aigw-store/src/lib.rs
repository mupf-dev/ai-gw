// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Persistenz von ai-gw (AIGW-9).
//!
//! SQLite ist Standard, Postgres optional für HA (E4). Im Hot Path wird nicht auf die Datenbank
//! zugegriffen; sie ist Quelle der Konfiguration (→ [`load_snapshot`]) und Ziel gebündelter Logs.
//!
//! Beide Datenbanken laufen über den `Any`-Treiber von sqlx mit **einem** portablen Schema
//! (`migrations/`, nur `TEXT`/`BIGINT`, `$n`-Platzhalter) und damit einem Code-Pfad.
//! Details: `docs/konzept/02-datenmodell.md`.

pub mod crypto;
pub mod keys;
pub mod reload;
pub mod repo;
mod snapshot;

use std::sync::Arc;
use std::time::Duration;

use aigw_core::config::StoreConfig;
use aigw_core::resource::ResourceKind;
use sqlx::any::AnyPoolOptions;
use sqlx::migrate::{Migration, MigrationType, Migrator};
use sqlx::{AnyPool, SqlSafeStr as _};
use tokio::sync::watch;

pub use crypto::{Keyring, MasterKey};
pub use keys::{generate_api_key, hash_api_key};
pub use reload::{ReloadMetrics, ReloadOptions, Reloader, ReloaderTask};
pub use repo::{AuditEntry, PlainResource, Resource};
pub use snapshot::load_snapshot;

/// Fehler der Persistenzschicht.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// Nicht unterstütztes URL-Schema.
    #[error("nicht unterstützte Datenbank-URL (erwartet sqlite: oder postgres:): {0}")]
    UnsupportedUrl(String),
    /// Ressource nicht gefunden.
    #[error("{kind} {name:?} nicht gefunden")]
    NotFound {
        /// Art.
        kind: ResourceKind,
        /// Name oder ID.
        name: String,
    },
    /// Verweis auf eine nicht existierende Ressource.
    #[error("Verweis auf unbekannte Ressource {kind} {name:?}")]
    InvalidReference {
        /// Art des Ziels.
        kind: ResourceKind,
        /// Name des Ziels.
        name: String,
    },
    /// Eindeutigkeit oder Fremdschlüssel verletzt.
    #[error("Konflikt: {0}")]
    Conflict(String),
    /// Inhaltlich ungültige Eingabe.
    #[error("ungültig: {0}")]
    Invalid(String),
    /// Verschlüsselung/Master-Key.
    #[error("Krypto: {0}")]
    Crypto(String),
    /// Unerwarteter Inhalt in der Datenbank.
    #[error("Datenbankinhalt unerwartet: {0}")]
    Corrupt(String),
    /// JSON-(De)Serialisierung.
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// Migration fehlgeschlagen.
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),
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
pub struct Store {
    pub(crate) pool: AnyPool,
    backend: Backend,
    /// Zuletzt in diesem Prozess committete `config_version` (Auslöser für den Reloader).
    commits: Arc<watch::Sender<i64>>,
}

/// Eingebettete Migrationen (ein Satz für SQLite und Postgres).
pub fn migrator() -> Migrator {
    Migrator::with_migrations(vec![Migration::new(
        1,
        "init".into(),
        MigrationType::Simple,
        include_str!("../migrations/0001_init.sql").into_sql_str(),
        false,
    )])
}

impl Store {
    /// Verbindet sich mit der Datenbank hinter `url` (Standard-Poolgröße).
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        Self::connect_with(&StoreConfig {
            url: url.to_owned(),
            ..StoreConfig::default()
        })
        .await
    }

    /// Verbindet sich laut Startkonfiguration.
    pub async fn connect_with(cfg: &StoreConfig) -> Result<Self, StoreError> {
        sqlx::any::install_default_drivers();
        let backend = Backend::from_url(&cfg.url)?;
        let in_memory = backend == Backend::Sqlite
            && (cfg.url.contains(":memory:") || cfg.url.contains("mode=memory"));
        let mut opts = AnyPoolOptions::new().max_connections(cfg.max_connections.max(1));
        if in_memory {
            // Jede SQLite-Verbindung hätte sonst ihre eigene leere In-Memory-Datenbank.
            opts = opts
                .max_connections(1)
                .min_connections(1)
                .idle_timeout(None)
                .max_lifetime(None);
        } else {
            opts = opts.acquire_timeout(Duration::from_secs(10));
        }
        let pool = opts.connect(&cfg.url).await?;
        Ok(Self {
            pool,
            backend,
            commits: Arc::new(watch::Sender::new(0)),
        })
    }

    /// Wendet ausstehende Migrationen an.
    pub async fn migrate(&self) -> Result<(), StoreError> {
        migrator().run(&self.pool).await?;
        Ok(())
    }

    /// Einfache Erreichbarkeitsprüfung (`SELECT 1`).
    pub async fn ping(&self) -> Result<(), StoreError> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    /// Verwendetes Backend.
    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Abonnement auf Commits **dieses Prozesses** (Wert = neue `config_version`).
    ///
    /// Alle Klone eines `Store` teilen den Kanal. Änderungen anderer Instanzen kommen über
    /// [`reload`] (Postgres `LISTEN`, Versionsabfrage).
    pub fn subscribe_commits(&self) -> watch::Receiver<i64> {
        self.commits.subscribe()
    }

    pub(crate) fn notify_committed(&self, version: i64) {
        self.commits.send_if_modified(|v| {
            if version > *v {
                *v = version;
                true
            } else {
                false
            }
        });
    }

    /// Schließt alle Verbindungen.
    pub async fn close(&self) {
        self.pool.close().await;
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
    async fn sqlite_in_memory_ping_and_migrate() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        assert_eq!(store.backend(), Backend::Sqlite);
        store.ping().await.unwrap();
        store.migrate().await.unwrap();
        store.migrate().await.unwrap(); // idempotent
        assert_eq!(store.config_version().await.unwrap(), 0);
    }
}
