// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

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
    /// Sigstore-Bundle (JSON, AIGW-106); `None` = ohne.
    pub sigstore_bundle: Option<String>,
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
            .field("sigstore_bundle", &self.sigstore_bundle.is_some())
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

/// Unterzeichner laut Sigstore-Bundle (AIGW-106).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SigstoreSigner {
    /// Subjekt (SAN) des Fulcio-Zertifikats, z. B. Workflow-URI oder E-Mail.
    pub subject: String,
    /// OIDC-Issuer.
    pub issuer: String,
    /// Zeitpunkt des Rekor-Eintrags (Unix-Sekunden).
    pub integrated_time: i64,
    /// Index im Rekor-Log.
    pub log_index: i64,
}

/// Ergebnis einer Signaturprüfung, die das Laden erlaubt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verified {
    /// Gültige Ed25519-Signatur eines vertrauenswürdigen Schlüssels (ein zusätzliches
    /// Sigstore-Bundle ist dann ebenfalls gültig).
    Valid {
        /// ID des Schlüssels.
        key_id: String,
    },
    /// Gültiges Sigstore-Bundle einer erlaubten Identität (AIGW-106).
    Sigstore(SigstoreSigner),
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
    /// Sigstore-Bundle ungültig (Kette, SCT, Rekor, Signatur) oder Sigstore nicht konfiguriert.
    #[error("Sigstore-Bundle ungültig: {0}")]
    Sigstore(String),
    /// Sigstore-Bundle gültig, aber von keiner erlaubten Identität (`wasm.sigstore.identities`).
    #[error("Sigstore-Identität {subject:?} (Issuer {issuer:?}) nicht erlaubt")]
    Identity {
        /// Subjekt(e) des Zertifikats.
        subject: String,
        /// OIDC-Issuer.
        issuer: String,
    },
}

impl SignatureError {
    /// Kurzform für Anzeigen (`missing`, `unknown-key`, `invalid`, `digest`, `sigstore`,
    /// `identity`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::UnknownKey(_) => "unknown-key",
            Self::Invalid(_) => "invalid",
            Self::Digest(_) => "digest",
            Self::Sigstore(_) => "sigstore",
            Self::Identity { .. } => "identity",
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
    sigstore: Option<std::sync::Arc<SigstoreTrust>>,
}

/// Sigstore-Teil der Richtlinie (AIGW-106): Trust-Root und erlaubte Identitäten.
#[derive(Debug)]
pub struct SigstoreTrust {
    root: aigw_sigstore::TrustedRoot,
    policy: aigw_sigstore::Policy,
    identities: Vec<crate::config::SigstoreIdentity>,
}

impl SigstoreTrust {
    /// Aus dem Inhalt eines `trusted_root.json` und den Identitäten.
    pub fn new(
        trusted_root_json: &[u8],
        identities: &[crate::config::SigstoreIdentity],
        require_inclusion_proof: bool,
    ) -> Result<Self, String> {
        let root = aigw_sigstore::TrustedRoot::from_json(trusted_root_json)
            .map_err(|e| format!("wasm.sigstore.trusted_root: {e}"))?;
        let ids = compile_identities(identities)?;
        let mut policy = aigw_sigstore::Policy::new(ids);
        policy.require_inclusion_proof = require_inclusion_proof;
        Ok(Self {
            root,
            policy,
            identities: identities.to_vec(),
        })
    }

    /// Liest das Trust-Root aus `wasm.sigstore.trusted_root` (Datei).
    pub fn from_config(cfg: &crate::config::SigstoreConfig) -> Result<Self, String> {
        let json = std::fs::read(&cfg.trusted_root)
            .map_err(|e| format!("wasm.sigstore.trusted_root: {}: {e}", cfg.trusted_root))?;
        Self::new(&json, &cfg.identities, cfg.require_inclusion_proof)
    }

    /// Erlaubte Identitäten (wie konfiguriert).
    pub fn identities(&self) -> &[crate::config::SigstoreIdentity] {
        &self.identities
    }

    /// Prüft ein Bundle über den SHA-256 (hex) eines Moduls.
    pub fn verify(&self, sha256_hex: &str, bundle: &str) -> Result<SigstoreSigner, SignatureError> {
        let digest = hex32(sha256_hex).ok_or_else(|| SignatureError::Digest(sha256_hex.into()))?;
        match aigw_sigstore::verify(
            &self.root,
            &self.policy,
            bundle.as_bytes(),
            aigw_sigstore::Artifact::Sha256(digest),
        ) {
            Ok(v) => Ok(SigstoreSigner {
                subject: v.subject,
                issuer: v.issuer,
                integrated_time: v.integrated_time,
                log_index: v.log_index,
            }),
            Err(aigw_sigstore::Error::Identity { subject, issuer }) => {
                Err(SignatureError::Identity { subject, issuer })
            }
            Err(e) => Err(SignatureError::Sigstore(e.to_string())),
        }
    }
}

fn hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

fn compile_identities(
    ids: &[crate::config::SigstoreIdentity],
) -> Result<Vec<aigw_sigstore::Identity>, String> {
    if ids.is_empty() {
        return Err("wasm.sigstore.identities: mindestens eine Identität nötig".into());
    }
    ids.iter()
        .enumerate()
        .map(|(i, id)| {
            aigw_sigstore::Identity::new(&id.issuer, &id.subject)
                .map_err(|e| format!("wasm.sigstore.identities[{i}]: {e}"))
        })
        .collect()
}

/// Höchstgröße eines Sigstore-Bundles (Byte).
pub const MAX_SIGSTORE_BUNDLE_BYTES: usize = aigw_sigstore::MAX_BUNDLE_BYTES;

/// Prüft nur das Format eines Sigstore-Bundles (lesbar, unterstützte Art; ohne Trust-Root).
pub fn check_bundle_format(bundle: &str) -> Result<(), String> {
    aigw_sigstore::check_format(bundle.as_bytes()).map_err(|e| format!("sigstoreBundle: {e}"))
}

/// Prüft die Signatur-Konfiguration ohne Dateizugriff (Konfigurationsprüfung): Schlüssel,
/// Identitäten, Profil. Das Trust-Root liest erst [`TrustPolicy::from_config`].
pub fn check_policy_config(cfg: &crate::config::WasmConfig) -> Result<(), String> {
    // Mit Sigstore darf `require_signature: true` auch ohne Ed25519-Schlüssel stehen.
    let require = if cfg.sigstore.is_some() {
        None
    } else {
        cfg.require_signature
    };
    let p = TrustPolicy::new(&cfg.trusted_keys, require)?;
    if let Some(s) = &cfg.sigstore {
        if s.trusted_root.trim().is_empty() {
            return Err("wasm.sigstore.trusted_root: Pfad fehlt".into());
        }
        compile_identities(&s.identities)?;
    }
    strict_check(cfg, p.active() || cfg.sigstore.is_some())
}

fn strict_check(cfg: &crate::config::WasmConfig, active: bool) -> Result<(), String> {
    if cfg.profile == crate::config::WasmProfile::Strict {
        if !active {
            return Err(
                "wasm.profile: strict verlangt Signaturen – wasm.trusted_keys oder wasm.sigstore \
                 konfigurieren"
                    .into(),
            );
        }
        if cfg.require_signature == Some(false) {
            return Err("wasm.profile: strict erlaubt kein require_signature: false".into());
        }
    }
    Ok(())
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
            sigstore: None,
        })
    }

    /// Ergänzt Sigstore (AIGW-106). Ohne ausdrückliches `require_signature` ist die Signatur
    /// danach Pflicht (wie bei Schlüsseln).
    pub fn with_sigstore(
        mut self,
        sigstore: SigstoreTrust,
        require_signature: Option<bool>,
    ) -> Self {
        self.sigstore = Some(std::sync::Arc::new(sigstore));
        self.required = require_signature.unwrap_or(true);
        self
    }

    /// Aus der WASM-Konfiguration (liest das Sigstore-Trust-Root aus der Datei).
    pub fn from_config(cfg: &crate::config::WasmConfig) -> Result<Self, String> {
        check_policy_config(cfg)?;
        let mut p = match &cfg.sigstore {
            Some(_) => Self::new(&cfg.trusted_keys, None)?,
            None => Self::new(&cfg.trusted_keys, cfg.require_signature)?,
        };
        if let Some(s) = &cfg.sigstore {
            p = p.with_sigstore(SigstoreTrust::from_config(s)?, cfg.require_signature);
        }
        if cfg.profile == crate::config::WasmProfile::Strict {
            p.required = true;
        }
        Ok(p)
    }

    /// Signaturprüfung aktiv (mindestens ein Schlüssel oder Sigstore)?
    pub fn active(&self) -> bool {
        !self.keys.is_empty() || self.sigstore.is_some()
    }

    /// Sigstore-Teil (falls konfiguriert).
    pub fn sigstore(&self) -> Option<&SigstoreTrust> {
        self.sigstore.as_deref()
    }

    /// Sind Signaturen Pflicht?
    pub fn required(&self) -> bool {
        self.required
    }

    /// IDs der vertrauenswürdigen Schlüssel.
    pub fn key_ids(&self) -> impl Iterator<Item = &str> {
        self.keys.iter().map(|(id, _)| id.as_str())
    }

    /// Prüft eine (optionale) Ed25519-Signatur über den SHA-256 (hex, klein) eines Moduls
    /// (ohne Sigstore-Bundle; siehe [`Self::verify_all`]).
    pub fn verify(
        &self,
        sha256_hex: &str,
        sig: Option<&ModuleSignature>,
    ) -> Result<Verified, SignatureError> {
        self.verify_all(sha256_hex, sig, None)
    }

    /// Prüft alle Signaturen eines Moduls (AIGW-106): Ed25519-Signatur und Sigstore-Bundle.
    /// Jede **vorhandene** Signatur muss gültig sein; ist die Signatur Pflicht, muss mindestens
    /// eine vorhanden sein.
    pub fn verify_all(
        &self,
        sha256_hex: &str,
        sig: Option<&ModuleSignature>,
        bundle: Option<&str>,
    ) -> Result<Verified, SignatureError> {
        if !self.active() {
            return Ok(Verified::Unchecked);
        }
        let signer = match bundle {
            Some(b) => Some(self.verify_sigstore(sha256_hex, b)?),
            None => None,
        };
        let Some(sig) = sig else {
            return match signer {
                Some(s) => Ok(Verified::Sigstore(s)),
                None if self.required => Err(SignatureError::Missing),
                None => Ok(Verified::Unsigned),
            };
        };
        self.verify_ed25519(sha256_hex, sig)
    }

    /// Prüft nur ein Sigstore-Bundle (Anzeige, Upload).
    pub fn verify_sigstore(
        &self,
        sha256_hex: &str,
        bundle: &str,
    ) -> Result<SigstoreSigner, SignatureError> {
        match &self.sigstore {
            Some(s) => s.verify(sha256_hex, bundle),
            None => Err(SignatureError::Sigstore(
                "wasm.sigstore ist nicht konfiguriert".into(),
            )),
        }
    }

    fn verify_ed25519(
        &self,
        sha256_hex: &str,
        sig: &ModuleSignature,
    ) -> Result<Verified, SignatureError> {
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
        self.verify_all(
            &m.sha256,
            m.signature.as_ref(),
            m.sigstore_bundle.as_deref(),
        )
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
            sigstore_bundle: None,
        };
        let p = TrustPolicy::new(&[key("release", TEST_PUB)], None).unwrap();
        assert!(p.check_module(&m).is_ok());
        m.content = Bytes::from_static(b"\0asm\x0d\0\x01\0getauscht");
        assert_eq!(p.check_module(&m), Err(SignatureError::Digest(sha)));
    }

    const SIGSTORE_DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../aigw-sigstore/tests/data");

    fn sigstore(subject: &str) -> SigstoreTrust {
        let root = std::fs::read(format!("{SIGSTORE_DATA}/trusted_root.public-good.json")).unwrap();
        SigstoreTrust::new(
            &root,
            &[crate::config::SigstoreIdentity {
                issuer: r"https://token\.actions\.githubusercontent\.com".into(),
                subject: subject.into(),
            }],
            true,
        )
        .unwrap()
    }

    fn bundle() -> (String, String) {
        let b = std::fs::read_to_string(format!(
            "{SIGSTORE_DATA}/conformance/happy-path-v0.3/bundle.sigstore.json"
        ))
        .unwrap();
        let artifact = std::fs::read(format!("{SIGSTORE_DATA}/conformance/a.txt")).unwrap();
        (sha256_hex(&artifact), b)
    }

    #[test]
    fn sigstore_policy() {
        let (sha, b) = bundle();
        let p = TrustPolicy::default().with_sigstore(
            sigstore(r"https://github\.com/sigstore-conformance/.*"),
            None,
        );
        assert!(p.active() && p.required());
        match p.verify_all(&sha, None, Some(&b)) {
            Ok(Verified::Sigstore(s)) => {
                assert!(
                    s.subject
                        .starts_with("https://github.com/sigstore-conformance/")
                );
                assert_eq!(s.issuer, "https://token.actions.githubusercontent.com");
            }
            other => panic!("{other:?}"),
        }
        // Ohne Signatur: Pflicht.
        assert_eq!(p.verify_all(&sha, None, None), Err(SignatureError::Missing));
        // Anderer Inhalt.
        assert!(matches!(
            p.verify_all(&sha256_hex(b"anders"), None, Some(&b)),
            Err(SignatureError::Sigstore(_))
        ));
        // Ed25519-Signatur ohne Schlüssel: vorhandene Signatur muss gültig sein.
        let der = private_key_der(TEST_KEY.as_bytes()).unwrap();
        let sig = signed(&der, "release", &sha);
        assert_eq!(
            p.verify_all(&sha, Some(&sig), Some(&b)),
            Err(SignatureError::UnknownKey("release".into()))
        );
        // Beide Verfahren gültig.
        let both = TrustPolicy::new(&[key("release", TEST_PUB)], None)
            .unwrap()
            .with_sigstore(sigstore(".*"), None);
        assert_eq!(
            both.verify_all(&sha, Some(&sig), Some(&b)),
            Ok(Verified::Valid {
                key_id: "release".into()
            })
        );
        assert!(matches!(
            both.verify_all(&sha, None, Some(&b)),
            Ok(Verified::Sigstore(_))
        ));
        // Falsche Identität.
        let other = TrustPolicy::default()
            .with_sigstore(sigstore(r"https://github\.com/mupf-dev/.*"), None);
        assert!(matches!(
            other.verify_all(&sha, None, Some(&b)),
            Err(SignatureError::Identity { .. })
        ));
        // Bundle, aber Sigstore nicht konfiguriert (nur Schlüssel).
        let keys = TrustPolicy::new(&[key("release", TEST_PUB)], None).unwrap();
        assert!(matches!(
            keys.verify_all(&sha, None, Some(&b)),
            Err(SignatureError::Sigstore(_))
        ));
        // Übergangsmodus: unsigniert erlaubt.
        let lax = TrustPolicy::default().with_sigstore(sigstore(".*"), Some(false));
        assert_eq!(lax.verify_all(&sha, None, None), Ok(Verified::Unsigned));
    }

    #[test]
    fn profile_and_config_checks() {
        use crate::config::{SigstoreConfig, SigstoreIdentity, WasmConfig, WasmProfile};
        let mut c = WasmConfig::default();
        assert!(!c.four_eyes());
        c.profile = WasmProfile::Strict;
        assert!(c.four_eyes(), "strict: Vier-Augen an");
        assert!(
            check_policy_config(&c).is_err(),
            "strict ohne Signaturverfahren"
        );
        c.trusted_keys = vec![key("release", TEST_PUB)];
        assert!(check_policy_config(&c).is_ok());
        c.require_signature = Some(false);
        assert!(check_policy_config(&c).is_err(), "strict ohne Pflicht");
        c.require_signature = None;
        c.four_eyes = Some(false);
        assert!(!c.four_eyes());
        let p = TrustPolicy::from_config(&c).unwrap();
        assert!(p.required());
        // Sigstore: Identitäten Pflicht, reguläre Ausdrücke gültig, Trust-Root lesbar.
        let mut c = WasmConfig {
            sigstore: Some(SigstoreConfig {
                trusted_root: format!("{SIGSTORE_DATA}/trusted_root.public-good.json"),
                identities: vec![],
                require_inclusion_proof: true,
            }),
            require_signature: Some(true),
            ..WasmConfig::default()
        };
        assert!(check_policy_config(&c).is_err());
        let id = SigstoreIdentity {
            issuer: "(".into(),
            subject: ".*".into(),
        };
        c.sigstore.as_mut().unwrap().identities = vec![id];
        assert!(check_policy_config(&c).is_err());
        c.sigstore.as_mut().unwrap().identities[0].issuer = ".*".into();
        assert!(check_policy_config(&c).is_ok());
        let p = TrustPolicy::from_config(&c).unwrap();
        assert!(p.active() && p.required() && p.sigstore().is_some());
        c.sigstore.as_mut().unwrap().trusted_root = "/nicht/vorhanden.json".into();
        assert!(check_policy_config(&c).is_ok(), "ohne Dateizugriff");
        assert!(TrustPolicy::from_config(&c).is_err());
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
