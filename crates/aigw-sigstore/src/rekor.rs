// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Rekor (v1): Signed Entry Timestamp (SET), Inclusion-Proof (RFC 9162 §2.1.3.2),
//! Checkpoint (C2SP signed note) und Abgleich des `hashedrekord`-Eintrags mit dem Bundle.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;

use crate::Error;
use crate::trust::Log;

fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut ctx = aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256);
    for p in parts {
        ctx.update(p);
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(ctx.finish().as_ref());
    out
}

pub(crate) fn hex(b: &[u8]) -> String {
    use std::fmt::Write as _;
    b.iter()
        .fold(String::with_capacity(b.len() * 2), |mut s, x| {
            let _ = write!(s, "{x:02x}");
            s
        })
}

/// Prüft das SET: Signatur des Logs über das kanonische JSON
/// `{"body":…,"integratedTime":…,"logID":…,"logIndex":…}` (Schlüssel sortiert, ohne Leerraum).
pub(crate) fn verify_set(
    log: &Log,
    body_b64: &str,
    integrated_time: i64,
    log_index: i64,
    set_b64: &str,
) -> Result<(), Error> {
    let sig = STANDARD
        .decode(set_b64.trim())
        .map_err(|_| Error::Tlog("signedEntryTimestamp: kein gültiges Base64".into()))?;
    let payload = format!(
        "{{\"body\":{},\"integratedTime\":{integrated_time},\"logID\":\"{}\",\"logIndex\":{log_index}}}",
        Value::String(body_b64.to_owned()),
        hex(&log.log_id)
    );
    if log.key.verify(payload.as_bytes(), &sig) {
        Ok(())
    } else {
        Err(Error::Tlog("Signed Entry Timestamp (SET) ungültig".into()))
    }
}

/// Merkle-Inclusion-Proof nach RFC 9162 §2.1.3.2.
pub(crate) fn verify_inclusion(
    index: u64,
    size: u64,
    leaf: [u8; 32],
    proof: &[[u8; 32]],
    root: &[u8; 32],
) -> bool {
    if index >= size {
        return false;
    }
    let (mut fnode, mut snode) = (index, size - 1);
    let mut r = leaf;
    for p in proof {
        if snode == 0 {
            return false;
        }
        if fnode & 1 == 1 || fnode == snode {
            r = sha256(&[&[1], p, &r]);
            if fnode & 1 == 0 {
                while fnode & 1 == 0 && fnode != 0 {
                    fnode >>= 1;
                    snode >>= 1;
                }
            }
        } else {
            r = sha256(&[&[1], &r, p]);
        }
        fnode >>= 1;
        snode >>= 1;
    }
    snode == 0 && r == *root
}

/// Inclusion-Proof eines Eintrags.
#[derive(Debug)]
pub(crate) struct Proof<'a> {
    pub(crate) log_index: i64,
    pub(crate) tree_size: i64,
    pub(crate) root_hash: &'a str,
    pub(crate) hashes: &'a [String],
    pub(crate) checkpoint: Option<&'a str>,
}

fn hash32(s: &str, what: &str) -> Result<[u8; 32], Error> {
    let v = STANDARD
        .decode(s.trim())
        .map_err(|_| Error::Tlog(format!("{what}: kein gültiges Base64")))?;
    <[u8; 32]>::try_from(v.as_slice()).map_err(|_| Error::Tlog(format!("{what}: 32 Byte erwartet")))
}

/// Prüft Proof und Checkpoint (Signatur des Logs über Baumgröße und Wurzel).
pub(crate) fn verify_proof(log: &Log, body: &[u8], p: &Proof<'_>) -> Result<(), Error> {
    let (Ok(index), Ok(size)) = (u64::try_from(p.log_index), u64::try_from(p.tree_size)) else {
        return Err(Error::Tlog(
            "inclusionProof: negativer Index bzw. Größe".into(),
        ));
    };
    let root = hash32(p.root_hash, "inclusionProof.rootHash")?;
    let hashes = p
        .hashes
        .iter()
        .map(|h| hash32(h, "inclusionProof.hashes"))
        .collect::<Result<Vec<_>, _>>()?;
    let leaf = sha256(&[&[0], body]);
    if !verify_inclusion(index, size, leaf, &hashes, &root) {
        return Err(Error::Tlog("Inclusion-Proof ungültig".into()));
    }
    let cp = p
        .checkpoint
        .ok_or_else(|| Error::Tlog("Inclusion-Proof ohne Checkpoint".into()))?;
    verify_checkpoint(log, cp, size, &root)
}

/// Checkpoint (signierte Notiz): `<origin>\n<size>\n<root base64>\n[…]\n\n— <name> <base64(hint‖sig)>\n`.
pub(crate) fn verify_checkpoint(
    log: &Log,
    envelope: &str,
    size: u64,
    root: &[u8; 32],
) -> Result<(), Error> {
    let bad = |m: &str| Error::Tlog(format!("Checkpoint: {m}"));
    let split = envelope
        .find("\n\n")
        .ok_or_else(|| bad("keine Signaturen"))?;
    let text = &envelope[..=split];
    let mut lines = text.lines();
    let origin = lines.next().unwrap_or_default();
    if origin.is_empty() {
        return Err(bad("Herkunft fehlt"));
    }
    let n: u64 = lines
        .next()
        .and_then(|l| l.parse().ok())
        .ok_or_else(|| bad("Baumgröße fehlt"))?;
    let r = lines
        .next()
        .map(|l| hash32(l, "Checkpoint-Wurzel"))
        .transpose()?
        .ok_or_else(|| bad("Wurzel fehlt"))?;
    if n != size || r != *root {
        return Err(bad("passt nicht zum Inclusion-Proof (Größe/Wurzel)"));
    }
    let hint = log.log_id.get(..4).ok_or_else(|| bad("Log-ID zu kurz"))?;
    let mut seen_hint = false;
    for line in envelope[split + 2..].lines() {
        let Some(rest) = line.strip_prefix("\u{2014} ") else {
            continue;
        };
        let Some((_name, b64)) = rest.rsplit_once(' ') else {
            continue;
        };
        let Ok(raw) = STANDARD.decode(b64.trim()) else {
            continue;
        };
        if raw.len() <= 4 || raw[..4] != *hint {
            continue;
        }
        seen_hint = true;
        if log.key.verify(text.as_bytes(), &raw[4..]) {
            return Ok(());
        }
    }
    Err(bad(if seen_hint {
        "Signatur des Logs ungültig"
    } else {
        "keine Signatur mit dem Schlüssel-Hinweis des Logs"
    }))
}

/// Gleicht den `hashedrekord`-Eintrag (v0.0.1) mit Bundle ab: Digest des Artefakts, Signatur
/// und Zertifikat müssen genau die des Bundles sein.
pub(crate) fn check_body(
    body: &[u8],
    digest: &[u8; 32],
    signature: &[u8],
    leaf_der: &[u8],
) -> Result<(), Error> {
    let bad = |m: &str| Error::Tlog(format!("Rekor-Eintrag: {m}"));
    let v: Value = serde_json::from_slice(body).map_err(|_| bad("canonicalizedBody kein JSON"))?;
    if v["kind"] != "hashedrekord" || v["apiVersion"] != "0.0.1" {
        return Err(Error::Unsupported(
            "Rekor-Eintrag: nur hashedrekord 0.0.1 (Rekor v1)".into(),
        ));
    }
    let spec = &v["spec"];
    if spec["data"]["hash"]["algorithm"] != "sha256"
        || spec["data"]["hash"]["value"].as_str() != Some(hex(digest).as_str())
    {
        return Err(bad("Digest passt nicht zum Artefakt"));
    }
    let sig = spec["signature"]["content"]
        .as_str()
        .and_then(|s| STANDARD.decode(s).ok())
        .ok_or_else(|| bad("Signatur fehlt"))?;
    if sig != signature {
        return Err(bad("Signatur passt nicht zum Bundle"));
    }
    let pem = spec["signature"]["publicKey"]["content"]
        .as_str()
        .and_then(|s| STANDARD.decode(s).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .ok_or_else(|| bad("Zertifikat fehlt"))?;
    let der: String = pem
        .lines()
        .filter(|l| !l.trim_start().starts_with("-----"))
        .collect();
    let der = STANDARD
        .decode(der.trim())
        .map_err(|_| bad("Zertifikat kein PEM"))?;
    if der != leaf_der {
        return Err(bad("Zertifikat passt nicht zum Bundle"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(i: u8) -> [u8; 32] {
        sha256(&[&[0], &[i]])
    }

    fn node(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
        sha256(&[&[1], a, b])
    }

    #[test]
    fn inclusion_small_trees() {
        // Baum mit 5 Blättern: ((0,1),(2,3)),4
        let l: Vec<_> = (0..5).map(leaf).collect();
        let n01 = node(&l[0], &l[1]);
        let n23 = node(&l[2], &l[3]);
        let n0123 = node(&n01, &n23);
        let root = node(&n0123, &l[4]);
        assert!(verify_inclusion(0, 5, l[0], &[l[1], n23, l[4]], &root));
        assert!(verify_inclusion(3, 5, l[3], &[l[2], n01, l[4]], &root));
        assert!(verify_inclusion(4, 5, l[4], &[n0123], &root));
        assert!(!verify_inclusion(4, 5, l[4], &[n01], &root));
        assert!(!verify_inclusion(5, 5, l[4], &[n0123], &root));
        assert!(!verify_inclusion(1, 5, l[0], &[l[1], n23, l[4]], &root));
        // Ein Blatt.
        assert!(verify_inclusion(0, 1, l[0], &[], &l[0]));
        assert!(!verify_inclusion(0, 1, l[0], &[l[1]], &l[0]));
    }
}
