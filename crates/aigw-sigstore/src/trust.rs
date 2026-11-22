// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Trust-Root (`trusted_root.json`, Sigstore-Protobuf-Spezifikation `TrustedRoot` als JSON).
//!
//! Verwendet werden Fulcio-Zertifizierungsstellen (`certificateAuthorities`), Rekor-Logs
//! (`tlogs`) und CT-Logs (`ctlogs`) mit ihren Gültigkeitszeiträumen (`validFor`, geschlossenes
//! Intervall). `timestampAuthorities` werden gelesen, aber nicht ausgewertet (keine
//! RFC-3161-Zeitstempel, siehe Crate-Doku).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::Error;
use crate::cert::PublicKey;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRoot {
    media_type: String,
    #[serde(default)]
    tlogs: Vec<RawLog>,
    #[serde(default)]
    certificate_authorities: Vec<RawCa>,
    #[serde(default)]
    ctlogs: Vec<RawLog>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLog {
    #[serde(default)]
    base_url: String,
    public_key: RawKey,
    log_id: RawLogId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawKey {
    raw_bytes: String,
    #[serde(default)]
    valid_for: Option<RawRange>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLogId {
    key_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCa {
    #[serde(default)]
    uri: String,
    cert_chain: RawChain,
    #[serde(default)]
    valid_for: Option<RawRange>,
}

#[derive(Deserialize)]
struct RawChain {
    certificates: Vec<RawCert>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCert {
    raw_bytes: String,
}

#[derive(Deserialize)]
struct RawRange {
    #[serde(default)]
    start: Option<String>,
    #[serde(default)]
    end: Option<String>,
}

/// Gültigkeitszeitraum (Unix-Sekunden, geschlossenes Intervall; ohne Ende = offen).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Validity {
    /// Beginn.
    pub start: i64,
    /// Ende (einschließlich).
    pub end: Option<i64>,
}

impl Validity {
    /// Liegt `t` im Zeitraum?
    pub fn contains(&self, t: i64) -> bool {
        t >= self.start && self.end.is_none_or(|e| t <= e)
    }
}

fn parse_time(s: &str, what: &str) -> Result<i64, Error> {
    OffsetDateTime::parse(s, &Rfc3339)
        .map(OffsetDateTime::unix_timestamp)
        .map_err(|e| Error::TrustRoot(format!("{what}: Zeitpunkt {s:?} ungültig ({e})")))
}

fn validity(r: Option<&RawRange>, what: &str) -> Result<Validity, Error> {
    let start = r
        .and_then(|r| r.start.as_deref())
        .ok_or_else(|| Error::TrustRoot(format!("{what}: validFor.start fehlt")))?;
    Ok(Validity {
        start: parse_time(start, what)?,
        end: r
            .and_then(|r| r.end.as_deref())
            .map(|e| parse_time(e, what))
            .transpose()?,
    })
}

fn b64(s: &str, what: &str) -> Result<Vec<u8>, Error> {
    STANDARD
        .decode(s.trim())
        .map_err(|_| Error::TrustRoot(format!("{what}: kein gültiges Base64")))
}

/// Ein Transparenz-Log (Rekor oder CT).
#[derive(Debug, Clone)]
pub struct Log {
    /// Basis-URL (nur zur Anzeige).
    pub base_url: String,
    /// Log-ID (SHA-256 des Schlüssels bei Rekor v1/CT).
    pub log_id: Vec<u8>,
    pub(crate) key: PublicKey,
    /// Gültigkeit des Schlüssels.
    pub valid_for: Validity,
}

/// Eine Zertifizierungsstelle (Fulcio): Kette von der Zwischen- bis zur Wurzel-CA.
#[derive(Debug, Clone)]
pub struct CertificateAuthority {
    /// URI (nur zur Anzeige).
    pub uri: String,
    /// Zertifikate (DER), erstes = ausstellende CA, letztes = Wurzel.
    pub chain: Vec<Vec<u8>>,
    /// Gültigkeit.
    pub valid_for: Validity,
}

/// Das Trust-Root: wem das Gateway bei der Bundle-Prüfung vertraut.
#[derive(Debug, Clone)]
pub struct TrustedRoot {
    /// Rekor-Logs.
    pub tlogs: Vec<Log>,
    /// Fulcio-Zertifizierungsstellen.
    pub certificate_authorities: Vec<CertificateAuthority>,
    /// CT-Logs (für SCTs der Fulcio-Zertifikate). Leer = SCT-Prüfung aus.
    pub ctlogs: Vec<Log>,
}

fn log(raw: &RawLog, what: &str) -> Result<Log, Error> {
    let what = format!("{what}[{}]", raw.base_url);
    let spki = b64(&raw.public_key.raw_bytes, &what)?;
    let key = PublicKey::from_spki(&spki).map_err(|e| Error::TrustRoot(format!("{what}: {e}")))?;
    Ok(Log {
        base_url: raw.base_url.clone(),
        log_id: b64(&raw.log_id.key_id, &what)?,
        key,
        valid_for: validity(raw.public_key.valid_for.as_ref(), &what)?,
    })
}

impl TrustedRoot {
    /// Liest ein `trusted_root.json`.
    pub fn from_json(json: &[u8]) -> Result<Self, Error> {
        let raw: RawRoot = serde_json::from_slice(json)
            .map_err(|e| Error::TrustRoot(format!("kein gültiges trusted_root.json: {e}")))?;
        if !raw
            .media_type
            .starts_with("application/vnd.dev.sigstore.trustedroot")
        {
            return Err(Error::TrustRoot(format!(
                "mediaType {:?} ist kein Sigstore-Trust-Root",
                raw.media_type
            )));
        }
        let tlogs = raw
            .tlogs
            .iter()
            .map(|l| log(l, "tlogs"))
            .collect::<Result<Vec<_>, _>>()?;
        let ctlogs = raw
            .ctlogs
            .iter()
            .map(|l| log(l, "ctlogs"))
            .collect::<Result<Vec<_>, _>>()?;
        let mut cas = Vec::new();
        for c in &raw.certificate_authorities {
            let what = format!("certificateAuthorities[{}]", c.uri);
            if c.cert_chain.certificates.is_empty() {
                return Err(Error::TrustRoot(format!("{what}: certChain leer")));
            }
            let chain = c
                .cert_chain
                .certificates
                .iter()
                .map(|x| b64(&x.raw_bytes, &what))
                .collect::<Result<Vec<_>, _>>()?;
            for der in &chain {
                crate::cert::Cert::parse(der)
                    .map_err(|e| Error::TrustRoot(format!("{what}: Zertifikat: {e}")))?;
            }
            cas.push(CertificateAuthority {
                uri: c.uri.clone(),
                chain,
                valid_for: validity(c.valid_for.as_ref(), &what)?,
            });
        }
        if tlogs.is_empty() || cas.is_empty() {
            return Err(Error::TrustRoot(
                "mindestens ein Rekor-Log (tlogs) und eine Zertifizierungsstelle \
                 (certificateAuthorities) nötig"
                    .into(),
            ));
        }
        Ok(Self {
            tlogs,
            certificate_authorities: cas,
            ctlogs,
        })
    }

    pub(crate) fn tlog(&self, log_id: &[u8]) -> Option<&Log> {
        self.tlogs.iter().find(|l| l.log_id == log_id)
    }

    pub(crate) fn ctlog(&self, log_id: &[u8]) -> Option<&Log> {
        self.ctlogs.iter().find(|l| l.log_id == log_id)
    }
}
