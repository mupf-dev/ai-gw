// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Persistenz von ai-gw (AIGW-9, AIGW-73).
//!
//! SQLite ist Standard, Postgres optional für HA (E4). Im Hot Path wird nicht auf die Datenbank
//! zugegriffen; sie ist Quelle der Konfiguration (→ [`load_snapshot`]) und Ziel gebündelter Logs.
//!
//! Seit AIGW-73 (F7) laufen beide Datenbanken über **eigene sqlx-Treiber mit nativen Typen**
//! (Postgres: `UUID`, `JSONB`, `TIMESTAMPTZ`, `BOOLEAN`, `BYTEA`) und je einem Migrationssatz
//! (`migrations/sqlite/`, `migrations/postgres/`). Der Zugriff ist per Enum-Dispatch gekapselt
//! (`db`-Modul): [`Store`] bleibt ein konkreter Typ mit unveränderter Schnittstelle.
//! Details: `docs/konzept/02-datenmodell.md`.

pub mod admin_auth;
pub mod analytics;
pub mod budget;
pub mod cache;
pub mod crypto;
pub(crate) mod db;
pub mod events;
pub mod gitops;
pub mod keys;
pub mod observe;
pub mod reload;
pub mod repo;
mod snapshot;
pub mod snapshot_cache;
pub mod tools;
pub mod wasm;

use std::str::FromStr as _;
use std::sync::Arc;
use std::time::Duration;

use aigw_core::config::StoreConfig;
use aigw_core::resource::ResourceKind;
use sqlx::SqlSafeStr as _;
use sqlx::migrate::{MigrateError, Migration, MigrationType, Migrator};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tokio::sync::watch;

use crate::db::{Pool, Q};

pub use crypto::{Keyring, MasterKey};
pub use keys::{generate_api_key, hash_api_key};
pub use reload::{ReloadMetrics, ReloadOptions, Reloader, ReloaderTask};
pub use repo::audit::{
    ActorType, AuditCursor, AuditEvent, AuditPage, AuditQuery, AuditScope, ScopeRule,
};
pub use repo::audit_chain::{AUDIT_LOG_KIND, AuditChainIssue, AuditChainReport};
pub use repo::list::{ListFilter, ListQuery, Page};
pub use repo::secret::validate_secret;
pub use repo::{AuditEntry, Expect, PlainResource, Resource, WriteExtras};
pub use snapshot::{Fallback, Incremental, load_incremental, load_parts, load_snapshot};
pub use snapshot_cache::{CacheError, CachedSnapshot, SnapshotCache};

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
    /// Erwarteter Stand passt nicht (optimistische Sperre, `If-Match`, F38): das Objekt wurde
    /// zwischen Lesen und Schreiben geändert – auch von einer anderen Instanz.
    #[error("{kind} {name:?} wurde zwischenzeitlich geändert")]
    PreconditionFailed {
        /// Art.
        kind: String,
        /// Name.
        name: String,
    },
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
    Migrate(#[from] MigrateError),
    /// Datenbank mit einem Schema, das dieser Stand nicht kennt (z. B. vor AIGW-73).
    #[error("unbekannter Schemastand: {0}")]
    IncompatibleSchema(String),
    /// Fehler des Datenbanktreibers.
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

impl StoreError {
    /// Datenbank nicht erreichbar bzw. Abfrage fehlgeschlagen (nicht: ungültige Daten,
    /// fremdes Schema, Master-Key)? Grundlage für den Kaltstart aus dem Snapshot-Cache (F14).
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Db(_) | Self::Migrate(MigrateError::Execute(_)))
    }
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
    pub(crate) pool: Pool,
    /// Zuletzt in diesem Prozess committete `config_version` (Auslöser für den Reloader).
    commits: Arc<watch::Sender<i64>>,
}

/// Migrationssatz für SQLite (`migrations/sqlite/`): (Version, Beschreibung, SQL).
const SQLITE_MIGRATIONS: &[(i64, &str, &str)] = &[
    (
        1,
        "init",
        include_str!("../migrations/sqlite/0001_init.sql"),
    ),
    (
        2,
        "snapshot_journal",
        include_str!("../migrations/sqlite/0002_snapshot_journal.sql"),
    ),
    (
        3,
        "admin_auth",
        include_str!("../migrations/sqlite/0003_admin_auth.sql"),
    ),
    (
        4,
        "observe",
        include_str!("../migrations/sqlite/0004_observe.sql"),
    ),
    (
        5,
        "restpunkte",
        include_str!("../migrations/sqlite/0005_restpunkte.sql"),
    ),
    (
        6,
        "payload_policy",
        include_str!("../migrations/sqlite/0006_payload_policy.sql"),
    ),
    (
        7,
        "credential_auth",
        include_str!("../migrations/sqlite/0007_credential_auth.sql"),
    ),
    (
        8,
        "tools",
        include_str!("../migrations/sqlite/0008_tools.sql"),
    ),
    (
        9,
        "wasm_modules",
        include_str!("../migrations/sqlite/0009_wasm_modules.sql"),
    ),
    (
        10,
        "cache_aad",
        include_str!("../migrations/sqlite/0010_cache_aad.sql"),
    ),
    (
        11,
        "wasm_signatures",
        include_str!("../migrations/sqlite/0011_wasm_signatures.sql"),
    ),
    (
        12,
        "workload_identities",
        include_str!("../migrations/sqlite/0012_workload_identities.sql"),
    ),
    (
        13,
        "cost_breakdown",
        include_str!("../migrations/sqlite/0013_cost_breakdown.sql"),
    ),
    (
        14,
        "wasm_approval",
        include_str!("../migrations/sqlite/0014_wasm_approval.sql"),
    ),
    (
        15,
        "deployment_budget",
        include_str!("../migrations/sqlite/0015_deployment_budget.sql"),
    ),
];

/// Migrationssatz für Postgres (`migrations/postgres/`), gleiche Versionen wie SQLite.
const POSTGRES_MIGRATIONS: &[(i64, &str, &str)] = &[
    (
        1,
        "init",
        include_str!("../migrations/postgres/0001_init.sql"),
    ),
    (
        2,
        "snapshot_journal",
        include_str!("../migrations/postgres/0002_snapshot_journal.sql"),
    ),
    (
        3,
        "admin_auth",
        include_str!("../migrations/postgres/0003_admin_auth.sql"),
    ),
    (
        4,
        "observe",
        include_str!("../migrations/postgres/0004_observe.sql"),
    ),
    (
        5,
        "restpunkte",
        include_str!("../migrations/postgres/0005_restpunkte.sql"),
    ),
    (
        6,
        "payload_policy",
        include_str!("../migrations/postgres/0006_payload_policy.sql"),
    ),
    (
        7,
        "credential_auth",
        include_str!("../migrations/postgres/0007_credential_auth.sql"),
    ),
    (
        8,
        "tools",
        include_str!("../migrations/postgres/0008_tools.sql"),
    ),
    (
        9,
        "wasm_modules",
        include_str!("../migrations/postgres/0009_wasm_modules.sql"),
    ),
    (
        10,
        "cache_aad",
        include_str!("../migrations/postgres/0010_cache_aad.sql"),
    ),
    (
        11,
        "wasm_signatures",
        include_str!("../migrations/postgres/0011_wasm_signatures.sql"),
    ),
    (
        12,
        "workload_identities",
        include_str!("../migrations/postgres/0012_workload_identities.sql"),
    ),
    (
        13,
        "cost_breakdown",
        include_str!("../migrations/postgres/0013_cost_breakdown.sql"),
    ),
    (
        14,
        "wasm_approval",
        include_str!("../migrations/postgres/0014_wasm_approval.sql"),
    ),
    (
        15,
        "deployment_budget",
        include_str!("../migrations/postgres/0015_deployment_budget.sql"),
    ),
];

/// Eingebettete Migrationen des Backends.
pub fn migrator(backend: Backend) -> Migrator {
    let set = match backend {
        Backend::Sqlite => SQLITE_MIGRATIONS,
        Backend::Postgres => POSTGRES_MIGRATIONS,
    };
    Migrator::with_migrations(
        set.iter()
            .map(|(v, name, sql)| {
                Migration::new(
                    *v,
                    (*name).into(),
                    MigrationType::Simple,
                    sql.into_sql_str(),
                    false,
                )
            })
            .collect(),
    )
}

/// Verständlicher Fehler für Datenbanken mit älterem bzw. fremdem Schemastand.
fn migrate_error(e: MigrateError) -> StoreError {
    match e {
        MigrateError::VersionMismatch(v) | MigrateError::VersionMissing(v) => {
            StoreError::IncompatibleSchema(format!(
                "Migration {v} der Datenbank passt nicht zu diesem Stand. Datenbanken aus der Zeit \
                 vor AIGW-73 (gemeinsamer Migrationssatz 0001–0007) werden nicht übernommen – \
                 neu anlegen und die Konfiguration per `aigw config export`/`apply` übertragen"
            ))
        }
        other => StoreError::Migrate(other),
    }
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
        Self::open(cfg, false).await
    }

    /// Wie [`Store::connect_with`], verbindet aber erst bei der ersten Abfrage (Kaltstart aus
    /// dem Snapshot-Cache bei nicht erreichbarer Datenbank, F14). Fehler zeigen sich dann an
    /// den einzelnen Abfragen.
    pub async fn connect_lazy_with(cfg: &StoreConfig) -> Result<Self, StoreError> {
        Self::open(cfg, true).await
    }

    async fn open(cfg: &StoreConfig, lazy: bool) -> Result<Self, StoreError> {
        let backend = Backend::from_url(&cfg.url)?;
        let max = cfg.max_connections.max(1);
        let pool = match backend {
            Backend::Sqlite => {
                let in_memory = cfg.url.contains(":memory:") || cfg.url.contains("mode=memory");
                // Fremdschlüssel sind bei sqlx standardmäßig an (Löschregeln, F3).
                let opts = SqliteConnectOptions::from_str(&cfg.url)?.foreign_keys(true);
                let mut pool = SqlitePoolOptions::new().max_connections(max);
                if in_memory {
                    // Jede SQLite-Verbindung hätte sonst ihre eigene leere In-Memory-Datenbank.
                    pool = pool
                        .max_connections(1)
                        .min_connections(1)
                        .idle_timeout(None)
                        .max_lifetime(None);
                } else {
                    pool = pool.acquire_timeout(Duration::from_secs(10));
                }
                Pool::Sqlite(if lazy {
                    pool.connect_lazy_with(opts)
                } else {
                    pool.connect_with(opts).await?
                })
            }
            Backend::Postgres => {
                let opts = PgConnectOptions::from_str(&cfg.url)?;
                let pool = PgPoolOptions::new()
                    .max_connections(max)
                    .acquire_timeout(Duration::from_secs(10));
                Pool::Postgres(if lazy {
                    pool.connect_lazy_with(opts)
                } else {
                    pool.connect_with(opts).await?
                })
            }
        };
        Ok(Self {
            pool,
            commits: Arc::new(watch::Sender::new(0)),
        })
    }

    /// Wendet ausstehende Migrationen an (Satz des jeweiligen Backends).
    pub async fn migrate(&self) -> Result<(), StoreError> {
        let m = migrator(self.backend());
        match &self.pool {
            Pool::Sqlite(p) => m.run(p).await,
            Pool::Postgres(p) => m.run(p).await,
        }
        .map_err(migrate_error)
    }

    /// Einfache Erreichbarkeitsprüfung (`SELECT 1`).
    pub async fn ping(&self) -> Result<(), StoreError> {
        Q::new("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    /// Verwendetes Backend.
    pub fn backend(&self) -> Backend {
        self.pool.backend()
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
