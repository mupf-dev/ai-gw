// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Konfigurationsmodell (Platzhalter).
//!
//! Enthält zunächst nur die Startkonfiguration (Listen-Adresse, Datenbank, Zähler-Backend).
//! Modelle, Routen, Keys usw. folgen mit dem Architekturkonzept.

use std::net::SocketAddr;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Gesamtkonfiguration des Gateways.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// HTTP-Server.
    pub server: ServerConfig,
    /// Persistenz.
    pub store: StoreConfig,
    /// Zähler-Backend für Limits und Budgets.
    pub counters: CountersConfig,
}

impl Config {
    /// Lädt die Konfiguration aus einer TOML-Datei.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::ConfigRead {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml(&text)
    }

    /// Parst die Konfiguration aus TOML-Text.
    pub fn from_toml(text: &str) -> Result<Self> {
        toml::from_str(text).map_err(|e| Error::ConfigInvalid(e.to_string()))
    }
}

/// Einstellungen des HTTP-Servers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Adresse, auf der Gateway und Admin-API lauschen.
    pub listen: SocketAddr,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 4000)),
        }
    }
}

/// Einstellungen der Persistenz.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StoreConfig {
    /// Datenbank-URL, `sqlite:` (Standard) oder `postgres:`.
    pub url: String,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            url: "sqlite://aigw.db?mode=rwc".to_owned(),
        }
    }
}

/// Zähler-Backend (E4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CounterBackendKind {
    /// Nur im Prozess (eine Instanz).
    #[default]
    Local,
    /// Lokal zählen, alle 1–2 s mit Postgres abgleichen (unscharf).
    PostgresSync,
    /// Exakt und verteilt über Redis.
    Redis,
}

/// Einstellungen der Zähler.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CountersConfig {
    /// Gewähltes Backend.
    pub backend: CounterBackendKind,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_partial_config_with_defaults() {
        let cfg = Config::from_toml(
            r#"
            [server]
            listen = "0.0.0.0:8080"
            [counters]
            backend = "postgres-sync"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.server.listen.port(), 8080);
        assert_eq!(cfg.counters.backend, CounterBackendKind::PostgresSync);
        assert_eq!(cfg.store, StoreConfig::default());
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(Config::from_toml("[server]\nlisten_typo = 1\n").is_err());
    }
}
