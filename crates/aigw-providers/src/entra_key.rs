// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Verschlüsselte PKCS#8-Schlüssel (`-----BEGIN ENCRYPTED PRIVATE KEY-----`, RFC 5958/8018) für
//! das Client-Zertifikat der Entra-ID-Anmeldung (AIGW-94).
//!
//! Unterstützt wird PBES2 mit PBKDF2 (PRF HMAC-SHA1/-SHA256/-SHA384/-SHA512) und AES-128/-192/
//! -256-CBC – das, was `openssl pkcs8 -topk8 -v2 aes-256-cbc`, `openssl genpkey -aes-256-cbc`,
//! .NET und Azure Key Vault (PEM-Export) erzeugen. Nicht unterstützt (mit klarer Meldung):
//! PBES1 (z. B. `PBE-SHA1-3DES`), scrypt, das alte OpenSSL-Format mit `Proc-Type: 4,ENCRYPTED`.
//!
//! Eigene, minimale DER-Auswertung (nur die benötigten Typen, Längen geprüft); Kryptografie aus
//! aws-lc-rs. Fehlermeldungen nennen nie Passwort oder Schlüsselmaterial.

use std::num::NonZeroU32;

use aws_lc_rs::cipher::{
    AES_128, AES_192, AES_256, DecryptionContext, PaddedBlockDecryptingKey, UnboundCipherKey,
};
use aws_lc_rs::iv::FixedLength;
use aws_lc_rs::pbkdf2;

/// Höchstzahl der PBKDF2-Iterationen (Schutz vor teuren Schlüsseln beim Konfigurationsaufbau;
/// OpenSSL nimmt 2048, .NET/Key Vault bis 600 000).
pub(crate) const MAX_ITERATIONS: u32 = 2_000_000;

const OID_PBES2: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x05, 0x0d];
const OID_PBKDF2: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x05, 0x0c];
const OID_SCRYPT: &[u8] = &[0x2b, 0x06, 0x01, 0x04, 0x01, 0xda, 0x47, 0x04, 0x0b];
/// `1.2.840.113549.1.12.1.*` (PKCS#12-PBE) bzw. `1.2.840.113549.1.5.{1,3,4,6,10,11}` (PBES1).
const OID_PKCS12_PBE_PREFIX: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x0c, 0x01];
const OID_PKCS5_PREFIX: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x05];
const OID_HMAC_SHA1: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x07];
const OID_HMAC_SHA256: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x09];
const OID_HMAC_SHA384: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x0a];
const OID_HMAC_SHA512: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x0b];
const OID_AES128_CBC: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x01, 0x02];
const OID_AES192_CBC: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x01, 0x16];
const OID_AES256_CBC: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x01, 0x2a];

const SEQUENCE: u8 = 0x30;
const INTEGER: u8 = 0x02;
const OCTET_STRING: u8 = 0x04;
const NULL: u8 = 0x05;
const OID: u8 = 0x06;

/// Fehler beim Entschlüsseln (ohne Geheimnisse).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeyError {
    /// DER-Struktur ungültig.
    Malformed(&'static str),
    /// Verfahren nicht unterstützt.
    Unsupported(&'static str),
    /// Passwort falsch oder Daten beschädigt (Padding/Struktur nach dem Entschlüsseln).
    Decrypt,
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(what) => write!(f, "verschlüsselter Schlüssel ungültig ({what})"),
            Self::Unsupported(what) => write!(
                f,
                "Verschlüsselung des Schlüssels nicht unterstützt ({what}; erwartet PBES2 mit \
                 PBKDF2 und AES-CBC, z. B. openssl pkcs8 -topk8 -v2 aes-256-cbc)"
            ),
            Self::Decrypt => f.write_str(
                "privater Schlüssel ließ sich nicht entschlüsseln (Passwort falsch oder Schlüssel \
                 beschädigt)",
            ),
        }
    }
}

/// Minimaler DER-Leser (definite Längen, höchstens 4 Längenbytes).
struct Der<'a>(&'a [u8]);

impl<'a> Der<'a> {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn peek_tag(&self) -> Option<u8> {
        self.0.first().copied()
    }

    /// Nächstes Element mit `tag`; liefert den Inhalt.
    fn read(&mut self, tag: u8, what: &'static str) -> Result<&'a [u8], KeyError> {
        let bad = KeyError::Malformed(what);
        let (&t, rest) = self.0.split_first().ok_or(bad.clone())?;
        if t != tag {
            return Err(bad);
        }
        let (&l0, mut rest) = rest.split_first().ok_or(bad.clone())?;
        let len = if l0 < 0x80 {
            usize::from(l0)
        } else {
            let n = usize::from(l0 & 0x7f);
            if n == 0 || n > 4 || rest.len() < n {
                return Err(bad);
            }
            let mut len = 0usize;
            for &b in &rest[..n] {
                len = (len << 8) | usize::from(b);
            }
            rest = &rest[n..];
            len
        };
        if rest.len() < len {
            return Err(bad);
        }
        let (content, after) = rest.split_at(len);
        self.0 = after;
        Ok(content)
    }

    fn seq(&mut self, what: &'static str) -> Result<Der<'a>, KeyError> {
        self.read(SEQUENCE, what).map(Der)
    }

    /// Nicht negative Ganzzahl bis `u32::MAX`.
    fn u32(&mut self, what: &'static str) -> Result<u32, KeyError> {
        let bytes = self.read(INTEGER, what)?;
        if bytes.is_empty() || bytes[0] & 0x80 != 0 {
            return Err(KeyError::Malformed(what));
        }
        let bytes = match bytes {
            [0, rest @ ..] if !rest.is_empty() => rest,
            b => b,
        };
        if bytes.len() > 4 {
            return Err(KeyError::Unsupported("Zahl zu groß"));
        }
        Ok(bytes.iter().fold(0u32, |n, &b| (n << 8) | u32::from(b)))
    }
}

/// Entschlüsselt `EncryptedPrivateKeyInfo` (DER) mit `password`; liefert `PrivateKeyInfo`
/// (PKCS#8, DER).
pub(crate) fn decrypt_pkcs8(der: &[u8], password: &[u8]) -> Result<Vec<u8>, KeyError> {
    let mut outer = Der(der);
    let mut info = outer.seq("EncryptedPrivateKeyInfo")?;
    if !outer.is_empty() {
        return Err(KeyError::Malformed("Daten nach dem Schlüssel"));
    }
    let mut alg = info.seq("AlgorithmIdentifier")?;
    let encrypted = info.read(OCTET_STRING, "encryptedData")?;
    let oid = alg.read(OID, "Verfahren")?;
    if oid != OID_PBES2 {
        return Err(KeyError::Unsupported(
            if oid.starts_with(OID_PKCS12_PBE_PREFIX) || oid.starts_with(OID_PKCS5_PREFIX) {
                "PBES1/PKCS#12-PBE"
            } else {
                "unbekanntes Verfahren"
            },
        ));
    }
    let mut params = alg.seq("PBES2-params")?;
    let mut kdf = params.seq("keyDerivationFunc")?;
    let mut enc = params.seq("encryptionScheme")?;

    // Verschlüsselung zuerst: bestimmt die Schlüssellänge.
    let (cipher, key_len) = match enc.read(OID, "encryptionScheme")? {
        o if o == OID_AES128_CBC => (&AES_128, 16),
        o if o == OID_AES192_CBC => (&AES_192, 24),
        o if o == OID_AES256_CBC => (&AES_256, 32),
        _ => return Err(KeyError::Unsupported("Verschlüsselung (nur AES-CBC)")),
    };
    let iv: [u8; 16] = enc
        .read(OCTET_STRING, "IV")?
        .try_into()
        .map_err(|_| KeyError::Malformed("IV"))?;

    match kdf.read(OID, "keyDerivationFunc")? {
        o if o == OID_PBKDF2 => {}
        o if o == OID_SCRYPT => return Err(KeyError::Unsupported("scrypt")),
        _ => return Err(KeyError::Unsupported("Schlüsselableitung (nur PBKDF2)")),
    }
    let mut p = kdf.seq("PBKDF2-params")?;
    let salt = p.read(OCTET_STRING, "Salt")?;
    let iterations = p.u32("Iterationen")?;
    if p.peek_tag() == Some(INTEGER) {
        let declared = p.u32("Schlüssellänge")?;
        if usize::try_from(declared).ok() != Some(key_len) {
            return Err(KeyError::Malformed("Schlüssellänge"));
        }
    }
    let prf = if p.peek_tag() == Some(SEQUENCE) {
        let mut a = p.seq("PRF")?;
        let o = a.read(OID, "PRF")?;
        if a.peek_tag() == Some(NULL) {
            a.read(NULL, "PRF")?;
        }
        match o {
            o if o == OID_HMAC_SHA1 => pbkdf2::PBKDF2_HMAC_SHA1,
            o if o == OID_HMAC_SHA256 => pbkdf2::PBKDF2_HMAC_SHA256,
            o if o == OID_HMAC_SHA384 => pbkdf2::PBKDF2_HMAC_SHA384,
            o if o == OID_HMAC_SHA512 => pbkdf2::PBKDF2_HMAC_SHA512,
            _ => return Err(KeyError::Unsupported("PRF (nur HMAC-SHA1/-SHA2)")),
        }
    } else {
        // DEFAULT algid-hmacWithSHA1 (RFC 8018).
        pbkdf2::PBKDF2_HMAC_SHA1
    };
    let iterations = NonZeroU32::new(iterations).ok_or(KeyError::Malformed("Iterationen"))?;
    if iterations.get() > MAX_ITERATIONS {
        return Err(KeyError::Unsupported(
            "mehr als 2 000 000 PBKDF2-Iterationen",
        ));
    }
    if salt.is_empty() || encrypted.is_empty() || encrypted.len() % 16 != 0 {
        return Err(KeyError::Malformed("Salt bzw. Länge der Daten"));
    }

    let mut key = [0u8; 32];
    let key = &mut key[..key_len];
    pbkdf2::derive(prf, iterations, salt, password, key);
    let unbound = UnboundCipherKey::new(cipher, key).map_err(|_| KeyError::Decrypt)?;
    key.fill(0);
    let dec = PaddedBlockDecryptingKey::cbc_pkcs7(unbound).map_err(|_| KeyError::Decrypt)?;
    let mut buf = encrypted.to_vec();
    let len = dec
        .decrypt(&mut buf, DecryptionContext::Iv128(FixedLength::from(iv)))
        .map_err(|_| KeyError::Decrypt)?
        .len();
    buf.truncate(len);
    // Ein falsches Passwort ergibt meist schon falsches Padding, sonst Datenmüll: Struktur
    // prüfen (SEQUENCE über alles).
    let mut check = Der(&buf);
    if check.seq("PrivateKeyInfo").is_err() || !check.is_empty() {
        buf.fill(0);
        return Err(KeyError::Decrypt);
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn der_lengths_are_checked() {
        assert!(Der(&[0x30, 0x05, 0x01]).seq("x").is_err());
        assert!(Der(&[0x30, 0x85, 0, 0, 0, 0, 1]).seq("x").is_err());
        assert!(Der(&[0x30, 0x80]).seq("x").is_err());
        let mut d = Der(&[0x30, 0x81, 0x01, 0x05, 0x02, 0x01, 0x07]);
        assert_eq!(d.seq("x").unwrap().0, &[0x05]);
        assert_eq!(d.u32("n").unwrap(), 7);
        assert!(Der(&[0x02, 0x01, 0x80]).u32("n").is_err());
        assert!(Der(&[0x02, 0x05, 1, 0, 0, 0, 0]).u32("n").is_err());
        assert_eq!(Der(&[0x02, 0x02, 0x00, 0xff]).u32("n").unwrap(), 255);
    }

    #[test]
    fn rejects_garbage_without_panicking() {
        for input in [
            &[][..],
            &[0x30],
            &[0x30, 0x00],
            &[0x04, 0x00],
            &[0x30, 0x02, 0x30, 0x00],
        ] {
            assert!(decrypt_pkcs8(input, b"pw").is_err());
        }
    }

    /// PBES2-Hülle mit wählbaren Teilen (für Fehlerfälle).
    fn wrap(kdf_oid: &[u8], iterations: &[u8], cipher_oid: &[u8]) -> Vec<u8> {
        fn tlv(tag: u8, body: &[u8]) -> Vec<u8> {
            let mut v = vec![tag];
            if body.len() < 0x80 {
                v.push(u8::try_from(body.len()).unwrap());
            } else {
                v.push(0x82);
                v.extend_from_slice(&u16::try_from(body.len()).unwrap().to_be_bytes());
            }
            v.extend_from_slice(body);
            v
        }
        let pbkdf2 = [tlv(OCTET_STRING, &[1; 8]), tlv(INTEGER, iterations)].concat();
        let kdf = tlv(
            SEQUENCE,
            &[tlv(OID, kdf_oid), tlv(SEQUENCE, &pbkdf2)].concat(),
        );
        let enc = tlv(
            SEQUENCE,
            &[tlv(OID, cipher_oid), tlv(OCTET_STRING, &[0; 16])].concat(),
        );
        let params = tlv(SEQUENCE, &[kdf, enc].concat());
        let alg = tlv(SEQUENCE, &[tlv(OID, OID_PBES2), params].concat());
        tlv(SEQUENCE, &[alg, tlv(OCTET_STRING, &[0; 32])].concat())
    }

    #[test]
    fn unsupported_and_limits() {
        let ok = wrap(OID_PBKDF2, &[0x08, 0x00], OID_AES256_CBC);
        assert_eq!(decrypt_pkcs8(&ok, b"pw"), Err(KeyError::Decrypt));
        assert!(matches!(
            decrypt_pkcs8(&wrap(OID_SCRYPT, &[1], OID_AES256_CBC), b"pw"),
            Err(KeyError::Unsupported("scrypt"))
        ));
        assert!(matches!(
            decrypt_pkcs8(&wrap(OID_PBKDF2, &[1], OID_HMAC_SHA1), b"pw"),
            Err(KeyError::Unsupported(_))
        ));
        // 0 und zu viele Iterationen.
        assert!(decrypt_pkcs8(&wrap(OID_PBKDF2, &[0], OID_AES128_CBC), b"pw").is_err());
        let e = decrypt_pkcs8(
            &wrap(OID_PBKDF2, &[0x7f, 0xff, 0xff, 0xff], OID_AES128_CBC),
            b"pw",
        )
        .unwrap_err();
        assert!(e.to_string().contains("2 000 000"), "{e}");
    }

    fn fixture_der(name: &str) -> Vec<u8> {
        let pem = match name {
            "aes256" => include_str!("../tests/fixtures/entra/client-aes256-sha256.testpem"),
            "aes128" => include_str!("../tests/fixtures/entra/client-aes128-sha1.testpem"),
            _ => include_str!("../tests/fixtures/entra/client-pbes1.testpem"),
        };
        let begin = "-----BEGIN ENCRYPTED PRIVATE KEY-----";
        let body = &pem[pem.find(begin).unwrap() + begin.len()..];
        let body = &body[..body.find("-----END").unwrap()];
        crate::codec::base64_decode(body).unwrap()
    }

    /// Mit OpenSSL 3 erzeugt (Passwort `test-passwort`, siehe tests/azure_entra.rs).
    #[test]
    fn decrypts_openssl_pbes2_fixtures() {
        for name in ["aes256", "aes128"] {
            let der = fixture_der(name);
            let plain = decrypt_pkcs8(&der, b"test-passwort").unwrap();
            aws_lc_rs::signature::RsaKeyPair::from_pkcs8(&plain).unwrap();
            let e = decrypt_pkcs8(&der, b"qwertz-pw").unwrap_err();
            assert_eq!(e, KeyError::Decrypt, "{name}");
            assert!(!e.to_string().contains("qwertz"));
        }
        let e = decrypt_pkcs8(&fixture_der("pbes1"), b"test-passwort").unwrap_err();
        assert_eq!(e, KeyError::Unsupported("PBES1/PKCS#12-PBE"));
    }
}
