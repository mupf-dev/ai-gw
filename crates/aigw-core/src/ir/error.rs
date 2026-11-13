// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Formatneutrale API-Fehler; die Wire-Module serialisieren sie im Format des Eingangs.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Fehlerklasse (bestimmt Statuscode und `type` im Fehlerobjekt).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorKind {
    /// Ungültige Anfrage (400).
    InvalidRequest,
    /// Fehlende/ungültige Zugangsdaten (401).
    Authentication,
    /// Keine Berechtigung (403).
    PermissionDenied,
    /// Unbekannter Pfad oder unbekanntes Modell (404).
    NotFound,
    /// Methode nicht erlaubt (405).
    MethodNotAllowed,
    /// Anfrage zu groß (413).
    RequestTooLarge,
    /// Limit überschritten (429).
    RateLimit,
    /// Interner Fehler (500).
    Internal,
    /// Funktion (noch) nicht umgesetzt (501).
    NotImplemented,
    /// Upstream-Fehler (502).
    BadGateway,
    /// Überlastet/nicht verfügbar (503 bzw. Anthropic 529).
    Overloaded,
    /// Zeitüberschreitung (504).
    Timeout,
}

impl ErrorKind {
    /// HTTP-Statuscode.
    pub fn status(self) -> u16 {
        match self {
            Self::InvalidRequest => 400,
            Self::Authentication => 401,
            Self::PermissionDenied => 403,
            Self::NotFound => 404,
            Self::MethodNotAllowed => 405,
            Self::RequestTooLarge => 413,
            Self::RateLimit => 429,
            Self::Internal => 500,
            Self::NotImplemented => 501,
            Self::BadGateway => 502,
            Self::Overloaded => 503,
            Self::Timeout => 504,
        }
    }

    /// Fehlerklasse zu einem HTTP-Status (für Upstream-Fehler).
    pub fn from_status(status: u16) -> Self {
        match status {
            401 => Self::Authentication,
            403 => Self::PermissionDenied,
            404 => Self::NotFound,
            405 => Self::MethodNotAllowed,
            413 => Self::RequestTooLarge,
            429 => Self::RateLimit,
            501 => Self::NotImplemented,
            502 => Self::BadGateway,
            503 | 529 => Self::Overloaded,
            504 => Self::Timeout,
            400..=499 => Self::InvalidRequest,
            _ => Self::Internal,
        }
    }
}

/// API-Fehler, der an den Client geht.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    /// Klasse.
    pub kind: ErrorKind,
    /// Menschenlesbare Meldung.
    pub message: String,
    /// Betroffener Parameter (OpenAI `param`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
    /// Maschinenlesbarer Code (OpenAI `code`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl ApiError {
    /// Neuer Fehler.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            param: None,
            code: None,
        }
    }

    /// Ungültige Anfrage.
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidRequest, message)
    }

    /// Setzt `param`.
    #[must_use]
    pub fn with_param(mut self, param: impl Into<String>) -> Self {
        self.param = Some(param.into());
        self
    }

    /// Setzt `code`.
    #[must_use]
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    /// HTTP-Statuscode.
    pub fn status(&self) -> u16 {
        self.kind.status()
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for ApiError {}

impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> Self {
        Self::invalid_request(format!("ungültiges JSON: {e}")).with_code("invalid_json")
    }
}
