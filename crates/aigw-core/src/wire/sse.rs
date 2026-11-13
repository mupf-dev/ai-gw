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
        let mut events = Vec::new();
        self.push_into(chunk, &mut events);
        events
    }

    /// Wie [`SseParser::push`], hängt die Ereignisse aber an `events` an (ohne eigene
    /// Allokation). Vollständige Zeilen werden direkt aus `chunk` gelesen; nur ein
    /// unvollständiger Rest wird zwischengespeichert.
    pub fn push_into(&mut self, chunk: &[u8], events: &mut Vec<SseEvent>) {
        if self.buf.is_empty() {
            let used = self.lines(chunk, events, 0);
            self.buf.extend_from_slice(&chunk[used..]);
        } else {
            let mut buf = std::mem::take(&mut self.buf);
            // Der alte Rest enthält keinen Zeilenumbruch: erst ab den neuen Bytes suchen
            // (AIGW-58: sonst quadratischer Aufwand bei langen Zeilen in kleinen Stücken).
            let scanned = buf.len();
            buf.extend_from_slice(chunk);
            let used = self.lines(&buf, events, scanned);
            buf.drain(..used);
            self.buf = buf;
        }
    }

    /// Anzahl der zwischengespeicherten Bytes (unvollständige Zeile).
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Zwischengespeicherte Bytes insgesamt: unvollständige Zeile plus `data` eines noch
    /// nicht abgeschlossenen Ereignisses (AIGW-58: Grenze gegen Speicher-DoS durch einen
    /// Upstream, der nie eine Leerzeile bzw. einen Zeilenumbruch schickt).
    pub fn pending_bytes(&self) -> usize {
        self.buf.len() + self.data.as_ref().map_or(0, String::len)
    }

    /// Verarbeitet alle vollständigen Zeilen in `data`; liefert die Anzahl verbrauchter Bytes.
    /// `from`: Ab hier nach dem ersten Zeilenumbruch suchen (davor gibt es keinen).
    fn lines(&mut self, data: &[u8], events: &mut Vec<SseEvent>, from: usize) -> usize {
        let mut start = 0;
        let mut search = from;
        while let Some(rel) = data[search..].iter().position(|&b| b == b'\n') {
            let end = search + rel;
            let mut line = &data[start..end];
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            start = end + 1;
            search = start;
            match std::str::from_utf8(line) {
                Ok(s) => self.line(s, events),
                Err(_) => self.line(&String::from_utf8_lossy(line), events),
            }
        }
        start
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
    fn pending_bytes_counts_line_and_open_event() {
        let mut p = SseParser::new();
        assert!(p.push(b"data: abc\ndata: de").is_empty());
        assert_eq!(p.pending_bytes(), 3 + 8);
        assert_eq!(p.push(b"f\n\n").len(), 1);
        assert_eq!(p.pending_bytes(), 0);
    }

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
