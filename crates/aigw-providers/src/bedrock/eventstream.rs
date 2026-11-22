// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! AWS-Eventstream-Binärformat (`application/vnd.amazon.eventstream`), eigene Implementierung.
//!
//! Aufbau einer Nachricht (alle Zahlen Big Endian):
//!
//! ```text
//! ┌────────────┬──────────────┬─────────────┬─────────┬─────────┬─────────────┐
//! │ total: u32 │ headers: u32 │ prelude_crc │ Header… │ Payload │ message_crc │
//! └────────────┴──────────────┴─────────────┴─────────┴─────────┴─────────────┘
//!   Prelude (12 Bytes, CRC32 über die ersten 8)          CRC32 über alles davor
//! ```
//!
//! Header: `name_len: u8`, Name (UTF-8), `typ: u8`, Wert – Typen 0/1 bool, 2 i8, 3 i16, 4 i32,
//! 5 i64, 6 Bytes (u16 Länge), 7 String (u16 Länge), 8 Zeitstempel (i64 ms), 9 UUID (16 Bytes).
//! CRC32 ist IEEE 802.3 (wie zlib). Quelle: AWS-Doku „Event stream encoding“ (Transcribe/S3
//! Select) und `aws-smithy-eventstream`.
//!
//! Der [`Decoder`] ist inkrementell: Bytes kommen in beliebigen Stücken, vollständige
//! Nachrichten werden ausgegeben, nur der Rest einer unvollständigen Nachricht wird gepuffert.

use bytes::{Buf as _, BytesMut};

/// Größte akzeptierte Nachricht (AWS: 16 MiB).
pub const MAX_MESSAGE_LEN: usize = 16 * 1024 * 1024;
const PRELUDE_LEN: usize = 12;
const MIN_MESSAGE_LEN: usize = PRELUDE_LEN + 4;

/// CRC32-Tabelle (Polynom 0xEDB88320, reflektiert).
const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

/// CRC32 (IEEE) über `data`, fortgesetzt ab `crc` (Startwert 0).
pub fn crc32_update(crc: u32, data: &[u8]) -> u32 {
    let mut c = !crc;
    for &b in data {
        c = CRC_TABLE[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

/// CRC32 (IEEE).
pub fn crc32(data: &[u8]) -> u32 {
    crc32_update(0, data)
}

/// Wert eines Headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderValue {
    /// Wahrheitswert.
    Bool(bool),
    /// 8-Bit-Zahl.
    Byte(i8),
    /// 16-Bit-Zahl.
    Short(i16),
    /// 32-Bit-Zahl.
    Int(i32),
    /// 64-Bit-Zahl.
    Long(i64),
    /// Bytes.
    Bytes(Vec<u8>),
    /// UTF-8-Text.
    String(String),
    /// Zeitstempel (ms seit Epoche).
    Timestamp(i64),
    /// UUID.
    Uuid([u8; 16]),
}

/// Eine dekodierte Nachricht.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Header in Reihenfolge.
    pub headers: Vec<(String, HeaderValue)>,
    /// Nutzdaten.
    pub payload: Vec<u8>,
}

impl Message {
    /// String-Header (z. B. `:event-type`, `:message-type`, `:exception-type`).
    pub fn header_str(&self, name: &str) -> Option<&str> {
        self.headers.iter().find_map(|(n, v)| match v {
            HeaderValue::String(s) if n == name => Some(s.as_str()),
            _ => None,
        })
    }

    /// Kodiert die Nachricht (für Tests und Fake-Server).
    pub fn encode(&self) -> Vec<u8> {
        let mut headers = Vec::new();
        for (name, value) in &self.headers {
            let n = name.as_bytes();
            headers.push(u8::try_from(n.len()).unwrap_or(u8::MAX));
            headers.extend_from_slice(&n[..n.len().min(255)]);
            match value {
                HeaderValue::Bool(true) => headers.push(0),
                HeaderValue::Bool(false) => headers.push(1),
                HeaderValue::Byte(v) => {
                    headers.push(2);
                    headers.extend_from_slice(&v.to_be_bytes());
                }
                HeaderValue::Short(v) => {
                    headers.push(3);
                    headers.extend_from_slice(&v.to_be_bytes());
                }
                HeaderValue::Int(v) => {
                    headers.push(4);
                    headers.extend_from_slice(&v.to_be_bytes());
                }
                HeaderValue::Long(v) => {
                    headers.push(5);
                    headers.extend_from_slice(&v.to_be_bytes());
                }
                HeaderValue::Bytes(b) => {
                    headers.push(6);
                    headers.extend_from_slice(&u16::try_from(b.len()).unwrap_or(0).to_be_bytes());
                    headers.extend_from_slice(b);
                }
                HeaderValue::String(s) => {
                    headers.push(7);
                    headers.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
                    headers.extend_from_slice(s.as_bytes());
                }
                HeaderValue::Timestamp(v) => {
                    headers.push(8);
                    headers.extend_from_slice(&v.to_be_bytes());
                }
                HeaderValue::Uuid(u) => {
                    headers.push(9);
                    headers.extend_from_slice(u);
                }
            }
        }
        let total = PRELUDE_LEN + headers.len() + self.payload.len() + 4;
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&u32::try_from(total).unwrap_or(u32::MAX).to_be_bytes());
        out.extend_from_slice(
            &u32::try_from(headers.len())
                .unwrap_or(u32::MAX)
                .to_be_bytes(),
        );
        let prelude_crc = crc32(&out);
        out.extend_from_slice(&prelude_crc.to_be_bytes());
        out.extend_from_slice(&headers);
        out.extend_from_slice(&self.payload);
        let crc = crc32(&out);
        out.extend_from_slice(&crc.to_be_bytes());
        out
    }
}

/// Fehler beim Dekodieren (danach ist der Strom nicht mehr synchron lesbar).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// Prelude-Prüfsumme falsch.
    #[error("Eventstream: Prelude-CRC stimmt nicht")]
    PreludeCrc,
    /// Nachrichten-Prüfsumme falsch.
    #[error("Eventstream: Nachrichten-CRC stimmt nicht")]
    MessageCrc,
    /// Längenangaben unmöglich oder zu groß.
    #[error("Eventstream: ungültige Länge ({0} Bytes)")]
    Length(usize),
    /// Header nicht lesbar.
    #[error("Eventstream: ungültiger Header ({0})")]
    Header(&'static str),
}

fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.0.len() < n {
            return Err(DecodeError::Header("abgeschnitten"));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }
}

fn parse_headers(mut r: Reader<'_>) -> Result<Vec<(String, HeaderValue)>, DecodeError> {
    let mut out = Vec::with_capacity(3);
    while !r.0.is_empty() {
        let [len] = r.array::<1>()?;
        let name = std::str::from_utf8(r.take(usize::from(len))?)
            .map_err(|_| DecodeError::Header("Name kein UTF-8"))?
            .to_owned();
        let [ty] = r.array::<1>()?;
        let value = match ty {
            0 => HeaderValue::Bool(true),
            1 => HeaderValue::Bool(false),
            2 => HeaderValue::Byte(i8::from_be_bytes(r.array()?)),
            3 => HeaderValue::Short(i16::from_be_bytes(r.array()?)),
            4 => HeaderValue::Int(i32::from_be_bytes(r.array()?)),
            5 => HeaderValue::Long(i64::from_be_bytes(r.array()?)),
            6 | 7 => {
                let len = u16::from_be_bytes(r.array()?);
                let data = r.take(usize::from(len))?;
                if ty == 6 {
                    HeaderValue::Bytes(data.to_vec())
                } else {
                    HeaderValue::String(
                        std::str::from_utf8(data)
                            .map_err(|_| DecodeError::Header("Wert kein UTF-8"))?
                            .to_owned(),
                    )
                }
            }
            8 => HeaderValue::Timestamp(i64::from_be_bytes(r.array()?)),
            9 => HeaderValue::Uuid(r.array()?),
            _ => return Err(DecodeError::Header("unbekannter Typ")),
        };
        out.push((name, value));
    }
    Ok(out)
}

/// Inkrementeller Decoder.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: BytesMut,
}

impl Decoder {
    /// Neuer Decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Hängt Bytes an und liefert alle jetzt vollständigen Nachrichten.
    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<Message>) -> Result<(), DecodeError> {
        self.buf.extend_from_slice(chunk);
        while let Some(msg) = self.next_message()? {
            out.push(msg);
        }
        Ok(())
    }

    /// Ungelesene Bytes (Rest einer unvollständigen Nachricht).
    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    fn next_message(&mut self) -> Result<Option<Message>, DecodeError> {
        if self.buf.len() < PRELUDE_LEN {
            return Ok(None);
        }
        let total = be_u32(&self.buf[0..4]) as usize;
        let headers_len = be_u32(&self.buf[4..8]) as usize;
        if crc32(&self.buf[0..8]) != be_u32(&self.buf[8..12]) {
            return Err(DecodeError::PreludeCrc);
        }
        if !(MIN_MESSAGE_LEN..=MAX_MESSAGE_LEN).contains(&total)
            || headers_len > total - MIN_MESSAGE_LEN
        {
            return Err(DecodeError::Length(total));
        }
        if self.buf.len() < total {
            return Ok(None);
        }
        let frame = &self.buf[..total];
        let (body, crc) = frame.split_at(total - 4);
        if crc32(body) != be_u32(crc) {
            return Err(DecodeError::MessageCrc);
        }
        let headers = parse_headers(Reader(&body[PRELUDE_LEN..PRELUDE_LEN + headers_len]))?;
        let payload = body[PRELUDE_LEN + headers_len..].to_vec();
        self.buf.advance(total);
        Ok(Some(Message { headers, payload }))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn crc32_check_value() {
        // Prüfwert CRC-32/ISO-HDLC.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32_update(crc32(b"1234"), b"56789"), 0xCBF4_3926);
    }

    #[test]
    fn decodes_reference_message() {
        // Testvektor „all_headers“ aus aws-smithy-eventstream / der Eventstream-Spezifikation
        // (alle Header-Typen, Payload `{'foo':'bar'}`) – hier nachgebaut und gegen die
        // dokumentierte Prelude geprüft.
        let msg = Message {
            headers: vec![
                ("event-type".into(), HeaderValue::Int(0x0000_A00C)),
                (
                    "content-type".into(),
                    HeaderValue::String("application/json".into()),
                ),
                ("bool false".into(), HeaderValue::Bool(false)),
                ("bool true".into(), HeaderValue::Bool(true)),
                ("byte".into(), HeaderValue::Byte(-49)),
                (
                    "byte buf".into(),
                    HeaderValue::Bytes(b"I'm a little teapot!".to_vec()),
                ),
                ("timestamp".into(), HeaderValue::Timestamp(8_675_309)),
                ("int16".into(), HeaderValue::Short(42)),
                ("int64".into(), HeaderValue::Long(42_424_242)),
                (
                    "uuid".into(),
                    HeaderValue::Uuid([
                        0xb7, 0x9b, 0xc9, 0x14, 0xde, 0x21, 0x4e, 0x13, 0xb8, 0xb2, 0xbc, 0x47,
                        0xe8, 0x5b, 0x7f, 0x0b,
                    ]),
                ),
            ],
            payload: b"{'foo':'bar'}".to_vec(),
        };
        let bytes = msg.encode();
        // Prelude: Gesamtlänge 204, Header 175 (Werte aus dem Referenzvektor).
        assert_eq!(&bytes[..8], &[0, 0, 0, 204, 0, 0, 0, 175]);
        let mut d = Decoder::new();
        let mut out = Vec::new();
        d.push(&bytes, &mut out).unwrap();
        assert_eq!(out, vec![msg]);
    }

    #[test]
    fn decodes_across_arbitrary_splits() {
        let mk = |t: &str, p: &str| Message {
            headers: vec![
                (":event-type".into(), HeaderValue::String(t.into())),
                (":message-type".into(), HeaderValue::String("event".into())),
            ],
            payload: p.as_bytes().to_vec(),
        };
        let msgs = [mk("a", "{\"x\":1}"), mk("bb", ""), mk("ccc", "{}")];
        let mut all = Vec::new();
        for m in &msgs {
            all.extend(m.encode());
        }
        for step in [1, 2, 3, 7, 13, all.len()] {
            let mut d = Decoder::new();
            let mut out = Vec::new();
            for c in all.chunks(step) {
                d.push(c, &mut out).unwrap();
            }
            assert_eq!(out.len(), 3, "Schrittweite {step}");
            assert_eq!(out[1].header_str(":event-type"), Some("bb"));
            assert_eq!(d.pending(), 0);
        }
    }

    #[test]
    fn rejects_corruption() {
        let m = Message {
            headers: vec![],
            payload: b"hello".to_vec(),
        };
        let good = m.encode();

        let mut bad = good.clone();
        bad[3] ^= 1; // Länge → Prelude-CRC
        assert_eq!(
            Decoder::new().push(&bad, &mut Vec::new()),
            Err(DecodeError::PreludeCrc)
        );

        let mut bad = good.clone();
        bad[13] ^= 1; // Payload → Nachrichten-CRC
        assert_eq!(
            Decoder::new().push(&bad, &mut Vec::new()),
            Err(DecodeError::MessageCrc)
        );

        // Riesige Längenangabe mit gültiger Prelude-CRC.
        let mut huge = Vec::new();
        huge.extend_from_slice(&u32::MAX.to_be_bytes());
        huge.extend_from_slice(&0u32.to_be_bytes());
        let crc = crc32(&huge);
        huge.extend_from_slice(&crc.to_be_bytes());
        assert!(matches!(
            Decoder::new().push(&huge, &mut Vec::new()),
            Err(DecodeError::Length(_))
        ));
    }
}
