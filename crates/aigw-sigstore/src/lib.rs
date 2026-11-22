// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Schlanke **Offline-Prüfung von Sigstore-Bundles** (keyless, AIGW-106).
//!
//! Geprüft wird ein Bundle (`application/vnd.dev.sigstore.bundle…`, v0.1–v0.3) mit
//! `messageSignature` über ein Artefakt, wie es `cosign sign-blob --bundle …` erzeugt:
//!
//! 1. **Signatur** des Artefakts mit dem Schlüssel des Fulcio-Zertifikats (ECDSA P-256 über
//!    SHA-256; mit [`Artifact::Sha256`] genügt der Digest). `messageDigest` muss passen.
//! 2. **Zertifikatskette** gegen die Fulcio-CAs des [`TrustedRoot`] (rustls-webpki: Signaturen,
//!    Gültigkeit **zum Zeitpunkt des Log-Eintrags**, EKU `codeSigning`, Basic Constraints).
//! 3. **SCT** (Certificate Transparency) – mindestens ein gültiger eingebetteter SCT, sofern das
//!    Trust-Root CT-Logs enthält.
//! 4. **Rekor-Eintrag** (v1, `hashedrekord` 0.0.1): bekanntes Log (Schlüssel zum Zeitpunkt
//!    gültig), **SET** (signierter Zeitstempel, Pflicht) und – Standard – **Inclusion-Proof**
//!    mit signiertem Checkpoint. Der Eintrag muss genau Digest, Signatur und Zertifikat des
//!    Bundles enthalten; der Zeitpunkt darf nicht in der Zukunft liegen.
//! 5. **Identität**: OIDC-Issuer (Fulcio-Erweiterung) und Subjekt (SAN: E-Mail, URI,
//!    Nutzername) gegen eine der konfigurierten [`Identity`] (reguläre Ausdrücke, vollständig
//!    verankert).
//!
//! Alles läuft **ohne Netz**: Trust-Root und Bundle sind Daten. Nicht unterstützt (→
//! [`Error::Unsupported`]): DSSE-Umschläge/Attestierungen, Rekor v2 (`hashedrekord` 0.0.2,
//! RFC-3161-Zeitstempel), Bundles mit festem Schlüssel statt Zertifikat.
//!
//! Warum eigen statt Crate `sigstore`: siehe `docs/konzept/17-wasm-plugins.md` §3b – kurz: das
//! Crate zieht ~30 weitere Crates (RustCrypto-Stapel inkl. `rsa` mit offenem RUSTSEC-Hinweis,
//! zweiter Krypto-Provider neben aws-lc-rs), hier genügen aws-lc-rs und rustls-webpki, die
//! ohnehin im Baum sind.

mod cert;
mod der;
mod rekor;
mod sct;
mod trust;

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use regex::Regex;
use serde::Deserialize;
use serde::de::Deserializer;

pub use trust::{CertificateAuthority, Log, TrustedRoot, Validity};

/// Fehler der Prüfung.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Bundle bzw. Zertifikat nicht lesbar.
    #[error("Bundle ungültig: {0}")]
    Malformed(String),
    /// Bundle-Art oder Verfahren nicht unterstützt.
    #[error("nicht unterstützt: {0}")]
    Unsupported(String),
    /// Trust-Root ungültig.
    #[error("Trust-Root: {0}")]
    TrustRoot(String),
    /// Signatur des Artefakts ungültig bzw. Digest passt nicht.
    #[error("Signatur: {0}")]
    Signature(String),
    /// Zertifikatskette nicht vertrauenswürdig.
    #[error("Zertifikat: {0}")]
    Certificate(String),
    /// SCT fehlt bzw. ungültig.
    #[error("Certificate Transparency: {0}")]
    Sct(String),
    /// Rekor-Eintrag ungültig (SET, Proof, Checkpoint, Inhalt, Zeit).
    #[error("Transparenz-Log: {0}")]
    Tlog(String),
    /// Kryptografisch gültig, aber Issuer/Subjekt passen zu keiner erlaubten Identität.
    #[error("Identität {subject:?} (Issuer {issuer:?}) nicht erlaubt")]
    Identity {
        /// Subjekt(e) aus dem Zertifikat.
        subject: String,
        /// OIDC-Issuer aus dem Zertifikat.
        issuer: String,
    },
}

/// Erlaubte Signier-Identität: OIDC-Issuer und Subjekt (SAN) als reguläre Ausdrücke. Beide
/// werden **vollständig** verglichen (`^(?:…)$`), ein Teiltreffer genügt nie.
#[derive(Debug, Clone)]
pub struct Identity {
    issuer: Regex,
    subject: Regex,
}

fn anchored(re: &str, what: &str) -> Result<Regex, String> {
    regex::RegexBuilder::new(&format!("^(?:{re})$"))
        .size_limit(1 << 20)
        .build()
        .map_err(|e| format!("{what}: kein gültiger regulärer Ausdruck ({e})"))
}

impl Identity {
    /// Baut eine Identität aus zwei regulären Ausdrücken.
    pub fn new(issuer: &str, subject: &str) -> Result<Self, String> {
        if issuer.trim().is_empty() || subject.trim().is_empty() {
            return Err("issuer und subject dürfen nicht leer sein".into());
        }
        Ok(Self {
            issuer: anchored(issuer, "issuer")?,
            subject: anchored(subject, "subject")?,
        })
    }

    fn matches(&self, issuer: &str, subjects: &[String]) -> bool {
        self.issuer.is_match(issuer) && subjects.iter().any(|s| self.subject.is_match(s))
    }
}

/// Prüfrichtlinie.
#[derive(Debug, Clone)]
pub struct Policy {
    /// Erlaubte Identitäten (mindestens eine muss passen).
    pub identities: Vec<Identity>,
    /// Inclusion-Proof (mit Checkpoint) Pflicht? Standard `true`; `false` lässt Bundles nur
    /// mit SET zu (ältere v0.1-Bundles, wie `cosign verify-blob --offline`).
    pub require_inclusion_proof: bool,
    /// Erlaubte Abweichung der Uhr: Log-Zeitpunkte bis zu so weit in der Zukunft gelten noch.
    pub clock_skew: Duration,
}

impl Policy {
    /// Richtlinie mit Standardwerten (Inclusion-Proof Pflicht, 60 s Uhrabweichung).
    pub fn new(identities: Vec<Identity>) -> Self {
        Self {
            identities,
            require_inclusion_proof: true,
            clock_skew: Duration::from_secs(60),
        }
    }
}

/// Das geprüfte Artefakt.
#[derive(Debug, Clone, Copy)]
pub enum Artifact<'a> {
    /// Inhalt.
    Bytes(&'a [u8]),
    /// Nur der SHA-256 (vom Aufrufer berechnet oder geprüft). Reicht für ECDSA P-256.
    Sha256([u8; 32]),
}

/// Ergebnis einer erfolgreichen Prüfung.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// Passendes Subjekt (SAN) des Zertifikats.
    pub subject: String,
    /// OIDC-Issuer des Zertifikats.
    pub issuer: String,
    /// Zeitpunkt des Log-Eintrags (Unix-Sekunden, aus dem SET).
    pub integrated_time: i64,
    /// Index im Rekor-Log.
    pub log_index: i64,
    /// Basis-URL des Logs (aus dem Trust-Root).
    pub log_url: String,
}

// ------------------------------------------------------------------ Bundle-Format (JSON)

fn int<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum N {
        S(String),
        I(i64),
    }
    match N::deserialize(d)? {
        N::I(i) => Ok(i),
        N::S(s) => s.parse().map_err(serde::de::Error::custom),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBundle {
    media_type: String,
    verification_material: RawMaterial,
    #[serde(default)]
    message_signature: Option<RawMessageSignature>,
    #[serde(default)]
    dsse_envelope: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMaterial {
    #[serde(default)]
    certificate: Option<RawCert>,
    #[serde(default)]
    x509_certificate_chain: Option<RawChain>,
    #[serde(default)]
    public_key: Option<serde_json::Value>,
    #[serde(default)]
    tlog_entries: Vec<RawEntry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCert {
    raw_bytes: String,
}

#[derive(Deserialize)]
struct RawChain {
    #[serde(default)]
    certificates: Vec<RawCert>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEntry {
    #[serde(deserialize_with = "int")]
    log_index: i64,
    log_id: RawLogId,
    kind_version: RawKindVersion,
    #[serde(default, deserialize_with = "int")]
    integrated_time: i64,
    #[serde(default)]
    inclusion_promise: Option<RawPromise>,
    #[serde(default)]
    inclusion_proof: Option<RawProof>,
    canonicalized_body: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLogId {
    key_id: String,
}

#[derive(Deserialize)]
struct RawKindVersion {
    kind: String,
    version: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPromise {
    signed_entry_timestamp: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawProof {
    #[serde(deserialize_with = "int")]
    log_index: i64,
    root_hash: String,
    #[serde(deserialize_with = "int")]
    tree_size: i64,
    #[serde(default)]
    hashes: Vec<String>,
    #[serde(default)]
    checkpoint: Option<RawCheckpoint>,
}

#[derive(Deserialize)]
struct RawCheckpoint {
    envelope: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMessageSignature {
    #[serde(default)]
    message_digest: Option<RawDigest>,
    signature: String,
}

#[derive(Deserialize)]
struct RawDigest {
    algorithm: String,
    digest: String,
}

const MEDIA_TYPES: [&str; 4] = [
    "application/vnd.dev.sigstore.bundle+json;version=0.1",
    "application/vnd.dev.sigstore.bundle+json;version=0.2",
    "application/vnd.dev.sigstore.bundle+json;version=0.3",
    "application/vnd.dev.sigstore.bundle.v0.3+json",
];

/// Höchstgröße eines Bundles (Byte).
pub const MAX_BUNDLE_BYTES: usize = 256 * 1024;

fn b64(s: &str, what: &str) -> Result<Vec<u8>, Error> {
    STANDARD
        .decode(s.trim())
        .map_err(|_| Error::Malformed(format!("{what}: kein gültiges Base64")))
}

/// Prüft nur das Format eines Bundles (lesbar, unterstützte Art) – ohne Trust-Root. Für
/// Eingaben, die erst später gegen eine Richtlinie geprüft werden.
pub fn check_format(bundle: &[u8]) -> Result<(), Error> {
    parse(bundle).map(|_| ())
}

fn parse(bundle: &[u8]) -> Result<RawBundle, Error> {
    if bundle.len() > MAX_BUNDLE_BYTES {
        return Err(Error::Malformed(format!(
            "größer als {MAX_BUNDLE_BYTES} Byte"
        )));
    }
    let b: RawBundle = serde_json::from_slice(bundle)
        .map_err(|e| Error::Malformed(format!("kein gültiges Sigstore-Bundle (JSON): {e}")))?;
    if !MEDIA_TYPES.contains(&b.media_type.as_str()) {
        return Err(Error::Unsupported(format!(
            "Bundle-Version {:?} (unterstützt: v0.1–v0.3)",
            b.media_type
        )));
    }
    if b.dsse_envelope.is_some() || b.message_signature.is_none() {
        return Err(Error::Unsupported(
            "nur messageSignature (cosign sign-blob), keine DSSE-Umschläge".into(),
        ));
    }
    if b.verification_material.public_key.is_some() {
        return Err(Error::Unsupported(
            "Bundle mit festem Schlüssel (keyless: Fulcio-Zertifikat erwartet)".into(),
        ));
    }
    Ok(b)
}

fn digest_of(a: Artifact<'_>) -> [u8; 32] {
    match a {
        Artifact::Sha256(d) => d,
        Artifact::Bytes(b) => {
            let mut out = [0u8; 32];
            out.copy_from_slice(aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, b).as_ref());
            out
        }
    }
}

static SIG_ALGS: &[&dyn rustls_pki_types::SignatureVerificationAlgorithm] = &[
    webpki::aws_lc_rs::ECDSA_P256_SHA256,
    webpki::aws_lc_rs::ECDSA_P256_SHA384,
    webpki::aws_lc_rs::ECDSA_P384_SHA256,
    webpki::aws_lc_rs::ECDSA_P384_SHA384,
    webpki::aws_lc_rs::ECDSA_P521_SHA512,
    webpki::aws_lc_rs::ED25519,
    webpki::aws_lc_rs::RSA_PKCS1_2048_8192_SHA256,
    webpki::aws_lc_rs::RSA_PKCS1_2048_8192_SHA384,
    webpki::aws_lc_rs::RSA_PKCS1_2048_8192_SHA512,
    webpki::aws_lc_rs::RSA_PSS_2048_8192_SHA256_LEGACY_KEY,
    webpki::aws_lc_rs::RSA_PSS_2048_8192_SHA384_LEGACY_KEY,
    webpki::aws_lc_rs::RSA_PSS_2048_8192_SHA512_LEGACY_KEY,
];

/// EKU `id-kp-codeSigning` (1.3.6.1.5.5.7.3.3).
const EKU_CODE_SIGNING: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x03];

/// Prüft die Kette des Blattzertifikats zum Zeitpunkt `t` gegen die CAs des Trust-Roots und
/// liefert den SubjectPublicKeyInfo der ausstellenden CA (für die SCT-Prüfung).
fn verify_chain(
    root: &TrustedRoot,
    leaf_der: &[u8],
    extra: &[Vec<u8>],
    t: i64,
) -> Result<Vec<u8>, Error> {
    use rustls_pki_types::{CertificateDer, UnixTime};
    let cas: Vec<&CertificateAuthority> = root
        .certificate_authorities
        .iter()
        .filter(|c| c.valid_for.contains(t))
        .collect();
    if cas.is_empty() {
        return Err(Error::Certificate(
            "keine Zertifizierungsstelle des Trust-Roots ist zum Zeitpunkt gültig".into(),
        ));
    }
    let anchor_ders: Vec<CertificateDer<'_>> = cas
        .iter()
        .filter_map(|c| c.chain.last())
        .map(|d| CertificateDer::from(d.as_slice()))
        .collect();
    let anchors = anchor_ders
        .iter()
        .map(|d| webpki::anchor_from_trusted_cert(d).map(|a| a.to_owned()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| Error::TrustRoot(format!("Wurzelzertifikat: {e}")))?;
    let mut intermediates: Vec<CertificateDer<'_>> = cas
        .iter()
        .flat_map(|c| c.chain.iter().take(c.chain.len().saturating_sub(1)))
        .map(|d| CertificateDer::from(d.as_slice()))
        .collect();
    intermediates.extend(extra.iter().map(|d| CertificateDer::from(d.as_slice())));
    let leaf = CertificateDer::from(leaf_der);
    let ee = webpki::EndEntityCert::try_from(&leaf)
        .map_err(|e| Error::Certificate(format!("nicht lesbar ({e})")))?;
    let time = UnixTime::since_unix_epoch(Duration::from_secs(u64::try_from(t).unwrap_or(0)));
    let path = ee
        .verify_for_usage(
            SIG_ALGS,
            &anchors,
            &intermediates,
            time,
            webpki::KeyUsage::required(EKU_CODE_SIGNING),
            None,
            None,
        )
        .map_err(|e| Error::Certificate(format!("nicht vertrauenswürdig ({e:?})")))?;
    Ok(path.issuer_spki().as_ref().to_vec())
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Prüft ein Bundle gegen Trust-Root und Richtlinie (Uhrzeit: Systemuhr).
pub fn verify(
    root: &TrustedRoot,
    policy: &Policy,
    bundle: &[u8],
    artifact: Artifact<'_>,
) -> Result<Verified, Error> {
    verify_at(root, policy, bundle, artifact, now_unix())
}

/// Wie [`verify`], mit vorgegebener aktueller Zeit (Unix-Sekunden; Tests).
pub fn verify_at(
    root: &TrustedRoot,
    policy: &Policy,
    bundle: &[u8],
    artifact: Artifact<'_>,
    now: i64,
) -> Result<Verified, Error> {
    let b = parse(bundle)?;
    let vm = &b.verification_material;
    let Some(ms) = &b.message_signature else {
        return Err(Error::Unsupported("messageSignature fehlt".into()));
    };
    // Zertifikat: v0.3 `certificate`, ältere Bundles `x509CertificateChain` (erstes = Blatt).
    let (leaf_der, extra) = match (&vm.certificate, &vm.x509_certificate_chain) {
        (Some(c), _) => (b64(&c.raw_bytes, "certificate")?, Vec::new()),
        (None, Some(ch)) => {
            let mut certs = ch
                .certificates
                .iter()
                .map(|c| b64(&c.raw_bytes, "x509CertificateChain"))
                .collect::<Result<Vec<_>, _>>()?;
            if certs.is_empty() {
                return Err(Error::Malformed("x509CertificateChain leer".into()));
            }
            let leaf = certs.remove(0);
            (leaf, certs)
        }
        (None, None) => return Err(Error::Malformed("Zertifikat fehlt".into())),
    };
    let leaf = cert::Cert::parse(&leaf_der)?;
    for d in std::iter::once(&leaf_der).chain(extra.iter()) {
        if cert::Cert::parse(d)?.self_issued() {
            return Err(Error::Certificate(
                "Bundle enthält ein Wurzelzertifikat (Wurzeln nur aus dem Trust-Root)".into(),
            ));
        }
    }
    debug_assert_eq!(leaf.der, leaf_der.as_slice());

    // 1. Signatur über das Artefakt.
    let digest = digest_of(artifact);
    if let Some(md) = &ms.message_digest {
        if md.algorithm != "SHA2_256" {
            return Err(Error::Unsupported(format!(
                "messageDigest {:?} (nur SHA2_256)",
                md.algorithm
            )));
        }
        if b64(&md.digest, "messageDigest")? != digest {
            return Err(Error::Signature(
                "messageDigest passt nicht zum Artefakt".into(),
            ));
        }
    }
    let sig = b64(&ms.signature, "messageSignature.signature")?;
    let key = cert::PublicKey::from_spki(leaf.spki)?;
    let ok = match artifact {
        Artifact::Bytes(bytes) => key.verify(bytes, &sig),
        Artifact::Sha256(d) => key.verify_sha256_digest(&d, &sig).ok_or_else(|| {
            Error::Unsupported("Prüfung über den Digest nur mit ECDSA P-256".into())
        })?,
    };
    if !ok {
        return Err(Error::Signature(
            "passt nicht zu Artefakt und Zertifikat".into(),
        ));
    }

    // 4. Rekor-Einträge (jeder muss stimmen, mindestens einer).
    if vm.tlog_entries.is_empty() {
        return Err(Error::Tlog("kein Rekor-Eintrag im Bundle".into()));
    }
    let mut first: Option<(i64, i64, String)> = None;
    for e in &vm.tlog_entries {
        if e.kind_version.kind != "hashedrekord" || e.kind_version.version != "0.0.1" {
            return Err(Error::Unsupported(format!(
                "Rekor-Eintrag {} {} (nur hashedrekord 0.0.1, Rekor v1)",
                e.kind_version.kind, e.kind_version.version
            )));
        }
        if e.log_index < 0 {
            return Err(Error::Tlog("negativer logIndex".into()));
        }
        let log_id = b64(&e.log_id.key_id, "logId")?;
        let log = root
            .tlog(&log_id)
            .ok_or_else(|| Error::Tlog("Log nicht im Trust-Root".into()))?;
        let t = e.integrated_time;
        if t <= 0 {
            return Err(Error::Tlog("integratedTime fehlt".into()));
        }
        let skew = i64::try_from(policy.clock_skew.as_secs()).unwrap_or(0);
        if t > now.saturating_add(skew) {
            return Err(Error::Tlog("integratedTime liegt in der Zukunft".into()));
        }
        if !log.valid_for.contains(t) {
            return Err(Error::Tlog(
                "Schlüssel des Logs ist zum Zeitpunkt nicht gültig".into(),
            ));
        }
        let body = b64(&e.canonicalized_body, "canonicalizedBody")?;
        rekor::check_body(&body, &digest, &sig, &leaf_der)?;
        let set = e
            .inclusion_promise
            .as_ref()
            .ok_or_else(|| Error::Tlog("Signed Entry Timestamp (inclusionPromise) fehlt".into()))?;
        rekor::verify_set(
            log,
            e.canonicalized_body.trim(),
            t,
            e.log_index,
            &set.signed_entry_timestamp,
        )?;
        match &e.inclusion_proof {
            Some(p) => rekor::verify_proof(
                log,
                &body,
                &rekor::Proof {
                    log_index: p.log_index,
                    tree_size: p.tree_size,
                    root_hash: &p.root_hash,
                    hashes: &p.hashes,
                    checkpoint: p.checkpoint.as_ref().map(|c| c.envelope.as_str()),
                },
            )?,
            None if policy.require_inclusion_proof => {
                return Err(Error::Tlog("Inclusion-Proof fehlt".into()));
            }
            None => {}
        }
        // 2./3. Kette und SCT zum Zeitpunkt des Eintrags.
        let issuer_spki = verify_chain(root, &leaf_der, &extra, t)?;
        if !root.ctlogs.is_empty() {
            sct::verify(root, &leaf, &issuer_spki)?;
        }
        if first.is_none() {
            first = Some((t, e.log_index, log.base_url.clone()));
        }
    }
    let (integrated_time, log_index, log_url) =
        first.ok_or_else(|| Error::Tlog("kein Rekor-Eintrag".into()))?;

    // 5. Identität.
    let issuer = leaf
        .fulcio_issuer()?
        .ok_or_else(|| Error::Certificate("OIDC-Issuer-Erweiterung fehlt".into()))?;
    let subjects = leaf.san_identities()?;
    if subjects.is_empty() {
        return Err(Error::Certificate("Zertifikat ohne Identität (SAN)".into()));
    }
    let Some(id) = policy
        .identities
        .iter()
        .find(|i| i.matches(&issuer, &subjects))
    else {
        return Err(Error::Identity {
            subject: subjects.join(", "),
            issuer,
        });
    };
    let subject = subjects
        .iter()
        .find(|s| id.subject.is_match(s))
        .cloned()
        .unwrap_or_default();
    Ok(Verified {
        subject,
        issuer,
        integrated_time,
        log_index,
        log_url,
    })
}

/// SHA-256 (Hilfsfunktion für Aufrufer).
pub fn sha256(b: &[u8]) -> [u8; 32] {
    digest_of(Artifact::Bytes(b))
}
