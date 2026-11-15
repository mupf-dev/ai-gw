// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! WASM-Module für Plugins der Art `wasm` (AIGW-63, `docs/konzept/17-wasm-plugins.md`).
//!
//! Ein Modul ist eine WebAssembly-**Komponente** (Component Model, WIT-Welt
//! `aigw:plugin/plugin`), hochgeladen über die Admin-API (nur globaler `admin`) oder aus einer
//! Datei der Startkonfiguration (`wasm.modules`). Es liegt mit SHA-256 in der Datenbank und
//! gelangt über den Snapshot zu allen Instanzen (Hot Reload); Bindungen verweisen per Name
//! (optional zusätzlich mit SHA-256).
//!
//! Signaturen (AIGW-101): ein Modul kann eine **Ed25519-Signatur** tragen
//! ([`ModuleSignature`], Schlüssel-ID + Signatur Base64). Signiert wird nicht der Inhalt
//! selbst, sondern die Nachricht [`signing_message`] = `aigw-wasm-module-v1:` + SHA-256 (hex,
//! klein) – Domänentrennung, damit eine Signatur nie für etwas anderes gilt, und das Signieren
//! großer Module braucht nur den Hash. [`TrustPolicy`] prüft gegen `wasm.trusted_keys`.

use std::fmt;

use base64::Engine as _;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::resource::Id;

/// Höchstgröße eines Moduls, die das Gateway überhaupt annimmt (`wasm.max_module_bytes` darf
/// nur kleiner sein).
pub const MAX_MODULE_BYTES: u64 = 64 * 1024 * 1024;

/// Ein gespeichertes WASM-Modul (Metadaten + Inhalt).
#[derive(Clone, PartialEq, Eq)]
pub struct WasmModule {
    /// ID (UUIDv7).
    pub id: Id,
    /// Name (wie Ressourcennamen).
    pub name: String,
    /// SHA-256 des Inhalts, hex, klein geschrieben.
    pub sha256: String,
    /// Beschreibung.
    pub description: Option<String>,
    /// Hochgeladen von (Admin-Principal bzw. `startup-config`).
    pub created_by: String,
    /// Angelegt.
    pub created_at: OffsetDateTime,
    /// Zuletzt ersetzt.
    pub updated_at: OffsetDateTime,
    /// Inhalt (Komponente im Binärformat).
    pub content: Bytes,
    /// Ed25519-Signatur (AIGW-101); `None` = unsigniert.
    pub signature: Option<ModuleSignature>,
}

impl WasmModule {
    /// Größe in Byte.
    pub fn size(&self) -> u64 {
        self.content.len() as u64
    }
}

impl fmt::Debug for WasmModule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasmModule")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("sha256", &self.sha256)
            .field("size", &self.content.len())
            .field("signature", &self.signature.as_ref().map(|s| &s.key_id))
            .finish_non_exhaustive()
    }
}

/// Prüft eine SHA-256-Angabe (64 Hex-Zeichen) und liefert sie klein geschrieben.
pub fn normalize_sha256(s: &str) -> Result<String, String> {
    let s = s.trim();
    let s = s.strip_prefix("sha256:").unwrap_or(s);
    if s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(s.to_ascii_lowercase())
    } else {
        Err("sha256: 64 Hex-Zeichen erwartet".into())
    }
}

/// Erste Bytes einer Komponente (Magic + Layer 1). Kern-Module (Layer 0) sind keine
/// Komponenten und werden abgelehnt.
pub fn looks_like_component(content: &[u8]) -> bool {
    content.len() >= 8 && content[..4] == *b"\0asm" && content[6..8] == [1, 0]
}

/// SHA-256 als Hex (klein).
pub fn sha256_hex(content: &[u8]) -> String {
    let d = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, content);
    d.as_ref()
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
}

// ------------------------------------------------------------------ Signaturen (AIGW-101)

/// Präfix der signierten Nachricht (Domänentrennung, Version des Formats).
pub const SIGNING_CONTEXT: &str = "aigw-wasm-module-v1:";

/// Länge einer Ed25519-Signatur in Byte.
const SIGNATURE_LEN: usize = 64;
/// Länge eines Ed25519-Schlüssels (roh) in Byte.
const PUBLIC_KEY_LEN: usize = 32;
/// DER-Präfix eines Ed25519-SubjectPublicKeyInfo (RFC 8410): danach folgen 32 Byte Schlüssel.
const SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// Die signierte Nachricht: `aigw-wasm-module-v1:<sha256 hex, klein>` (ASCII, ohne
/// Zeilenumbruch). Mit OpenSSL: `printf 'aigw-wasm-module-v1:%s' "$SHA" | openssl pkeyutl
/// -sign -inkey key.pem -rawin | base64 -w0`.
pub fn signing_message(sha256_hex: &str) -> Vec<u8> {
    format!("{SIGNING_CONTEXT}{sha256_hex}").into_bytes()
}

/// Dekodiert Base64 (Standard- oder URL-Alphabet, mit oder ohne Auffüllung, Leerraum egal).
fn decode_b64(s: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    [STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD]
        .iter()
        .find_map(|e| e.decode(s.as_bytes()).ok())
}

/// Ed25519-Signatur eines Moduls: Schlüssel-ID (aus `wasm.trusted_keys`) und Signatur
/// (Base64, Standard-Alphabet, normalisiert).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModuleSignature {
    /// ID des Schlüssels.
    pub key_id: String,
    /// Signatur (64 Byte, Base64).
    pub signature: String,
}

impl ModuleSignature {
    /// Prüft das Format (Schlüssel-ID wie Ressourcennamen, 64 Byte Signatur) und normalisiert
    /// die Signatur auf Base64 im Standard-Alphabet.
    pub fn parse(key_id: &str, signature: &str) -> Result<Self, String> {
        let key_id = key_id.trim();
        crate::resource::validate_name(key_id).map_err(|e| format!("keyId: {e}"))?;
        // `+` aus nicht kodierten Query-Parametern kommt als Leerzeichen an.
        let raw = decode_b64(&signature.trim().replace(' ', "+"))
            .filter(|b| b.len() == SIGNATURE_LEN)
            .ok_or("signature: 64 Byte Ed25519-Signatur als Base64 erwartet")?;
        Ok(Self {
            key_id: key_id.to_owned(),
            signature: base64::engine::general_purpose::STANDARD.encode(raw),
        })
    }

    fn raw(&self) -> Option<Vec<u8>> {
        decode_b64(&self.signature).filter(|b| b.len() == SIGNATURE_LEN)
    }
}

/// Vertrauenswürdiger öffentlicher Schlüssel (`wasm.trusted_keys[]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustedKey {
    /// ID (wie Ressourcennamen), steht in der Signatur des Moduls.
    pub id: String,
    /// Öffentlicher Ed25519-Schlüssel: Base64 der 32 Rohbytes, Base64 eines
    /// SubjectPublicKeyInfo (DER) oder PEM (`-----BEGIN PUBLIC KEY-----`, z. B. aus
    /// `openssl pkey -pubout` oder `aigw wasm keygen`).
    pub public_key: String,
}

/// Liest einen öffentlichen Ed25519-Schlüssel (roh, SPKI-DER oder PEM; jeweils Base64).
pub fn parse_public_key(s: &str) -> Result<[u8; PUBLIC_KEY_LEN], String> {
    let body: String = s
        .lines()
        .filter(|l| !l.trim_start().starts_with("-----"))
        .collect();
    let raw = decode_b64(&body).ok_or("public_key: kein gültiges Base64")?;
    let key = match raw.len() {
        PUBLIC_KEY_LEN => raw.as_slice(),
        44 if raw[..12] == SPKI_PREFIX => &raw[12..],
        _ => {
            return Err(
                "public_key: Ed25519-Schlüssel erwartet (32 Byte roh oder SubjectPublicKeyInfo)"
                    .into(),
            );
        }
    };
    let mut out = [0u8; PUBLIC_KEY_LEN];
    out.copy_from_slice(key);
    Ok(out)
}

/// Kodiert einen öffentlichen Schlüssel als PEM (SubjectPublicKeyInfo).
pub fn public_key_pem(raw: &[u8; PUBLIC_KEY_LEN]) -> String {
    let mut der = SPKI_PREFIX.to_vec();
    der.extend_from_slice(raw);
    format!(
        "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
        base64::engine::general_purpose::STANDARD.encode(der)
    )
}

/// Ergebnis einer Signaturprüfung, die das Laden erlaubt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verified {
    /// Gültige Signatur eines vertrauenswürdigen Schlüssels.
    Valid {
        /// ID des Schlüssels.
        key_id: String,
    },
    /// Unsigniert, aber erlaubt (`wasm.require_signature: false`).
    Unsigned,
    /// Nicht geprüft: keine `wasm.trusted_keys` konfiguriert (Signaturprüfung aus).
    Unchecked,
}

/// Grund, warum ein Modul nicht geladen bzw. angenommen wird.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SignatureError {
    /// Signatur ist Pflicht, fehlt aber.
    #[error("Signatur fehlt (wasm.require_signature: Module müssen signiert sein)")]
    Missing,
    /// Schlüssel-ID nicht in `wasm.trusted_keys`.
    #[error("unbekannter Schlüssel {0:?} (nicht in wasm.trusted_keys)")]
    UnknownKey(String),
    /// Signatur passt nicht zu Inhalt und Schlüssel.
    #[error("Signatur mit Schlüssel {0:?} ungültig")]
    Invalid(String),
    /// Inhalt passt nicht zum gespeicherten SHA-256 (Laufzeitprüfung).
    #[error("Inhalt passt nicht zum SHA-256 {0}")]
    Digest(String),
}

impl SignatureError {
    /// Kurzform für Anzeigen (`missing`, `unknown-key`, `invalid`, `digest`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::UnknownKey(_) => "unknown-key",
            Self::Invalid(_) => "invalid",
            Self::Digest(_) => "digest",
        }
    }
}

/// Signatur-Richtlinie aus `wasm.trusted_keys` und `wasm.require_signature`.
///
/// - Keine Schlüssel: Prüfung aus (wie vor AIGW-101); Signaturen werden nur gespeichert.
/// - Schlüssel konfiguriert: eine **vorhandene** Signatur muss immer gültig sein (unbekannter
///   Schlüssel oder falsche Signatur = abgelehnt); ohne Signatur entscheidet
///   `require_signature` (Standard dann `true`).
#[derive(Debug, Clone, Default)]
pub struct TrustPolicy {
    keys: Vec<(String, [u8; PUBLIC_KEY_LEN])>,
    required: bool,
}

impl TrustPolicy {
    /// Baut die Richtlinie und prüft die Konfiguration (Schlüssel-IDs eindeutig, Schlüssel
    /// lesbar, `require_signature: true` nur mit Schlüsseln).
    pub fn new(keys: &[TrustedKey], require_signature: Option<bool>) -> Result<Self, String> {
        let mut out: Vec<(String, [u8; PUBLIC_KEY_LEN])> = Vec::with_capacity(keys.len());
        for k in keys {
            crate::resource::validate_name(&k.id)
                .map_err(|e| format!("wasm.trusted_keys[{}]: id: {e}", k.id))?;
            if out.iter().any(|(id, _)| *id == k.id) {
                return Err(format!("wasm.trusted_keys: id {:?} doppelt", k.id));
            }
            let raw = parse_public_key(&k.public_key)
                .map_err(|e| format!("wasm.trusted_keys[{}]: {e}", k.id))?;
            out.push((k.id.clone(), raw));
        }
        let required = require_signature.unwrap_or(!out.is_empty());
        if required && out.is_empty() {
            return Err(
                "wasm.require_signature: true braucht mindestens einen Schlüssel in wasm.trusted_keys"
                    .into(),
            );
        }
        Ok(Self {
            keys: out,
            required,
        })
    }

    /// Aus der WASM-Konfiguration.
    pub fn from_config(cfg: &crate::config::WasmConfig) -> Result<Self, String> {
        Self::new(&cfg.trusted_keys, cfg.require_signature)
    }

    /// Signaturprüfung aktiv (mindestens ein Schlüssel)?
    pub fn active(&self) -> bool {
        !self.keys.is_empty()
    }

    /// Sind Signaturen Pflicht?
    pub fn required(&self) -> bool {
        self.required
    }

    /// IDs der vertrauenswürdigen Schlüssel.
    pub fn key_ids(&self) -> impl Iterator<Item = &str> {
        self.keys.iter().map(|(id, _)| id.as_str())
    }

    /// Prüft eine (optionale) Signatur über den SHA-256 (hex, klein) eines Moduls.
    pub fn verify(
        &self,
        sha256_hex: &str,
        sig: Option<&ModuleSignature>,
    ) -> Result<Verified, SignatureError> {
        if !self.active() {
            return Ok(Verified::Unchecked);
        }
        let Some(sig) = sig else {
            return if self.required {
                Err(SignatureError::Missing)
            } else {
                Ok(Verified::Unsigned)
            };
        };
        let Some((_, key)) = self.keys.iter().find(|(id, _)| *id == sig.key_id) else {
            return Err(SignatureError::UnknownKey(sig.key_id.clone()));
        };
        let raw = sig
            .raw()
            .ok_or_else(|| SignatureError::Invalid(sig.key_id.clone()))?;
        aws_lc_rs::signature::UnparsedPublicKey::new(&aws_lc_rs::signature::ED25519, key)
            .verify(&signing_message(sha256_hex), &raw)
            .map_err(|_| SignatureError::Invalid(sig.key_id.clone()))?;
        Ok(Verified::Valid {
            key_id: sig.key_id.clone(),
        })
    }

    /// Laufzeitprüfung vor dem Kompilieren: bei aktiver Prüfung zusätzlich SHA-256 des Inhalts
    /// neu berechnen (Schutz gegen geänderte Inhalte in Datenbank oder Snapshot-Cache).
    pub fn check_module(&self, m: &WasmModule) -> Result<Verified, SignatureError> {
        if !self.active() {
            return Ok(Verified::Unchecked);
        }
        if sha256_hex(&m.content) != m.sha256 {
            return Err(SignatureError::Digest(m.sha256.clone()));
        }
        self.verify(&m.sha256, m.signature.as_ref())
    }
}

/// Signiert den SHA-256 (hex) eines Moduls mit einem privaten Ed25519-Schlüssel (PKCS#8, DER).
/// Liefert die Signatur als Base64 (Standard-Alphabet).
pub fn sign_sha256(pkcs8_der: &[u8], sha256_hex: &str) -> Result<String, String> {
    let kp = aws_lc_rs::signature::Ed25519KeyPair::from_pkcs8(pkcs8_der)
        .map_err(|e| format!("privater Schlüssel: kein Ed25519-PKCS#8 ({e})"))?;
    let sig = kp.sign(&signing_message(sha256_hex));
    Ok(base64::engine::general_purpose::STANDARD.encode(sig.as_ref()))
}

/// Öffentlicher Schlüssel (32 Byte roh) zu einem privaten Ed25519-Schlüssel (PKCS#8, DER).
pub fn public_key_of(pkcs8_der: &[u8]) -> Result<[u8; PUBLIC_KEY_LEN], String> {
    use aws_lc_rs::signature::KeyPair as _;
    let kp = aws_lc_rs::signature::Ed25519KeyPair::from_pkcs8(pkcs8_der)
        .map_err(|e| format!("privater Schlüssel: kein Ed25519-PKCS#8 ({e})"))?;
    let mut out = [0u8; PUBLIC_KEY_LEN];
    out.copy_from_slice(kp.public_key().as_ref());
    Ok(out)
}

/// Neuer privater Ed25519-Schlüssel (PKCS#8 v2, DER).
pub fn generate_pkcs8() -> Result<Vec<u8>, String> {
    let doc =
        aws_lc_rs::signature::Ed25519KeyPair::generate_pkcs8(&aws_lc_rs::rand::SystemRandom::new())
            .map_err(|e| format!("Schlüssel erzeugen: {e}"))?;
    Ok(doc.as_ref().to_vec())
}

/// Liest einen privaten Schlüssel: PEM (`-----BEGIN PRIVATE KEY-----`) oder DER.
pub fn private_key_der(bytes: &[u8]) -> Result<Vec<u8>, String> {
    match std::str::from_utf8(bytes) {
        Ok(text) if text.contains("-----BEGIN") => {
            let body: String = text
                .lines()
                .filter(|l| !l.trim_start().starts_with("-----"))
                .collect();
            decode_b64(&body).ok_or_else(|| "privater Schlüssel: PEM ohne gültiges Base64".into())
        }
        _ => Ok(bytes.to_vec()),
    }
}

/// Kodiert einen privaten Schlüssel (PKCS#8 DER) als PEM.
pub fn private_key_pem(der: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = String::from("-----BEGIN PRIVATE KEY-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push('\n');
    }
    out.push_str("-----END PRIVATE KEY-----\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Testschlüssel (nur für Tests und e2e; PKCS#8 v1 aus `openssl genpkey -algorithm ed25519`).
    const TEST_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIISeyP02UPhXd4EJLgZwJ3YqquNP2arCUOoBO3c+8ExW\n-----END PRIVATE KEY-----\n";
    const TEST_PUB: &str = "MCowBQYDK2VwAyEAzE7fgt1PTL6EqPFBijaqM3R4YvdTwI3prnZJgrtvzc0=";

    fn key(id: &str, public_key: &str) -> TrustedKey {
        TrustedKey {
            id: id.into(),
            public_key: public_key.into(),
        }
    }

    fn signed(der: &[u8], id: &str, sha: &str) -> ModuleSignature {
        ModuleSignature::parse(id, &sign_sha256(der, sha).unwrap()).unwrap()
    }

    #[test]
    fn public_key_formats() {
        let raw = parse_public_key(TEST_PUB).unwrap();
        let pem = public_key_pem(&raw);
        assert_eq!(parse_public_key(&pem).unwrap(), raw);
        let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
        assert_eq!(parse_public_key(&b64).unwrap(), raw);
        assert!(parse_public_key("AAAA").is_err());
        let der = private_key_der(TEST_KEY.as_bytes()).unwrap();
        assert_eq!(public_key_of(&der).unwrap(), raw);
    }

    #[test]
    fn verify_cases() {
        let sha = sha256_hex(b"modul");
        let der = private_key_der(TEST_KEY.as_bytes()).unwrap();
        let other = generate_pkcs8().unwrap();
        let p = TrustPolicy::new(&[key("release", TEST_PUB)], None).unwrap();
        assert!(p.required(), "mit Schlüsseln ist die Signatur Pflicht");
        // gültig
        let good = signed(&der, "release", &sha);
        assert_eq!(
            p.verify(&sha, Some(&good)),
            Ok(Verified::Valid {
                key_id: "release".into()
            })
        );
        // anderer Inhalt
        assert_eq!(
            p.verify(&sha256_hex(b"anders"), Some(&good)),
            Err(SignatureError::Invalid("release".into()))
        );
        // falscher Schlüssel unter bekannter ID
        let forged = signed(&other, "release", &sha);
        assert_eq!(
            p.verify(&sha, Some(&forged)),
            Err(SignatureError::Invalid("release".into()))
        );
        // unbekannte ID
        let unknown = signed(&der, "fremd", &sha);
        assert_eq!(
            p.verify(&sha, Some(&unknown)),
            Err(SignatureError::UnknownKey("fremd".into()))
        );
        // Pflicht ohne Signatur
        assert_eq!(p.verify(&sha, None), Err(SignatureError::Missing));
        // nicht Pflicht: unsigniert erlaubt, falsche Signatur trotzdem abgelehnt
        let lax = TrustPolicy::new(&[key("release", TEST_PUB)], Some(false)).unwrap();
        assert_eq!(lax.verify(&sha, None), Ok(Verified::Unsigned));
        assert!(lax.verify(&sha, Some(&forged)).is_err());
        // keine Schlüssel: Prüfung aus
        let off = TrustPolicy::default();
        assert_eq!(off.verify(&sha, Some(&forged)), Ok(Verified::Unchecked));
        assert_eq!(off.verify(&sha, None), Ok(Verified::Unchecked));
    }

    #[test]
    fn key_rotation() {
        let sha = sha256_hex(b"modul");
        let old = private_key_der(TEST_KEY.as_bytes()).unwrap();
        let new = generate_pkcs8().unwrap();
        let new_pub = public_key_pem(&public_key_of(&new).unwrap());
        let s_old = signed(&old, "k2026", &sha);
        let s_new = signed(&new, "k2027", &sha);
        // Übergang: beide Schlüssel vertraut.
        let both =
            TrustPolicy::new(&[key("k2026", TEST_PUB), key("k2027", &new_pub)], None).unwrap();
        assert!(both.verify(&sha, Some(&s_old)).is_ok());
        assert!(both.verify(&sha, Some(&s_new)).is_ok());
        // Alter Schlüssel entfernt: alte Signaturen laden nicht mehr.
        let only_new = TrustPolicy::new(&[key("k2027", &new_pub)], None).unwrap();
        assert_eq!(
            only_new.verify(&sha, Some(&s_old)),
            Err(SignatureError::UnknownKey("k2026".into()))
        );
        assert!(only_new.verify(&sha, Some(&s_new)).is_ok());
    }

    #[test]
    fn policy_config_errors() {
        assert!(TrustPolicy::new(&[], Some(true)).is_err());
        assert!(!TrustPolicy::new(&[], None).unwrap().required());
        assert!(TrustPolicy::new(&[key("a", TEST_PUB), key("a", TEST_PUB)], None).is_err());
        assert!(TrustPolicy::new(&[key("a", "kaputt")], None).is_err());
        assert!(TrustPolicy::new(&[key("A B", TEST_PUB)], None).is_err());
    }

    #[test]
    fn signature_format() {
        assert!(ModuleSignature::parse("k", "AAAA").is_err());
        let url = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([0xfbu8; 64]);
        let s = ModuleSignature::parse(" k ", &url).unwrap();
        assert_eq!(s.key_id, "k");
        assert_eq!(
            s.signature,
            base64::engine::general_purpose::STANDARD.encode([0xfbu8; 64])
        );
    }

    #[test]
    fn module_digest_checked_at_runtime() {
        let der = private_key_der(TEST_KEY.as_bytes()).unwrap();
        let content = Bytes::from_static(b"\0asm\x0d\0\x01\0inhalt");
        let sha = sha256_hex(&content);
        let mut m = WasmModule {
            id: crate::resource::new_id(),
            name: "m".into(),
            sha256: sha.clone(),
            description: None,
            created_by: "t".into(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            content,
            signature: Some(signed(&der, "release", &sha)),
        };
        let p = TrustPolicy::new(&[key("release", TEST_PUB)], None).unwrap();
        assert!(p.check_module(&m).is_ok());
        m.content = Bytes::from_static(b"\0asm\x0d\0\x01\0getauscht");
        assert_eq!(p.check_module(&m), Err(SignatureError::Digest(sha)));
    }

    #[test]
    fn sha_normalized() {
        let h = "AB".repeat(32);
        assert_eq!(normalize_sha256(&h).unwrap(), "ab".repeat(32));
        assert_eq!(
            normalize_sha256(&format!("sha256:{h}")).unwrap(),
            "ab".repeat(32)
        );
        assert!(normalize_sha256("abc").is_err());
        assert!(normalize_sha256(&"g".repeat(64)).is_err());
    }

    #[test]
    fn component_magic() {
        assert!(looks_like_component(b"\0asm\x0d\0\x01\0rest"));
        assert!(!looks_like_component(b"\0asm\x01\0\0\0"));
        assert!(!looks_like_component(b"\0asm"));
    }
}
