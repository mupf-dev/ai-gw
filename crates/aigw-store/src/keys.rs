// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Virtuelle API-Keys: Erzeugung und Hash.
//!
//! Format: `sk-aigw-<Präfix>-<Geheimnis>`
//! - Präfix: 12 Zeichen Base62 (zufällig, sichtbar, eindeutig) zur Wiedererkennung,
//! - Geheimnis: 32 Byte Zufall, Base62 mit fester Länge 43.
//!
//! Gespeichert wird nur SHA-256 über den vollständigen Key (hex). Wegen 256 Bit Entropie ist
//! kein langsamer Passwort-Hash nötig; der Hot Path bleibt bei einer SHA-256-Berechnung.

use aigw_core::snapshot::KeyHash;
use aws_lc_rs::digest;
use aws_lc_rs::rand::{SecureRandom as _, SystemRandom};

use crate::StoreError;

/// Präfix aller Keys.
pub const KEY_PREFIX: &str = "sk-aigw-";
/// Länge des sichtbaren Präfixes.
pub const PREFIX_LEN: usize = 12;
/// Länge des Base62-kodierten 32-Byte-Geheimnisses (⌈256 / log2 62⌉).
pub const SECRET_LEN: usize = 43;

const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Ein frisch erzeugter Key.
#[derive(Debug, Clone)]
pub struct GeneratedKey {
    /// Klartext (nur einmal herausgeben).
    pub secret: String,
    /// Sichtbares Präfix.
    pub prefix: String,
    /// SHA-256 des Klartexts.
    pub hash: KeyHash,
}

/// Erzeugt einen neuen API-Key.
pub fn generate_api_key() -> Result<GeneratedKey, StoreError> {
    let rng = SystemRandom::new();
    let fill = |buf: &mut [u8]| {
        rng.fill(buf)
            .map_err(|_| StoreError::Crypto("Zufallsquelle nicht verfügbar".into()))
    };

    // Präfix: gleichverteilte Base62-Zeichen per Verwerfungsmethode (Bytes < 248 = 4 × 62).
    let mut prefix = String::with_capacity(PREFIX_LEN);
    while prefix.len() < PREFIX_LEN {
        let mut buf = [0u8; 32];
        fill(&mut buf)?;
        for b in buf {
            if b < 248 && prefix.len() < PREFIX_LEN {
                prefix.push(char::from(BASE62[usize::from(b % 62)]));
            }
        }
    }

    let mut raw = [0u8; 32];
    fill(&mut raw)?;
    let secret = format!("{KEY_PREFIX}{prefix}-{}", base62_fixed(&raw, SECRET_LEN));
    let hash = hash_api_key(&secret);
    Ok(GeneratedKey {
        secret,
        prefix,
        hash,
    })
}

/// SHA-256 eines Keys (so wird er gespeichert und im Snapshot nachgeschlagen).
pub fn hash_api_key(key: &str) -> KeyHash {
    let d = digest::digest(&digest::SHA256, key.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    KeyHash(out)
}

/// Liest das Präfix aus einem Key (ohne Prüfung des Geheimnisses).
pub fn parse_prefix(key: &str) -> Option<&str> {
    let rest = key.strip_prefix(KEY_PREFIX)?;
    let (prefix, secret) = rest.split_once('-')?;
    (prefix.len() == PREFIX_LEN && secret.len() == SECRET_LEN).then_some(prefix)
}

/// Kodiert eine große Zahl (Big-Endian-Bytes) in Base62 mit fester Länge (führende Nullen).
fn base62_fixed(bytes: &[u8], len: usize) -> String {
    let mut num = bytes.to_vec();
    let mut out = vec![b'0'; len];
    for slot in out.iter_mut().rev() {
        // num /= 62, Rest → Ziffer
        let mut rem: u32 = 0;
        for d in &mut num {
            let cur = (rem << 8) | u32::from(*d);
            *d = u8::try_from(cur / 62).unwrap_or(0);
            rem = cur % 62;
        }
        *slot = BASE62[rem as usize];
    }
    String::from_utf8(out).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_format() {
        let k = generate_api_key().unwrap();
        assert!(k.secret.starts_with("sk-aigw-"));
        assert_eq!(
            k.secret.len(),
            KEY_PREFIX.len() + PREFIX_LEN + 1 + SECRET_LEN
        );
        assert_eq!(parse_prefix(&k.secret), Some(k.prefix.as_str()));
        assert!(k.prefix.bytes().all(|b| b.is_ascii_alphanumeric()));
        assert_eq!(hash_api_key(&k.secret), k.hash);
        let k2 = generate_api_key().unwrap();
        assert_ne!(k.secret, k2.secret);
    }

    #[test]
    fn base62_known_values() {
        assert_eq!(base62_fixed(&[0; 32], 43), "0".repeat(43));
        assert_eq!(base62_fixed(&[0, 61], 3), "00z");
        assert_eq!(base62_fixed(&[0, 62], 3), "010");
        // 2^256 - 1 passt in 43 Stellen
        let max = base62_fixed(&[0xff; 32], 43);
        assert_ne!(&max[..1], "0");
    }

    #[test]
    fn sha256_hex() {
        // SHA-256("abc")
        assert_eq!(
            hash_api_key("abc").to_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
