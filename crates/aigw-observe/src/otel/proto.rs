// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Minimaler Protobuf-Encoder für die OTLP-Nachrichten, die ai-gw sendet
//! (`ExportTraceServiceRequest`, `ExportLogsServiceRequest`).
//!
//! Feldnummern nach `opentelemetry-proto` v1 (`common`, `resource`, `trace`, `logs`). Es wird
//! nur geschrieben, nie gelesen; leere/Standardwerte werden weggelassen (proto3).

/// Attributwert (`AnyValue`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AnyValue {
    /// `string_value` (1).
    Str(String),
    /// `bool_value` (2).
    Bool(bool),
    /// `int_value` (3).
    Int(i64),
    /// `double_value` (4).
    Double(f64),
}

/// Attribut (`KeyValue`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct KeyValue {
    pub(crate) key: String,
    pub(crate) value: AnyValue,
}

impl KeyValue {
    pub(crate) fn new(key: impl Into<String>, value: AnyValue) -> Self {
        Self {
            key: key.into(),
            value,
        }
    }

    pub(crate) fn str(key: impl Into<String>, v: impl Into<String>) -> Self {
        Self::new(key, AnyValue::Str(v.into()))
    }

    pub(crate) fn int(key: impl Into<String>, v: i64) -> Self {
        Self::new(key, AnyValue::Int(v))
    }

    pub(crate) fn bool(key: impl Into<String>, v: bool) -> Self {
        Self::new(key, AnyValue::Bool(v))
    }
}

/// `Span.SpanKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SpanKind {
    #[default]
    Internal = 1,
    Server = 2,
    Client = 3,
}

impl SpanKind {
    pub(crate) fn parse(s: &str) -> Self {
        if s.eq_ignore_ascii_case("server") {
            Self::Server
        } else if s.eq_ignore_ascii_case("client") {
            Self::Client
        } else {
            Self::Internal
        }
    }
}

/// `Status.StatusCode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum StatusCode {
    #[default]
    Unset = 0,
    Ok = 1,
    Error = 2,
}

/// Ereignis eines Spans (`Span.Event`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EventData {
    pub(crate) time_unix_nano: u64,
    pub(crate) name: String,
    pub(crate) attributes: Vec<KeyValue>,
}

/// Abgeschlossener Span.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SpanData {
    pub(crate) trace_id: [u8; 16],
    pub(crate) span_id: [u8; 8],
    pub(crate) parent_span_id: Option<[u8; 8]>,
    pub(crate) name: String,
    pub(crate) kind: SpanKind,
    pub(crate) start_unix_nano: u64,
    pub(crate) end_unix_nano: u64,
    pub(crate) attributes: Vec<KeyValue>,
    pub(crate) events: Vec<EventData>,
    pub(crate) status: StatusCode,
    pub(crate) status_message: String,
}

/// Log-Eintrag (`LogRecord`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LogData {
    pub(crate) time_unix_nano: u64,
    pub(crate) observed_unix_nano: u64,
    pub(crate) severity_number: i32,
    pub(crate) severity_text: &'static str,
    pub(crate) body: Option<AnyValue>,
    pub(crate) attributes: Vec<KeyValue>,
    pub(crate) event_name: &'static str,
    /// Trace-ID der Anfrage (F100, Feld 9 `trace_id`).
    pub(crate) trace_id: Option<[u8; 16]>,
}

/// Instrumentierungs-Bereich und Ressource (je Export gleich).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Origin {
    pub(crate) resource: Vec<KeyValue>,
    pub(crate) scope_name: String,
    pub(crate) scope_version: String,
}

// Wire-Typen.
const VARINT: u32 = 0;
const I64: u32 = 1;
const LEN: u32 = 2;

fn varint(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

fn tag(buf: &mut Vec<u8>, field: u32, wire: u32) {
    varint(buf, u64::from((field << 3) | wire));
}

fn bytes_field(buf: &mut Vec<u8>, field: u32, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    tag(buf, field, LEN);
    varint(buf, data.len() as u64);
    buf.extend_from_slice(data);
}

fn uint_field(buf: &mut Vec<u8>, field: u32, v: u64) {
    if v != 0 {
        tag(buf, field, VARINT);
        varint(buf, v);
    }
}

fn fixed64_field(buf: &mut Vec<u8>, field: u32, v: u64) {
    if v != 0 {
        tag(buf, field, I64);
        buf.extend_from_slice(&v.to_le_bytes());
    }
}

/// Eingebettete Nachricht (auch leer, damit z. B. ein leerer `AnyValue` erhalten bleibt).
fn message(buf: &mut Vec<u8>, field: u32, body: impl FnOnce(&mut Vec<u8>)) {
    let mut inner = Vec::new();
    body(&mut inner);
    tag(buf, field, LEN);
    varint(buf, inner.len() as u64);
    buf.extend_from_slice(&inner);
}

fn any_value(buf: &mut Vec<u8>, v: &AnyValue) {
    match v {
        AnyValue::Str(s) => {
            // Leere Zeichenkette ausdrücklich schreiben (sonst wäre der Wert „leer“).
            tag(buf, 1, LEN);
            varint(buf, s.len() as u64);
            buf.extend_from_slice(s.as_bytes());
        }
        AnyValue::Bool(b) => {
            tag(buf, 2, VARINT);
            varint(buf, u64::from(*b));
        }
        AnyValue::Int(i) => {
            tag(buf, 3, VARINT);
            varint(buf, *i as u64);
        }
        AnyValue::Double(d) => {
            tag(buf, 4, I64);
            buf.extend_from_slice(&d.to_bits().to_le_bytes());
        }
    }
}

fn key_values(buf: &mut Vec<u8>, field: u32, attrs: &[KeyValue]) {
    for kv in attrs {
        message(buf, field, |m| {
            bytes_field(m, 1, kv.key.as_bytes());
            message(m, 2, |v| any_value(v, &kv.value));
        });
    }
}

fn resource_and_scope(origin: &Origin) -> (Vec<u8>, Vec<u8>) {
    let mut resource = Vec::new();
    key_values(&mut resource, 1, &origin.resource);
    let mut scope = Vec::new();
    bytes_field(&mut scope, 1, origin.scope_name.as_bytes());
    bytes_field(&mut scope, 2, origin.scope_version.as_bytes());
    (resource, scope)
}

/// `ExportTraceServiceRequest` mit einer `ResourceSpans` und einer `ScopeSpans`.
pub(crate) fn encode_traces(origin: &Origin, spans: &[SpanData]) -> Vec<u8> {
    let (resource, scope) = resource_and_scope(origin);
    let mut out = Vec::with_capacity(spans.len() * 512 + 128);
    message(&mut out, 1, |rs| {
        message(rs, 1, |r| r.extend_from_slice(&resource));
        message(rs, 2, |ss| {
            message(ss, 1, |s| s.extend_from_slice(&scope));
            for span in spans {
                message(ss, 2, |s| encode_span(s, span));
            }
        });
    });
    out
}

fn encode_span(s: &mut Vec<u8>, span: &SpanData) {
    bytes_field(s, 1, &span.trace_id);
    bytes_field(s, 2, &span.span_id);
    if let Some(p) = &span.parent_span_id {
        bytes_field(s, 4, p);
    }
    bytes_field(s, 5, span.name.as_bytes());
    uint_field(s, 6, span.kind as u64);
    fixed64_field(s, 7, span.start_unix_nano);
    fixed64_field(s, 8, span.end_unix_nano);
    key_values(s, 9, &span.attributes);
    for ev in &span.events {
        message(s, 11, |e| {
            fixed64_field(e, 1, ev.time_unix_nano);
            bytes_field(e, 2, ev.name.as_bytes());
            key_values(e, 3, &ev.attributes);
        });
    }
    if span.status != StatusCode::Unset || !span.status_message.is_empty() {
        message(s, 15, |st| {
            bytes_field(st, 2, span.status_message.as_bytes());
            uint_field(st, 3, span.status as u64);
        });
    }
}

/// `ExportLogsServiceRequest` mit einer `ResourceLogs` und einer `ScopeLogs`.
pub(crate) fn encode_logs(origin: &Origin, logs: &[LogData]) -> Vec<u8> {
    let (resource, scope) = resource_and_scope(origin);
    let mut out = Vec::with_capacity(logs.len() * 512 + 128);
    message(&mut out, 1, |rl| {
        message(rl, 1, |r| r.extend_from_slice(&resource));
        message(rl, 2, |sl| {
            message(sl, 1, |s| s.extend_from_slice(&scope));
            for log in logs {
                message(sl, 2, |l| {
                    fixed64_field(l, 1, log.time_unix_nano);
                    uint_field(l, 2, u64::try_from(log.severity_number).unwrap_or(0));
                    bytes_field(l, 3, log.severity_text.as_bytes());
                    if let Some(body) = &log.body {
                        message(l, 5, |b| any_value(b, body));
                    }
                    key_values(l, 6, &log.attributes);
                    if let Some(t) = &log.trace_id {
                        bytes_field(l, 9, t);
                    }
                    fixed64_field(l, 11, log.observed_unix_nano);
                    bytes_field(l, 12, log.event_name.as_bytes());
                });
            }
        });
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_encoding() {
        let mut b = Vec::new();
        varint(&mut b, 1);
        varint(&mut b, 300);
        varint(&mut b, u64::MAX);
        assert_eq!(&b[..3], &[0x01, 0xAC, 0x02]);
        assert_eq!(b.len(), 3 + 10);
    }

    #[test]
    fn key_value_layout() {
        let mut b = Vec::new();
        key_values(&mut b, 9, &[KeyValue::int("a", 5)]);
        // 9<<3|2 = 0x4A, len 7: [0x0A 0x01 'a'] [0x12 0x02 [0x18 0x05]]
        assert_eq!(b, vec![0x4A, 7, 0x0A, 1, b'a', 0x12, 2, 0x18, 5]);
    }
}
