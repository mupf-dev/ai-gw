// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Gemeinsamer Fehlertyp.

use std::path::PathBuf;

/// Ergebnis-Alias mit [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Fehler, die crate-übergreifend auftreten können.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Konfigurationsdatei konnte nicht gelesen werden.
    #[error("Konfiguration {path} nicht lesbar: {source}")]
    ConfigRead {
        /// Pfad der Datei.
        path: PathBuf,
        /// Ursache.
        #[source]
        source: std::io::Error,
    },
    /// Konfiguration ist syntaktisch oder inhaltlich ungültig.
    #[error("Konfiguration ungültig: {0}")]
    ConfigInvalid(String),
    /// Interner Fehler (Invariante verletzt, z. B. vergiftetes Lock).
    #[error("interner Fehler: {0}")]
    Internal(String),
    /// Funktion ist noch nicht umgesetzt (Platzhalter in frühen Ausbaustufen).
    #[error("nicht umgesetzt: {0}")]
    NotImplemented(&'static str),
}
