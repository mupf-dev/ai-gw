// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! HMAC-SHA256-Signatur ausgehender Webhooks.
//!
//! Header `x-aigw-signature: t=<unix-sekunden>,v1=<hex>`; signiert wird
//! `"<t>." + <roher Body>` mit dem geteilten Geheimnis. Empfänger prüfen:
//!
//! 1. `t` liegt höchstens [`DEFAULT_TOLERANCE`] (5 min) von der eigenen Uhr entfernt
//!    (Replay-Schutz-Zeitfenster),
//! 2. `v1` stimmt (Vergleich in konstanter Zeit), mehrere `v1=` sind erlaubt
//!    (Schlüsselrotation),
//! 3. optional: `x-aigw-delivery` (bzw. `x-aigw-request-id` + Phase) wurde im Zeitfenster
//!    noch nicht gesehen – dann ist auch eine Wiederholung innerhalb des Fensters wirkungslos.
//!
//! [`verify`] ist der Referenz-Verifier (Tests, Doku, Beispiel für Empfänger).

use std::fmt::Write as _;
use std::time::Duration;

use aws_lc_rs::{constant_time, hmac};

/// Name des Signatur-Headers.
pub const SIGNATURE_HEADER: &str = "x-aigw-signature";

/// Empfohlenes Zeitfenster für den Replay-Schutz.
pub const DEFAULT_TOLERANCE: Duration = Duration::from_secs(300);

fn mac(secret: &[u8], ts: i64, body: &[u8]) -> hmac::Tag {
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    let mut ctx = hmac::Context::with_key(&key);
    ctx.update(ts.to_string().as_bytes());
    ctx.update(b".");
    ctx.update(body);
    ctx.sign()
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Header-Wert für `body` zum Zeitpunkt `ts` (Unix-Sekunden).
pub fn sign(secret: &[u8], ts: i64, body: &[u8]) -> String {
    format!("t={ts},v1={}", hex(mac(secret, ts, body).as_ref()))
}

/// Fehler bei der Prüfung einer Signatur.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    /// Header fehlt oder ist unlesbar.
    #[error("Signatur-Header unlesbar")]
    Malformed,
    /// Zeitstempel außerhalb des Zeitfensters.
    #[error("Zeitstempel außerhalb des Zeitfensters")]
    Expired,
    /// Keine passende Signatur.
    #[error("Signatur stimmt nicht")]
    Mismatch,
}

/// Prüft `header` gegen `body` (Referenz-Verifier für Empfänger). `now` in Unix-Sekunden.
pub fn verify(
    secret: &[u8],
    header: &str,
    body: &[u8],
    now: i64,
    tolerance: Duration,
) -> Result<(), VerifyError> {
    let mut ts = None;
    let mut sigs = Vec::new();
    for part in header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", v)) => ts = Some(v.parse::<i64>().map_err(|_| VerifyError::Malformed)?),
            Some(("v1", v)) => sigs.push(unhex(v).ok_or(VerifyError::Malformed)?),
            Some(_) => {}
            None => return Err(VerifyError::Malformed),
        }
    }
    let ts = ts.ok_or(VerifyError::Malformed)?;
    if sigs.is_empty() {
        return Err(VerifyError::Malformed);
    }
    let tol = i64::try_from(tolerance.as_secs()).unwrap_or(i64::MAX);
    if now.abs_diff(ts) > tol.unsigned_abs() {
        return Err(VerifyError::Expired);
    }
    let expected = mac(secret, ts, body);
    if sigs
        .iter()
        .any(|s| constant_time::verify_slices_are_equal(s, expected.as_ref()).is_ok())
    {
        Ok(())
    } else {
        Err(VerifyError::Mismatch)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn known_vector() {
        // HMAC-SHA256("secret", "1700000000.{}") – unabhängig nachgerechnet (Python hmac).
        assert_eq!(
            sign(b"secret", 1_700_000_000, b"{}"),
            "t=1700000000,v1=b8569b78799ff9e3cbff0fc2d63a33a2b57f3282abd07c37ae5e8e7d79a5f163"
        );
    }

    #[test]
    fn roundtrip_and_failures() {
        let body = br#"{"a":1}"#;
        let h = sign(b"k1", 1000, body);
        assert_eq!(verify(b"k1", &h, body, 1000, DEFAULT_TOLERANCE), Ok(()));
        assert_eq!(verify(b"k1", &h, body, 1299, DEFAULT_TOLERANCE), Ok(()));
        assert_eq!(
            verify(b"k1", &h, body, 1301, DEFAULT_TOLERANCE),
            Err(VerifyError::Expired)
        );
        assert_eq!(
            verify(b"k2", &h, body, 1000, DEFAULT_TOLERANCE),
            Err(VerifyError::Mismatch)
        );
        assert_eq!(
            verify(b"k1", &h, b"{\"a\":2}", 1000, DEFAULT_TOLERANCE),
            Err(VerifyError::Mismatch)
        );
        assert_eq!(
            verify(b"k1", "v1=00", body, 1000, DEFAULT_TOLERANCE),
            Err(VerifyError::Malformed)
        );
        // Rotation: zweite Signatur passt.
        let other = sign(b"k2", 1000, body);
        let both = format!("{h},v1={}", other.split_once("v1=").unwrap().1);
        assert_eq!(verify(b"k2", &both, body, 1000, DEFAULT_TOLERANCE), Ok(()));
    }
}
