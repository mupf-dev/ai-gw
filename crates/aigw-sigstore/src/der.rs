// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Minimaler DER-Leser/-Schreiber (nur, was X.509-Auswertung und SCT-Prüfung brauchen).
//!
//! Unterstützt Ein-Byte-Tags (alle in X.509 vorkommenden) und definite Längen bis 2³²−1.
//! Indefinite Längen (BER) und nicht minimale Längenkodierungen werden abgelehnt – die
//! Zertifikatskette prüft ohnehin rustls-webpki; hier werden nur Felder herausgelesen.

use crate::Error;

/// SEQUENCE.
pub(crate) const SEQUENCE: u8 = 0x30;
/// OBJECT IDENTIFIER.
pub(crate) const OID: u8 = 0x06;
/// OCTET STRING.
pub(crate) const OCTET_STRING: u8 = 0x04;
/// BIT STRING.
pub(crate) const BIT_STRING: u8 = 0x03;
/// BOOLEAN.
pub(crate) const BOOLEAN: u8 = 0x01;
/// UTF8String.
pub(crate) const UTF8_STRING: u8 = 0x0c;

/// Ein Element (Tag, Inhalt, vollständige Kodierung).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Tlv<'a> {
    pub(crate) tag: u8,
    pub(crate) value: &'a [u8],
    pub(crate) raw: &'a [u8],
}

/// Liest Elemente nacheinander aus einem Puffer.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
}

fn bad(what: &str) -> Error {
    Error::Malformed(format!("DER: {what}"))
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub(crate) fn peek_tag(&self) -> Option<u8> {
        self.buf.first().copied()
    }

    /// Nächstes Element.
    pub(crate) fn read(&mut self) -> Result<Tlv<'a>, Error> {
        let buf = self.buf;
        let (&tag, rest) = buf.split_first().ok_or_else(|| bad("Ende erreicht"))?;
        if tag & 0x1f == 0x1f {
            return Err(bad("mehrbytige Tags nicht unterstützt"));
        }
        let (&first, rest) = rest.split_first().ok_or_else(|| bad("Länge fehlt"))?;
        let (len, rest) = if first < 0x80 {
            (usize::from(first), rest)
        } else {
            let n = usize::from(first & 0x7f);
            if n == 0 || n > 4 || rest.len() < n {
                return Err(bad("ungültige Länge"));
            }
            let mut len = 0usize;
            for &b in &rest[..n] {
                len = (len << 8) | usize::from(b);
            }
            if len < 0x80 || (n > 1 && rest[0] == 0) {
                return Err(bad("nicht minimale Länge"));
            }
            (len, &rest[n..])
        };
        if rest.len() < len {
            return Err(bad("Inhalt abgeschnitten"));
        }
        let header = buf.len() - rest.len();
        let raw = &buf[..header + len];
        self.buf = &rest[len..];
        Ok(Tlv {
            tag,
            value: &rest[..len],
            raw,
        })
    }

    /// Nächstes Element mit erwartetem Tag.
    pub(crate) fn expect(&mut self, tag: u8) -> Result<Tlv<'a>, Error> {
        let t = self.read()?;
        if t.tag != tag {
            return Err(bad(&format!(
                "Tag {:#04x} erwartet, {:#04x} gefunden",
                tag, t.tag
            )));
        }
        Ok(t)
    }

    /// Liest ein Element nur, wenn das nächste Tag passt.
    pub(crate) fn optional(&mut self, tag: u8) -> Result<Option<Tlv<'a>>, Error> {
        if self.peek_tag() == Some(tag) {
            self.read().map(Some)
        } else {
            Ok(None)
        }
    }
}

/// Ein einzelnes Element, das den ganzen Puffer ausfüllen muss.
pub(crate) fn single(buf: &[u8], tag: u8) -> Result<Tlv<'_>, Error> {
    let mut r = Reader::new(buf);
    let t = r.expect(tag)?;
    if !r.is_empty() {
        return Err(bad("Daten nach dem Element"));
    }
    Ok(t)
}

/// Kodiert ein Element (Tag + definite Länge + Inhalt).
pub(crate) fn encode(tag: u8, content: &[u8]) -> Vec<u8> {
    let len = content.len();
    let mut out = Vec::with_capacity(len + 6);
    out.push(tag);
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes = (len as u32).to_be_bytes();
        let skip = bytes.iter().take_while(|b| **b == 0).count();
        out.push(0x80 | (4 - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
    out.extend_from_slice(content);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_lengths() {
        for n in [0usize, 1, 127, 128, 255, 256, 70_000] {
            let content = vec![7u8; n];
            let enc = encode(SEQUENCE, &content);
            let t = single(&enc, SEQUENCE).unwrap();
            assert_eq!(t.value.len(), n);
            assert_eq!(t.raw, enc.as_slice());
        }
    }

    #[test]
    fn rejects_garbage() {
        assert!(single(&[0x30], SEQUENCE).is_err());
        assert!(single(&[0x30, 0x80], SEQUENCE).is_err(), "indefinite Länge");
        assert!(
            single(&[0x30, 0x81, 0x05, 0, 0, 0, 0, 0], SEQUENCE).is_err(),
            "nicht minimal"
        );
        assert!(
            single(&[0x30, 0x02, 0x01], SEQUENCE).is_err(),
            "abgeschnitten"
        );
        assert!(single(&[0x30, 0x00, 0x00], SEQUENCE).is_err(), "Rest");
        assert!(single(&[0x1f, 0x00], 0x1f).is_err(), "Mehrbyte-Tag");
    }
}
