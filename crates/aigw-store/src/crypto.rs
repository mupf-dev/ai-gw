// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Verschlüsselung der Anbieter-Zugangsdaten: AES-256-GCM über aws-lc-rs (derselbe
//! Krypto-Provider wie rustls), Master-Key aus Datei oder Umgebung, Rotation über Key-IDs.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use aigw_core::config::SecretsConfig;
use aws_lc_rs::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use aws_lc_rs::digest;
use aws_lc_rs::rand::{SecureRandom as _, SystemRandom};

use crate::StoreError;

/// Länge des Master-Keys in Byte.
pub const KEY_LEN: usize = 32;

/// Ein Master-Key mit abgeleiteter ID.
pub struct MasterKey {
    id: String,
    key: LessSafeKey,
}

impl fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MasterKey").field("id", &self.id).finish()
    }
}

impl MasterKey {
    /// Erzeugt einen Key aus 32 Rohbytes. Die ID sind die ersten 8 Byte (hex) von
    /// SHA-256(`"aigw-master-key-id" || key`) – stabil, ohne den Key preiszugeben.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, StoreError> {
        if bytes.len() != KEY_LEN {
            return Err(StoreError::Crypto(format!(
                "Master-Key muss {KEY_LEN} Byte lang sein, hat {}",
                bytes.len()
            )));
        }
        let mut ctx = digest::Context::new(&digest::SHA256);
        ctx.update(b"aigw-master-key-id");
        ctx.update(bytes);
        let id = hex(&ctx.finish().as_ref()[..8]);
        let unbound = UnboundKey::new(&AES_256_GCM, bytes)
            .map_err(|_| StoreError::Crypto("ungültiger AES-256-Key".into()))?;
        Ok(Self {
            id,
            key: LessSafeKey::new(unbound),
        })
    }

    /// Parst einen hexkodierten Key (64 Zeichen, Leerraum am Rand wird ignoriert).
    pub fn from_hex(text: &str) -> Result<Self, StoreError> {
        let bytes = unhex(text.trim()).ok_or_else(|| {
            StoreError::Crypto("Master-Key muss 64 Hex-Zeichen (32 Byte) sein".into())
        })?;
        Self::from_bytes(&bytes)
    }

    /// Erzeugt einen zufälligen Key (Tests, `aigw keygen`).
    pub fn generate() -> Result<(Self, String), StoreError> {
        let mut bytes = [0u8; KEY_LEN];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| StoreError::Crypto("Zufallsquelle nicht verfügbar".into()))?;
        let text = hex(&bytes);
        Ok((Self::from_bytes(&bytes)?, text))
    }

    /// Key-ID.
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// Verschlüsselter Datensatz.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
    /// ID des verwendeten Master-Keys.
    pub key_id: String,
    /// Nonce (hex).
    pub nonce: String,
    /// Chiffrat inkl. Tag (hex).
    pub ciphertext: String,
}

/// Aktueller Master-Key plus frühere Keys zum Entschlüsseln.
#[derive(Debug)]
pub struct Keyring {
    primary: MasterKey,
    previous: HashMap<String, MasterKey>,
}

impl Keyring {
    /// Keyring nur mit einem Key.
    pub fn new(primary: MasterKey) -> Self {
        Self {
            primary,
            previous: HashMap::new(),
        }
    }

    /// Fügt einen früheren Key hinzu (nur Entschlüsseln).
    #[must_use]
    pub fn with_previous(mut self, key: MasterKey) -> Self {
        if key.id != self.primary.id {
            self.previous.insert(key.id.clone(), key);
        }
        self
    }

    /// Lädt die Keys laut Startkonfiguration (Datei vor Umgebungsvariable).
    pub fn from_config(cfg: &SecretsConfig) -> Result<Self, StoreError> {
        let read = |p: &Path| {
            std::fs::read_to_string(p).map_err(|e| {
                StoreError::Crypto(format!(
                    "Master-Key-Datei {} nicht lesbar: {e}",
                    p.display()
                ))
            })
        };
        let primary = match &cfg.master_key_file {
            Some(p) => MasterKey::from_hex(&read(p)?)?,
            None => {
                let v = std::env::var(&cfg.master_key_env).map_err(|_| {
                    StoreError::Crypto(format!(
                        "kein Master-Key: weder secrets.master_key_file noch {} gesetzt",
                        cfg.master_key_env
                    ))
                })?;
                MasterKey::from_hex(&v)?
            }
        };
        let mut ring = Self::new(primary);
        for p in &cfg.previous_key_files {
            ring = ring.with_previous(MasterKey::from_hex(&read(p)?)?);
        }
        Ok(ring)
    }

    /// ID des aktuellen Keys.
    pub fn primary_id(&self) -> &str {
        self.primary.id()
    }

    /// Verschlüsselt `plaintext` mit dem aktuellen Key. `aad` bindet das Chiffrat an seinen
    /// Datensatz (hier: die Ressourcen-ID), damit es nicht in andere Zeilen kopiert werden kann.
    pub fn seal(&self, aad: &[u8], plaintext: &[u8]) -> Result<Sealed, StoreError> {
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| StoreError::Crypto("Zufallsquelle nicht verfügbar".into()))?;
        let mut buf = plaintext.to_vec();
        self.primary
            .key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad),
                &mut buf,
            )
            .map_err(|_| StoreError::Crypto("Verschlüsselung fehlgeschlagen".into()))?;
        Ok(Sealed {
            key_id: self.primary.id.clone(),
            nonce: hex(&nonce),
            ciphertext: hex(&buf),
        })
    }

    /// Entschlüsselt einen Datensatz (mit dem Key passend zur `key_id`).
    pub fn open(&self, aad: &[u8], sealed: &Sealed) -> Result<Vec<u8>, StoreError> {
        let key = if sealed.key_id == self.primary.id {
            &self.primary
        } else {
            self.previous.get(&sealed.key_id).ok_or_else(|| {
                StoreError::Crypto(format!(
                    "Master-Key {} nicht vorhanden (secrets.previous_key_files?)",
                    sealed.key_id
                ))
            })?
        };
        let nonce: [u8; NONCE_LEN] = unhex(&sealed.nonce)
            .and_then(|n| n.try_into().ok())
            .ok_or_else(|| StoreError::Crypto("ungültige Nonce".into()))?;
        let mut buf = unhex(&sealed.ciphertext)
            .ok_or_else(|| StoreError::Crypto("ungültiges Chiffrat".into()))?;
        let plain = key
            .key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad),
                &mut buf,
            )
            .map_err(|_| {
                StoreError::Crypto("Entschlüsselung fehlgeschlagen (Key/Daten?)".into())
            })?;
        Ok(plain.to_vec())
    }
}

/// Kleinbuchstaben-Hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(DIGITS[usize::from(b >> 4)]));
        s.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    s
}

/// Gegenstück zu [`hex`].
pub(crate) fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip_and_aad_binding() {
        let (k, _) = MasterKey::generate().unwrap();
        let ring = Keyring::new(k);
        let s = ring.seal(b"id-1", b"geheim").unwrap();
        assert_eq!(s.key_id, ring.primary_id());
        assert_eq!(ring.open(b"id-1", &s).unwrap(), b"geheim");
        assert!(ring.open(b"id-2", &s).is_err(), "AAD bindet an Datensatz");
        let mut tampered = s.clone();
        let flipped = if s.ciphertext.starts_with("00") {
            "11"
        } else {
            "00"
        };
        tampered.ciphertext.replace_range(0..2, flipped);
        assert!(ring.open(b"id-1", &tampered).is_err());
    }

    #[test]
    fn rotation_with_previous_key() {
        let (old, old_hex) = MasterKey::generate().unwrap();
        let sealed = Keyring::new(old).seal(b"x", b"alt").unwrap();
        let (new, _) = MasterKey::generate().unwrap();
        let ring = Keyring::new(new).with_previous(MasterKey::from_hex(&old_hex).unwrap());
        assert_ne!(ring.primary_id(), sealed.key_id);
        assert_eq!(ring.open(b"x", &sealed).unwrap(), b"alt");
        let (other, _) = MasterKey::generate().unwrap();
        assert!(Keyring::new(other).open(b"x", &sealed).is_err());
    }

    #[test]
    fn key_parsing() {
        assert!(MasterKey::from_hex("abcd").is_err());
        let hex64 = "00".repeat(32);
        let a = MasterKey::from_hex(&format!("{hex64}\n")).unwrap();
        let b = MasterKey::from_hex(&hex64).unwrap();
        assert_eq!(a.id(), b.id());
        assert_eq!(a.id().len(), 16);
        assert_eq!(unhex(&hex(&[1, 2, 255])).unwrap(), vec![1, 2, 255]);
    }
}
