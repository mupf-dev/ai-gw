// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! `multipart/form-data` (RFC 7578) lesen und schreiben – für Audio-Uploads
//! (`/v1/audio/transcriptions`, `/v1/audio/translations`, AIGW-21).
//!
//! Der Parser arbeitet auf dem bereits vollständig gelesenen (und größenbegrenzten) Body und
//! gibt die Teile als `Bytes`-Ausschnitte zurück – die Audiodaten werden nicht kopiert.

use bytes::{BufMut as _, Bytes, BytesMut};

/// Höchstens so viele Teile werden gelesen (Schutz vor Missbrauch).
pub const MAX_PARTS: usize = 64;
/// Größter Header-Block eines Teils.
const MAX_HEADER_BYTES: usize = 8 * 1024;

/// Ein Teil eines Formulars.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormPart {
    /// Feldname (`name`).
    pub name: String,
    /// Dateiname (`filename`), nur bei Datei-Feldern.
    pub filename: Option<String>,
    /// `Content-Type` des Teils.
    pub content_type: Option<String>,
    /// Inhalt.
    pub data: Bytes,
}

impl FormPart {
    /// Inhalt als UTF-8-Text (`None`, wenn ungültig).
    pub fn text(&self) -> Option<&str> {
        std::str::from_utf8(&self.data).ok()
    }
}

/// Fehler beim Lesen eines Formulars.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("multipart/form-data ungültig: {0}")]
pub struct MultipartError(pub String);

fn err(m: impl Into<String>) -> MultipartError {
    MultipartError(m.into())
}

/// Liest die `boundary` aus einem `Content-Type`-Header (`multipart/form-data; boundary=…`).
pub fn boundary(content_type: &str) -> Option<&str> {
    let mut parts = content_type.split(';');
    let mime = parts.next()?.trim();
    if !mime.eq_ignore_ascii_case("multipart/form-data") {
        return None;
    }
    parts.find_map(|p| {
        let (k, v) = p.split_once('=')?;
        if !k.trim().eq_ignore_ascii_case("boundary") {
            return None;
        }
        let v = v.trim();
        let v = v
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(v);
        (!v.is_empty() && v.len() <= 70).then_some(v)
    })
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    let first = *needle.first()?;
    let mut i = from;
    while i + needle.len() <= hay.len() {
        let rel = hay.get(i..)?.iter().position(|b| *b == first)?;
        i += rel;
        if hay.get(i..i + needle.len()) == Some(needle) {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Parameter eines Header-Werts (`form-data; name="a"; filename="b.mp3"`).
fn param(value: &str, key: &str) -> Option<String> {
    let mut rest = value;
    while let Some(pos) = rest.find(';') {
        rest = rest.get(pos + 1..)?.trim_start();
        let (k, after) = rest.split_once('=')?;
        let k = k.trim();
        let (v, tail) = if let Some(q) = after.strip_prefix('"') {
            // Wert in Anführungszeichen; `\"` und `\\` sind maskiert.
            let mut out = String::new();
            let mut chars = q.char_indices();
            let mut end = q.len();
            while let Some((i, c)) = chars.next() {
                match c {
                    '\\' => {
                        if let Some((_, n)) = chars.next() {
                            out.push(n);
                        }
                    }
                    '"' => {
                        end = i + 1;
                        break;
                    }
                    c => out.push(c),
                }
            }
            (out, q.get(end..).unwrap_or_default())
        } else {
            let end = after.find(';').unwrap_or(after.len());
            (
                after.get(..end).unwrap_or_default().trim().to_owned(),
                after.get(end..).unwrap_or_default(),
            )
        };
        if k.eq_ignore_ascii_case(key) {
            return Some(v);
        }
        rest = tail;
    }
    None
}

/// Zerlegt einen `multipart/form-data`-Body.
pub fn parse(body: &Bytes, boundary: &str) -> Result<Vec<FormPart>, MultipartError> {
    let delim = format!("--{boundary}");
    let delim = delim.as_bytes();
    // Erster Trenner (davor ggf. Präambel).
    let mut pos = find(body, delim, 0).ok_or_else(|| err("Trenner nicht gefunden"))?;
    let mut parts = Vec::new();
    loop {
        pos += delim.len();
        let rest = body.get(pos..).unwrap_or_default();
        if rest.starts_with(b"--") {
            return Ok(parts);
        }
        // Rest der Trennerzeile (Leerraum erlaubt) bis CRLF.
        let line_end = find(body, b"\r\n", pos).ok_or_else(|| err("Trennerzeile ohne CRLF"))?;
        if body
            .get(pos..line_end)
            .is_some_and(|l| !l.iter().all(|b| *b == b' ' || *b == b'\t'))
        {
            return Err(err("Zeichen nach dem Trenner"));
        }
        let headers_start = line_end + 2;
        let headers_end = find(body, b"\r\n\r\n", headers_start)
            .filter(|e| e - headers_start <= MAX_HEADER_BYTES)
            .ok_or_else(|| err("Kopfzeilen eines Teils fehlen oder sind zu lang"))?;
        let headers = std::str::from_utf8(body.get(headers_start..headers_end).unwrap_or_default())
            .map_err(|_| err("Kopfzeilen sind kein UTF-8"))?;
        let data_start = headers_end + 4;
        let mut close = Vec::with_capacity(delim.len() + 2);
        close.extend_from_slice(b"\r\n");
        close.extend_from_slice(delim);
        let data_end = find(body, &close, data_start).ok_or_else(|| err("Teil ohne Ende"))?;

        let mut name = None;
        let mut filename = None;
        let mut content_type = None;
        for line in headers.split("\r\n") {
            let Some((k, v)) = line.split_once(':') else {
                continue;
            };
            let k = k.trim();
            if k.eq_ignore_ascii_case("content-disposition") {
                name = param(v, "name");
                filename = param(v, "filename");
            } else if k.eq_ignore_ascii_case("content-type") {
                content_type = Some(v.trim().to_owned());
            }
        }
        let name = name.ok_or_else(|| err("Teil ohne Feldnamen (Content-Disposition name)"))?;
        if parts.len() >= MAX_PARTS {
            return Err(err(format!("mehr als {MAX_PARTS} Teile")));
        }
        parts.push(FormPart {
            name,
            filename,
            content_type,
            data: body.slice(data_start..data_end),
        });
        pos = data_end + 2;
    }
}

/// Baut einen `multipart/form-data`-Body (für Upstream-Anfragen).
#[derive(Debug)]
pub struct FormBuilder {
    boundary: String,
    buf: BytesMut,
}

impl Default for FormBuilder {
    fn default() -> Self {
        Self::new()
    }
}

fn quote(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '"' | '\\' | '\r' | '\n' => '_',
            c => c,
        })
        .collect()
}

impl FormBuilder {
    /// Neues Formular mit zufälliger Trennzeichenfolge.
    pub fn new() -> Self {
        Self {
            boundary: format!("aigw-{}", uuid::Uuid::now_v7().simple()),
            buf: BytesMut::new(),
        }
    }

    /// Wert für den `Content-Type`-Header.
    pub fn content_type(&self) -> String {
        format!("multipart/form-data; boundary={}", self.boundary)
    }

    fn head(&mut self, name: &str) {
        self.buf.put_slice(b"--");
        self.buf.put_slice(self.boundary.as_bytes());
        self.buf
            .put_slice(b"\r\nContent-Disposition: form-data; name=\"");
        self.buf.put_slice(quote(name).as_bytes());
        self.buf.put_slice(b"\"");
    }

    /// Textfeld.
    pub fn text(&mut self, name: &str, value: &str) -> &mut Self {
        self.head(name);
        self.buf.put_slice(b"\r\n\r\n");
        self.buf.put_slice(value.as_bytes());
        self.buf.put_slice(b"\r\n");
        self
    }

    /// Datei-Feld.
    pub fn file(
        &mut self,
        name: &str,
        filename: &str,
        content_type: &str,
        data: &[u8],
    ) -> &mut Self {
        self.buf.reserve(data.len() + 256);
        self.head(name);
        self.buf.put_slice(b"; filename=\"");
        self.buf.put_slice(quote(filename).as_bytes());
        self.buf.put_slice(b"\"\r\nContent-Type: ");
        self.buf.put_slice(quote(content_type).as_bytes());
        self.buf.put_slice(b"\r\n\r\n");
        self.buf.put_slice(data);
        self.buf.put_slice(b"\r\n");
        self
    }

    /// Schließt das Formular ab.
    pub fn finish(mut self) -> (String, Bytes) {
        self.buf.put_slice(b"--");
        self.buf.put_slice(self.boundary.as_bytes());
        self.buf.put_slice(b"--\r\n");
        (self.content_type(), self.buf.freeze())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundary_from_header() {
        assert_eq!(boundary("multipart/form-data; boundary=abc"), Some("abc"));
        assert_eq!(
            boundary("Multipart/Form-Data; charset=utf-8; boundary=\"a b\""),
            Some("a b")
        );
        assert_eq!(boundary("application/json"), None);
        assert_eq!(boundary("multipart/form-data"), None);
    }

    #[test]
    fn roundtrip_and_curl_style() {
        let mut b = FormBuilder::new();
        b.text("model", "whisper-1")
            .file("file", "a\"b.mp3", "audio/mpeg", b"\x00\x01\r\n--x\xff");
        let (ct, body) = b.finish();
        let parts = parse(&body, boundary(&ct).unwrap()).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].name, "model");
        assert_eq!(parts[0].text(), Some("whisper-1"));
        assert_eq!(parts[1].filename.as_deref(), Some("a_b.mp3"));
        assert_eq!(parts[1].content_type.as_deref(), Some("audio/mpeg"));
        assert_eq!(&parts[1].data[..], b"\x00\x01\r\n--x\xff");

        // Wie curl -F: Präambel, Parameter ohne Anführungszeichen, maskierte Anführungszeichen.
        let body = Bytes::from_static(
            b"ignored\r\n--XY\r\nContent-Disposition: form-data; name=language\r\n\r\nde\r\n\
--XY\r\ncontent-disposition: form-data; name=\"file\"; filename=\"x\\\"y.wav\"\r\n\r\nRIFF\r\n--XY--\r\n",
        );
        let parts = parse(&body, "XY").unwrap();
        assert_eq!(parts[0].name, "language");
        assert_eq!(parts[0].text(), Some("de"));
        assert_eq!(parts[1].filename.as_deref(), Some("x\"y.wav"));
        assert_eq!(&parts[1].data[..], b"RIFF");
    }

    #[test]
    fn rejects_broken_bodies() {
        for body in [
            &b"no boundary here"[..],
            b"--XY\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nunterminated",
            b"--XY\r\nContent-Type: text/plain\r\n\r\nx\r\n--XY--",
            b"--XYjunk\r\n",
        ] {
            assert!(parse(&Bytes::copy_from_slice(body), "XY").is_err());
        }
    }
}
