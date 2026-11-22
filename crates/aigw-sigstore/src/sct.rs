// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Eingebettete SCTs (RFC 6962 §3.2/§3.3) eines Fulcio-Zertifikats gegen die CT-Logs des
//! Trust-Roots: mindestens ein SCT eines bekannten, zum Zeitpunkt gültigen Logs muss stimmen.

use crate::Error;
use crate::cert::Cert;
use crate::trust::TrustedRoot;

struct Tls<'a>(&'a [u8]);

impl<'a> Tls<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if self.0.len() < n {
            return Err(Error::Sct("SCT-Liste abgeschnitten".into()));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }

    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<usize, Error> {
        let b = self.take(2)?;
        Ok(usize::from(u16::from_be_bytes([b[0], b[1]])))
    }

    fn u64(&mut self) -> Result<u64, Error> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_be_bytes(a))
    }

    fn vec16(&mut self) -> Result<&'a [u8], Error> {
        let n = self.u16()?;
        self.take(n)
    }
}

/// Prüft die SCTs. `issuer_spki` = SubjectPublicKeyInfo (DER) der ausstellenden CA.
pub(crate) fn verify(root: &TrustedRoot, leaf: &Cert<'_>, issuer_spki: &[u8]) -> Result<(), Error> {
    let list = leaf
        .sct_list()?
        .ok_or_else(|| Error::Sct("Zertifikat ohne eingebettete SCTs".into()))?;
    let mut outer = Tls(list);
    let mut scts = Tls(outer.vec16()?);
    let tbs = leaf.tbs_without_scts()?;
    let tbs_len = u32::try_from(tbs.len())
        .ok()
        .filter(|n| *n < (1 << 24))
        .ok_or_else(|| Error::Sct("TBSCertificate zu groß".into()))?;
    let issuer_key_hash = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, issuer_spki);
    let mut known = false;
    while !scts.0.is_empty() {
        let mut s = Tls(scts.vec16()?);
        if s.u8()? != 0 {
            continue; // unbekannte SCT-Version
        }
        let log_id = s.take(32)?;
        let ts = s.u64()?;
        let ext = s.vec16()?;
        let (_hash, _alg) = (s.u8()?, s.u8()?);
        let sig = s.vec16()?;
        let Some(log) = root.ctlog(log_id) else {
            continue;
        };
        known = true;
        let secs = i64::try_from(ts / 1000).unwrap_or(i64::MAX);
        if !log.valid_for.contains(secs) {
            continue;
        }
        let mut signed = Vec::with_capacity(tbs.len() + 64);
        signed.extend_from_slice(&[0, 0]); // v1, certificate_timestamp
        signed.extend_from_slice(&ts.to_be_bytes());
        signed.extend_from_slice(&[0, 1]); // precert_entry
        signed.extend_from_slice(issuer_key_hash.as_ref());
        signed.extend_from_slice(&tbs_len.to_be_bytes()[1..]);
        signed.extend_from_slice(&tbs);
        signed.extend_from_slice(&u16::try_from(ext.len()).unwrap_or(u16::MAX).to_be_bytes());
        signed.extend_from_slice(ext);
        if log.key.verify(&signed, sig) {
            return Ok(());
        }
    }
    Err(Error::Sct(if known {
        "kein gültiger SCT (Signatur oder Gültigkeitszeitraum des CT-Logs)".into()
    } else {
        "kein SCT eines CT-Logs aus dem Trust-Root".into()
    }))
}
