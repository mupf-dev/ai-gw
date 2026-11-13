// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Kleine Kodierhilfen ohne Zusatz-Crates: Hex, Base64 (Standard und URL-sicher),
//! Prozentkodierung nach RFC 3986, UTC-Zeitstempel und Zufalls-IDs.

use std::time::{SystemTime, UNIX_EPOCH};

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Kleinbuchstaben-Hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(HEX[usize::from(b >> 4)]));
        s.push(char::from(HEX[usize::from(b & 0x0f)]));
    }
    s
}

const B64_STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const B64_URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn b64_encode(input: &[u8], alphabet: &[u8; 64], pad: bool) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let chars = chunk.len() + 1;
        for i in 0..4 {
            if i < chars {
                let idx = (n >> (18 - 6 * i)) & 0x3f;
                out.push(char::from(alphabet[idx as usize]));
            } else if pad {
                out.push('=');
            }
        }
    }
    out
}

/// Base64 mit Standardalphabet und Auffüllung.
pub(crate) fn base64(input: &[u8]) -> String {
    b64_encode(input, B64_STD, true)
}

/// Base64url ohne Auffüllung (JWT, RFC 7515).
pub(crate) fn base64url(input: &[u8]) -> String {
    b64_encode(input, B64_URL, false)
}

/// Dekodiert Base64 (Standard- oder URL-Alphabet, mit oder ohne Auffüllung; Leerraum wird
/// ignoriert, z. B. Zeilenumbrüche in PEM).
pub(crate) fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut padding = false;
    for c in input.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => {
                padding = true;
                continue;
            }
            b' ' | b'\t' | b'\r' | b'\n' => continue,
            _ => return None,
        };
        if padding {
            return None;
        }
        // Es bleiben höchstens 7 unverbrauchte Bits stehen.
        acc = ((acc & 0xff) << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    // Übrige Bits müssen Null sein (kanonische Kodierung), mehr als 4 Restbits = ungültig.
    if bits >= 6 || acc & ((1 << bits) - 1) != 0 {
        return None;
    }
    Some(out)
}

/// Prozentkodierung nach RFC 3986: alles außer `A-Z a-z 0-9 - _ . ~` (und `/`, wenn
/// `keep_slash`) wird als `%XX` (Großbuchstaben) kodiert – wie von SigV4 verlangt.
pub(crate) fn uri_encode(input: &str, keep_slash: bool) -> String {
    const UPPER: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) || (keep_slash && b == b'/') {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(char::from(UPPER[usize::from(b >> 4)]));
            out.push(char::from(UPPER[usize::from(b & 0x0f)]));
        }
    }
    out
}

/// Sekunden seit der Unix-Epoche.
pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Unix-Sekunden → (Jahr, Monat, Tag, Stunde, Minute, Sekunde) in UTC
/// (Algorithmus „civil_from_days“ von H. Hinnant).
pub(crate) fn utc_parts(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    (
        y,
        m as u32,
        d as u32,
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    )
}

/// Zufällige ID `<prefix><24 Hex-Zeichen>` (für Antworten ohne Anbieter-ID).
pub(crate) fn random_id(prefix: &str) -> String {
    let mut buf = [0u8; 12];
    if aws_lc_rs::rand::fill(&mut buf).is_err() {
        // Ohne Zufall: Zeit + Zähler (nur Kennung, keine Sicherheitsfunktion).
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        buf[..8].copy_from_slice(&unix_now().to_be_bytes());
        buf[8..].copy_from_slice(&n.to_be_bytes()[4..]);
    }
    format!("{prefix}{}", hex(&buf))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn base64_roundtrip_and_vectors() {
        // RFC 4648 §10
        for (raw, enc) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(raw.as_bytes()), enc);
            assert_eq!(base64_decode(enc).unwrap(), raw.as_bytes());
            assert_eq!(base64url(raw.as_bytes()), enc.trim_end_matches('='));
        }
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
        assert_eq!(base64_decode("-_8").unwrap(), vec![0xfb, 0xff]);
        assert_eq!(base64_decode("Zm9v\nYmFy").unwrap(), b"foobar");
        assert!(base64_decode("Zm9v!").is_none());
        assert!(base64_decode("Zg=a").is_none());
    }

    #[test]
    fn uri_encode_rfc3986() {
        assert_eq!(uri_encode("a b/c:d~", false), "a%20b%2Fc%3Ad~");
        assert_eq!(uri_encode("a b/c", true), "a%20b/c");
        assert_eq!(uri_encode("ሴ", true), "%E1%88%B4");
    }

    #[test]
    fn utc_conversion() {
        assert_eq!(utc_parts(0), (1970, 1, 1, 0, 0, 0));
        // 2015-08-30T12:36:00Z (AWS-SigV4-Testsuite)
        assert_eq!(utc_parts(1_440_938_160), (2015, 8, 30, 12, 36, 0));
        // Schalttag
        assert_eq!(utc_parts(951_782_400), (2000, 2, 29, 0, 0, 0));
    }

    #[test]
    fn random_ids_differ() {
        let a = random_id("x_");
        assert!(a.starts_with("x_") && a.len() == 26);
        assert_ne!(a, random_id("x_"));
    }
}
