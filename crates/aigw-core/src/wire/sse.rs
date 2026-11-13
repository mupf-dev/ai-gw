// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Server-Sent Events: Kodieren von Frames und inkrementelles Parsen.

/// Ein SSE-Ereignis.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseEvent {
    /// Feld `event` (fehlt bei OpenAI).
    pub event: Option<String>,
    /// Feld `data` (mehrere `data:`-Zeilen mit `\n` verbunden).
    pub data: String,
}

/// Hängt einen Frame `data: …\n\n` an.
pub fn write_data(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(b"data: ");
    out.extend_from_slice(data);
    out.extend_from_slice(b"\n\n");
}

/// Hängt einen benannten Frame `event: …\ndata: …\n\n` an.
pub fn write_event(out: &mut Vec<u8>, event: &str, data: &[u8]) {
    out.extend_from_slice(b"event: ");
    out.extend_from_slice(event.as_bytes());
    out.push(b'\n');
    write_data(out, data);
}

/// Inkrementeller SSE-Parser (WHATWG-Regeln, ohne `id`/`retry`-Auswertung).
///
/// Bytes können in beliebig großen Stücken kommen; [`SseParser::push`] liefert alle bis dahin
/// vollständigen Ereignisse. Kommentarzeilen (`:`) werden ignoriert.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    event: Option<String>,
    data: Option<String>,
}

impl SseParser {
    /// Neuer Parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Verarbeitet weitere Bytes.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut events = Vec::new();
        let mut start = 0;
        while let Some(rel) = self.buf[start..].iter().position(|&b| b == b'\n') {
            let end = start + rel;
            let mut line = &self.buf[start..end];
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            let line = String::from_utf8_lossy(line).into_owned();
            start = end + 1;
            self.line(&line, &mut events);
        }
        self.buf.drain(..start);
        events
    }

    fn line(&mut self, line: &str, events: &mut Vec<SseEvent>) {
        if line.is_empty() {
            if let Some(data) = self.data.take() {
                events.push(SseEvent {
                    event: self.event.take(),
                    data,
                });
            }
            self.event = None;
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => match &mut self.data {
                Some(d) => {
                    d.push('\n');
                    d.push_str(value);
                }
                None => self.data = Some(value.to_owned()),
            },
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_split_frames() {
        let mut p = SseParser::new();
        let mut out = Vec::new();
        write_event(&mut out, "ping", b"{\"type\":\"ping\"}");
        write_data(&mut out, b"[DONE]");
        let mut events = Vec::new();
        for chunk in out.chunks(3) {
            events.extend(p.push(chunk));
        }
        assert_eq!(
            events,
            vec![
                SseEvent {
                    event: Some("ping".into()),
                    data: "{\"type\":\"ping\"}".into()
                },
                SseEvent {
                    event: None,
                    data: "[DONE]".into()
                }
            ]
        );
    }

    #[test]
    fn handles_crlf_comments_and_multiline() {
        let mut p = SseParser::new();
        let ev = p.push(b": keep-alive\r\ndata: a\r\ndata: b\r\n\r\n");
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].data, "a\nb");
    }
}
