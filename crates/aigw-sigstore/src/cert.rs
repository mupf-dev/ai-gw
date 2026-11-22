// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Felder aus X.509-Zertifikaten (Fulcio): Aussteller/Subjekt (roh), Schlüssel,
//! Erweiterungen, SAN, Fulcio-Issuer-Erweiterung und eingebettete SCTs.
//!
//! Die **Kettenprüfung** (Signaturen, Gültigkeit, EKU, Basic Constraints) macht rustls-webpki;
//! dieses Modul liest nur Felder, die webpki nicht herausgibt.

use crate::Error;
use crate::der::{self, Reader};

/// SubjectAltName (2.5.29.17).
const OID_SAN: &[u8] = &[0x55, 0x1d, 0x11];
/// Eingebettete SCT-Liste (1.3.6.1.4.1.11129.2.4.2, RFC 6962 §3.3).
pub(crate) const OID_SCT_LIST: &[u8] =
    &[0x2b, 0x06, 0x01, 0x04, 0x01, 0xd6, 0x79, 0x02, 0x04, 0x02];
/// Fulcio: OIDC-Issuer (veraltet, Rohbytes) 1.3.6.1.4.1.57264.1.1.
const OID_FULCIO_ISSUER_V1: &[u8] = &[0x2b, 0x06, 0x01, 0x04, 0x01, 0x83, 0xbf, 0x30, 0x01, 0x01];
/// Fulcio: OIDC-Issuer (DER UTF8String) 1.3.6.1.4.1.57264.1.8.
const OID_FULCIO_ISSUER_V2: &[u8] = &[0x2b, 0x06, 0x01, 0x04, 0x01, 0x83, 0xbf, 0x30, 0x01, 0x08];
/// id-ecPublicKey.
const OID_EC_PUBLIC_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
/// prime256v1 / secp256r1.
const OID_P256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
/// secp384r1.
const OID_P384: &[u8] = &[0x2b, 0x81, 0x04, 0x00, 0x22];
/// Ed25519.
const OID_ED25519: &[u8] = &[0x2b, 0x65, 0x70];

/// Eine Erweiterung.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Extension<'a> {
    pub(crate) oid: &'a [u8],
    pub(crate) value: &'a [u8],
    /// Vollständige DER-Kodierung (für den Precert-Nachbau).
    pub(crate) raw: &'a [u8],
}

/// Ausgelesenes Zertifikat.
#[derive(Debug, Clone)]
pub(crate) struct Cert<'a> {
    pub(crate) der: &'a [u8],
    pub(crate) tbs: der::Tlv<'a>,
    pub(crate) issuer: &'a [u8],
    pub(crate) subject: &'a [u8],
    pub(crate) spki: &'a [u8],
    pub(crate) extensions: Vec<Extension<'a>>,
}

impl<'a> Cert<'a> {
    /// Zerlegt ein Zertifikat (DER).
    pub(crate) fn parse(der_bytes: &'a [u8]) -> Result<Self, Error> {
        let cert = der::single(der_bytes, der::SEQUENCE)?;
        let mut c = Reader::new(cert.value);
        let tbs = c.expect(der::SEQUENCE)?;
        let mut t = Reader::new(tbs.value);
        t.optional(0xa0)?; // version
        t.read()?; // serialNumber
        t.expect(der::SEQUENCE)?; // signature
        let issuer = t.expect(der::SEQUENCE)?.raw;
        t.expect(der::SEQUENCE)?; // validity
        let subject = t.expect(der::SEQUENCE)?.raw;
        let spki = t.expect(der::SEQUENCE)?.raw;
        t.optional(0x81)?; // issuerUniqueID
        t.optional(0x82)?; // subjectUniqueID
        let mut extensions = Vec::new();
        if let Some(ext) = t.optional(0xa3)? {
            let seq = der::single(ext.value, der::SEQUENCE)?;
            let mut r = Reader::new(seq.value);
            while !r.is_empty() {
                let e = r.expect(der::SEQUENCE)?;
                let mut er = Reader::new(e.value);
                let oid = er.expect(der::OID)?.value;
                er.optional(der::BOOLEAN)?;
                let value = er.expect(der::OCTET_STRING)?.value;
                extensions.push(Extension {
                    oid,
                    value,
                    raw: e.raw,
                });
            }
        }
        Ok(Self {
            der: der_bytes,
            tbs,
            issuer,
            subject,
            spki,
            extensions,
        })
    }

    fn extension(&self, oid: &[u8]) -> Option<&Extension<'a>> {
        self.extensions.iter().find(|e| e.oid == oid)
    }

    /// Selbst ausgestellt (Subjekt = Aussteller) – z. B. ein Wurzelzertifikat.
    pub(crate) fn self_issued(&self) -> bool {
        self.issuer == self.subject
    }

    /// Identitäten aus dem SubjectAltName: E-Mail (rfc822Name), URI und `otherName` mit
    /// UTF8String (Fulcio-Nutzername). Andere Arten werden übergangen.
    pub(crate) fn san_identities(&self) -> Result<Vec<String>, Error> {
        let Some(ext) = self.extension(OID_SAN) else {
            return Ok(Vec::new());
        };
        let names = der::single(ext.value, der::SEQUENCE)?;
        let mut r = Reader::new(names.value);
        let mut out = Vec::new();
        while !r.is_empty() {
            let n = r.read()?;
            match n.tag {
                // rfc822Name [1] IA5String, uniformResourceIdentifier [6] IA5String
                0x81 | 0x86 => out.push(ascii(n.value)?),
                // otherName [0] { type-id OID, value [0] EXPLICIT ANY }
                0xa0 => {
                    let mut o = Reader::new(n.value);
                    o.expect(der::OID)?;
                    if let Some(v) = o.optional(0xa0)?
                        && let Ok(s) = der::single(v.value, der::UTF8_STRING)
                    {
                        out.push(utf8(s.value)?);
                    }
                }
                _ => {}
            }
        }
        Ok(out)
    }

    /// OIDC-Issuer aus der Fulcio-Erweiterung (neue Form bevorzugt).
    pub(crate) fn fulcio_issuer(&self) -> Result<Option<String>, Error> {
        if let Some(e) = self.extension(OID_FULCIO_ISSUER_V2) {
            let s = der::single(e.value, der::UTF8_STRING)?;
            return utf8(s.value).map(Some);
        }
        if let Some(e) = self.extension(OID_FULCIO_ISSUER_V1) {
            return utf8(e.value).map(Some);
        }
        Ok(None)
    }

    /// Inhalt der SCT-Liste (TLS-kodiert), falls vorhanden.
    pub(crate) fn sct_list(&self) -> Result<Option<&'a [u8]>, Error> {
        match self.extension(OID_SCT_LIST) {
            Some(e) => Ok(Some(der::single(e.value, der::OCTET_STRING)?.value)),
            None => Ok(None),
        }
    }

    /// TBSCertificate ohne die SCT-Erweiterung (Precertificate-Sicht, RFC 6962 §3.2).
    pub(crate) fn tbs_without_scts(&self) -> Result<Vec<u8>, Error> {
        let mut t = Reader::new(self.tbs.value);
        let mut body = Vec::with_capacity(self.tbs.value.len());
        while !t.is_empty() {
            let el = t.read()?;
            if el.tag != 0xa3 {
                body.extend_from_slice(el.raw);
                continue;
            }
            let mut exts = Vec::new();
            for e in &self.extensions {
                if e.oid != OID_SCT_LIST {
                    exts.extend_from_slice(e.raw);
                }
            }
            body.extend_from_slice(&der::encode(0xa3, &der::encode(der::SEQUENCE, &exts)));
        }
        Ok(der::encode(der::SEQUENCE, &body))
    }
}

fn ascii(b: &[u8]) -> Result<String, Error> {
    if b.is_ascii() {
        utf8(b)
    } else {
        Err(Error::Malformed("SAN: kein ASCII".into()))
    }
}

fn utf8(b: &[u8]) -> Result<String, Error> {
    String::from_utf8(b.to_vec()).map_err(|_| Error::Malformed("Zeichenkette kein UTF-8".into()))
}

/// Öffentlicher Schlüssel aus einem SubjectPublicKeyInfo (DER).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PublicKey {
    /// ECDSA P-256 (unkomprimierter Punkt).
    P256(Vec<u8>),
    /// ECDSA P-384 (unkomprimierter Punkt).
    P384(Vec<u8>),
    /// Ed25519 (32 Byte).
    Ed25519(Vec<u8>),
}

impl PublicKey {
    /// Liest ein SubjectPublicKeyInfo.
    pub(crate) fn from_spki(spki: &[u8]) -> Result<Self, Error> {
        let seq = der::single(spki, der::SEQUENCE)?;
        let mut r = Reader::new(seq.value);
        let alg = r.expect(der::SEQUENCE)?;
        let bits = r.expect(der::BIT_STRING)?.value;
        let (&unused, key) = bits
            .split_first()
            .ok_or_else(|| Error::Malformed("Schlüssel leer".into()))?;
        if unused != 0 {
            return Err(Error::Malformed(
                "Schlüssel: BIT STRING mit Restbits".into(),
            ));
        }
        let mut a = Reader::new(alg.value);
        let oid = a.expect(der::OID)?.value;
        if oid == OID_ED25519 {
            return Ok(Self::Ed25519(key.to_vec()));
        }
        if oid == OID_EC_PUBLIC_KEY {
            let curve = a.expect(der::OID)?.value;
            if curve == OID_P256 {
                return Ok(Self::P256(key.to_vec()));
            }
            if curve == OID_P384 {
                return Ok(Self::P384(key.to_vec()));
            }
        }
        Err(Error::Unsupported(
            "Schlüsselart (unterstützt: ECDSA P-256/P-384, Ed25519)".into(),
        ))
    }

    /// Prüft eine Signatur über `msg` (ECDSA: ASN.1/DER-Signatur; P-256 mit SHA-256, P-384
    /// mit SHA-384).
    pub(crate) fn verify(&self, msg: &[u8], sig: &[u8]) -> bool {
        use aws_lc_rs::signature::{
            ECDSA_P256_SHA256_ASN1, ECDSA_P384_SHA384_ASN1, ED25519, UnparsedPublicKey,
        };
        match self {
            Self::P256(k) => UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, k)
                .verify(msg, sig)
                .is_ok(),
            Self::P384(k) => UnparsedPublicKey::new(&ECDSA_P384_SHA384_ASN1, k)
                .verify(msg, sig)
                .is_ok(),
            Self::Ed25519(k) => UnparsedPublicKey::new(&ED25519, k).verify(msg, sig).is_ok(),
        }
    }

    /// Prüft eine ECDSA-P-256-Signatur über einen vorab berechneten SHA-256 (das Artefakt
    /// muss dafür nicht vorliegen). Andere Schlüsselarten: `None` (nicht unterstützt).
    pub(crate) fn verify_sha256_digest(&self, digest: &[u8; 32], sig: &[u8]) -> Option<bool> {
        use aws_lc_rs::signature::{ECDSA_P256_SHA256_ASN1, UnparsedPublicKey};
        match self {
            Self::P256(k) => {
                let d =
                    aws_lc_rs::digest::Digest::import_less_safe(digest, &aws_lc_rs::digest::SHA256)
                        .ok()?;
                Some(
                    UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, k)
                        .verify_digest(&d, sig)
                        .is_ok(),
                )
            }
            _ => None,
        }
    }
}
