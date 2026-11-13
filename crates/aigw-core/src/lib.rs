// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Gemeinsame Grundlagen von ai-gw: Typen, Fehler, Konfigurationsmodell, kanonische IR
//! ([`ir`]) und Wire-Formate mit Konvertern ([`wire`]).
//!
//! Die Konfiguration liegt zur Laufzeit vollständig im RAM (E4); dieses Crate enthält nur das
//! Datenmodell und das Laden aus einer Datei, keinen Zugriff auf Persistenz.

pub mod config;
pub mod error;
pub mod ir;
pub mod observe;
pub mod resource;
pub mod snapshot;
pub mod types;
pub mod wire;

pub use config::Config;
pub use error::{Error, Result};

/// Version des Gateways (aus `Cargo.toml`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Produktname, wie er in API-Antworten und Logs erscheint.
pub const NAME: &str = "ai-gw";
