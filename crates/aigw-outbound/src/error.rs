// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Fehler ausgehender Aufrufe.

use std::error::Error as StdError;
use std::io;
use std::time::Duration;

use crate::resolve::Blocked;

/// Fehler eines ausgehenden Aufrufs.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum OutboundError {
    /// Ziel durch die SSRF-Regeln gesperrt (Name löst nur auf nicht erlaubte Adressen auf,
    /// oder IP-Literal nicht erlaubt). Eine Wiederholung hilft nicht.
    #[error("Ziel gesperrt: {0}")]
    Blocked(String),
    /// Verbindung/Übertragung fehlgeschlagen.
    #[error("Verbindung fehlgeschlagen: {0}")]
    Connect(String),
    /// Zeitlimit überschritten.
    #[error("Zeitlimit von {} ms überschritten", .0.as_millis())]
    Timeout(Duration),
    /// Antwort größer als erlaubt.
    #[error("Antwort größer als {0} Byte")]
    TooLarge(usize),
    /// Weiterleitung (3xx) – wird nicht verfolgt.
    #[error("Weiterleitung (HTTP {status}) wird nicht verfolgt{}", location_suffix(.location))]
    Redirect {
        /// Status.
        status: u16,
        /// `Location`-Header, falls vorhanden.
        location: Option<String>,
    },
    /// Anfrage ungültig (Schema, Host, Header).
    #[error("ungültige Anfrage: {0}")]
    Request(String),
    /// TLS-Konfiguration (Wurzelzertifikate, CA-Datei).
    #[error("TLS: {0}")]
    Tls(String),
}

impl OutboundError {
    /// Lohnt eine Wiederholung (vorübergehender Fehler)?
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Connect(_) | Self::Timeout(_))
    }

    /// Ziel durch die SSRF-Regeln gesperrt?
    pub fn is_blocked(&self) -> bool {
        matches!(self, Self::Blocked(_))
    }

    /// Fehler des hyper-Clients einordnen (SSRF-Sperre aus dem Resolver erkennen).
    pub(crate) fn from_client(e: &hyper_util::client::legacy::Error) -> Self {
        let mut cur: Option<&(dyn StdError + 'static)> = Some(e);
        while let Some(err) = cur {
            if let Some(b) = err.downcast_ref::<Blocked>() {
                return Self::Blocked(b.to_string());
            }
            if let Some(b) = err
                .downcast_ref::<io::Error>()
                .and_then(|io| io.get_ref())
                .and_then(|inner| inner.downcast_ref::<Blocked>())
            {
                return Self::Blocked(b.to_string());
            }
            cur = err.source();
        }
        Self::Connect(error_chain(e))
    }
}

fn location_suffix(l: &Option<String>) -> String {
    l.as_deref()
        .map(|l| format!(" (Ziel {l})"))
        .unwrap_or_default()
}

/// Fehlerkette als Text (hyper-Fehler nennen die Ursache erst in `source()`).
pub(crate) fn error_chain(e: &(dyn StdError + 'static)) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(s) = cur {
        let msg = s.to_string();
        if !out.ends_with(&msg) {
            out.push_str(": ");
            out.push_str(&msg);
        }
        cur = s.source();
    }
    out
}
