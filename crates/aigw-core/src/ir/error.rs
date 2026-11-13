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

/// Klasse eines Upstream-Fehlers – Grundlage für Retry- und Fallback-Entscheidungen
/// (Architektur §6, AIGW-25) und für das Request-Log.
///
/// Die Einordnung übernimmt der Anbieter-Adapter (`classify_error`), weil Statuscodes allein
/// nicht genügen (z. B. 429 „Rate-Limit“ vs. 429 „Kontingent erschöpft“, 400 „Kontextlänge“).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorClass {
    /// Ungültige Anfrage (400/422), nicht retrybar.
    InvalidRequest,
    /// Kontext zu lang – Fallback auf ein Modell mit größerem Kontext sinnvoll.
    ContextLength,
    /// Inhaltsfilter des Anbieters, nicht retrybar.
    ContentFilter,
    /// Upstream lehnt die Zugangsdaten des Gateways ab (401/403).
    Auth,
    /// Modell/Deployment beim Anbieter unbekannt (404).
    NotFound,
    /// Rate-Limit (429), retrybar (ggf. nach `Retry-After`).
    RateLimit,
    /// Kontingent/Guthaben erschöpft (z. B. OpenAI `insufficient_quota`); auf diesem
    /// Deployment nicht retrybar, Fallback sinnvoll.
    Quota,
    /// Zeitüberschreitung (erstes Byte, Chunk-Pause, Gesamtzeit; 408/504).
    Timeout,
    /// Verbindung nicht aufgebaut oder abgerissen.
    Connect,
    /// Serverfehler des Anbieters (500/502 …), retrybar.
    ServerError,
    /// Anbieter überlastet (503/529), retrybar.
    Overloaded,
    /// Antwort des Anbieters nicht lesbar (Protokollfehler), nicht retrybar.
    Protocol,
    /// Fehler im Gateway selbst (oder vom Gateway abgelehnte Anfrage).
    Internal,
}

impl ErrorClass {
    /// Darf die Anfrage (vor dem ersten Byte an den Client) wiederholt werden?
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::RateLimit | Self::Timeout | Self::Connect | Self::ServerError | Self::Overloaded
        )
    }

    /// Bezeichner (für Logs und Metriken).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::ContextLength => "context_length",
            Self::ContentFilter => "content_filter",
            Self::Auth => "auth",
            Self::NotFound => "not_found",
            Self::RateLimit => "rate_limit",
            Self::Quota => "quota",
            Self::Timeout => "timeout",
            Self::Connect => "connect",
            Self::ServerError => "server_error",
            Self::Overloaded => "overloaded",
            Self::Protocol => "protocol",
            Self::Internal => "internal",
        }
    }

    /// Grobe Klasse zu einer Fehlerart (wenn kein Adapter klassifiziert hat).
    pub fn from_kind(kind: ErrorKind) -> Self {
        match kind {
            ErrorKind::InvalidRequest
            | ErrorKind::RequestTooLarge
            | ErrorKind::MethodNotAllowed => Self::InvalidRequest,
            ErrorKind::Authentication | ErrorKind::PermissionDenied => Self::Auth,
            ErrorKind::NotFound => Self::NotFound,
            ErrorKind::RateLimit => Self::RateLimit,
            ErrorKind::Timeout => Self::Timeout,
            ErrorKind::BadGateway => Self::ServerError,
            ErrorKind::Overloaded => Self::Overloaded,
            ErrorKind::Internal | ErrorKind::NotImplemented => Self::Internal,
        }
    }
}

impl fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Klassifizierter Fehler eines Upstream-Aufrufs (Ergebnis von `classify_error`).
///
/// `error` geht – im Format des Eingangs – an den Client; die übrigen Felder steuern Retries,
/// Fallbacks und das Request-Log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamError {
    /// Klasse.
    pub class: ErrorClass,
    /// Darf wiederholt werden (Standard: [`ErrorClass::is_retryable`]).
    pub retryable: bool,
    /// Vom Anbieter verlangte Wartezeit (`Retry-After`, `retry-after-ms`).
    pub retry_after: Option<std::time::Duration>,
    /// HTTP-Status des Anbieters (fehlt bei Verbindungs-/Zeitfehlern).
    pub upstream_status: Option<u16>,
    /// Fehler für den Client.
    pub error: ApiError,
}

impl UpstreamError {
    /// Neuer Fehler; `retryable` folgt der Klasse.
    pub fn new(class: ErrorClass, error: ApiError) -> Self {
        Self {
            class,
            retryable: class.is_retryable(),
            retry_after: None,
            upstream_status: None,
            error,
        }
    }

    /// Setzt den HTTP-Status des Anbieters.
    #[must_use]
    pub fn with_status(mut self, status: u16) -> Self {
        self.upstream_status = Some(status);
        self
    }

    /// Setzt die Wartezeit.
    #[must_use]
    pub fn with_retry_after(mut self, d: Option<std::time::Duration>) -> Self {
        self.retry_after = d;
        self
    }
}

impl From<ApiError> for UpstreamError {
    fn from(error: ApiError) -> Self {
        Self::new(ErrorClass::from_kind(error.kind), error)
    }
}

impl fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.class, self.error)
    }
}

impl std::error::Error for UpstreamError {}
