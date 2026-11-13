// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Cache-Schlüssel (AIGW-39) und Partition des semantischen Caches (AIGW-40).
//!
//! **Exakter Schlüssel** = BLAKE3 über
//! `"aigw-cache/v1" ‖ Scope ‖ Modellgruppe ‖ kanonisches JSON der normalisierten IR-Anfrage`.
//!
//! Normalisierung: Die IR-Anfrage wird nach JSON serialisiert; entfernt werden `model` (die
//! kanonische Modellgruppe steht separat im Schlüssel – Aliasse teilen sich Einträge), `user`,
//! `metadata`, `stream`, `stream_options`, `source` (OpenAI- und Anthropic-Eingang teilen sich
//! Einträge) und die Format-Hinweise außer `anthropic_beta`, außerdem `cache_control` an
//! Inhaltsteilen und Tools (Anbieter-Prompt-Caching ändert die Antwort nicht). Das kanonische
//! JSON sortiert Objektschlüssel rekursiv (unabhängig von der Reihenfolge im Client-Body und
//! von `serde_json`-Features) und schreibt Zahlen in der Normalform von `serde_json`.
//!
//! **Partition** (semantisch, harter Filter) = BLAKE3 über Scope, Modellgruppe und die
//! normalisierte Anfrage **ohne** die letzte Nutzernachricht – also Systemprompt, Tools,
//! Tool-Wahl, Antwortformat, Sampling-Parameter und bisheriger Verlauf. Nur die letzte
//! Nutzernachricht wird per Embedding verglichen.

use std::fmt;

use aigw_core::ir::{ChatRequest, Content, PartKind, Role};
use serde_json::Value;

/// Scope eines Cache-Eintrags (aufgelöst aus Key, Team, Organisation).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Scope {
    /// Je API-Key (ID).
    Key(String),
    /// Je Team (ID).
    Team(String),
    /// Je Organisation (ID).
    Organization(String),
    /// Ohne Schlüsselprüfung (Tests/Entwicklung, `AllowAll`).
    Anonymous,
    /// Mandantenübergreifend (nur mit ausdrücklicher Richtlinie `scope: global`).
    Global,
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Key(id) => write!(f, "key:{id}"),
            Self::Team(id) => write!(f, "team:{id}"),
            Self::Organization(id) => write!(f, "org:{id}"),
            Self::Anonymous => f.write_str("anonymous"),
            Self::Global => f.write_str("global"),
        }
    }
}

/// 32-Byte-BLAKE3-Hash (Schlüssel bzw. Partition).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hash32(pub [u8; 32]);

impl Hash32 {
    /// Kleinbuchstaben-Hex (64 Zeichen).
    pub fn to_hex(&self) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut s = String::with_capacity(64);
        for b in self.0 {
            s.push(char::from(DIGITS[usize::from(b >> 4)]));
            s.push(char::from(DIGITS[usize::from(b & 0xf)]));
        }
        s
    }

    /// Aus Hex (64 Zeichen).
    pub fn from_hex(s: &str) -> Option<Self> {
        let b = s.as_bytes();
        if b.len() != 64 {
            return None;
        }
        let nib = |c: u8| match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        };
        let mut out = [0u8; 32];
        for (i, o) in out.iter_mut().enumerate() {
            *o = (nib(b[2 * i])? << 4) | nib(b[2 * i + 1])?;
        }
        Some(Self(out))
    }
}

impl fmt::Debug for Hash32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hex = self.to_hex();
        write!(f, "{}…", &hex[..12])
    }
}

const EXACT_DOMAIN: &[u8] = b"aigw-cache/exact/v1\0";
const EMBEDDINGS_DOMAIN: &[u8] = b"aigw-cache/embeddings/v1\0";
const PARTITION_DOMAIN: &[u8] = b"aigw-cache/partition/v1\0";

/// Normalisierte Anfrage als JSON-Wert (siehe Moduldoku).
pub fn normalized(req: &ChatRequest) -> Value {
    let mut v = serde_json::to_value(req).unwrap_or(Value::Null);
    if let Value::Object(o) = &mut v {
        for k in [
            "model",
            "user",
            "metadata",
            "stream",
            "stream_options",
            "source",
        ] {
            o.remove(k);
        }
        // Format-Hinweise: nur Beta-Kennungen beeinflussen die Antwort.
        let beta = o
            .get("hints")
            .and_then(|h| h.get("anthropic_beta"))
            .cloned();
        o.remove("hints");
        if let Some(b) = beta {
            o.insert("anthropic_beta".into(), b);
        }
        if let Some(Value::Array(msgs)) = o.get_mut("messages") {
            for m in msgs {
                if let Some(Value::Object(content)) = m.get_mut("content")
                    && let Some(Value::Array(parts)) = content.get_mut("parts")
                {
                    for p in parts {
                        if let Value::Object(p) = p {
                            p.remove("cache_control");
                        }
                    }
                }
            }
        }
        if let Some(Value::Array(tools)) = o.get_mut("tools") {
            for t in tools {
                if let Value::Object(t) = t {
                    t.remove("cache_control");
                }
            }
        }
    }
    v
}

/// Schreibt `v` als kanonisches JSON (Objektschlüssel sortiert, keine Leerzeichen).
pub fn write_canonical(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Value::Number(n) => out.extend_from_slice(n.to_string().as_bytes()),
        Value::String(s) => write_string(s, out),
        Value::Array(a) => {
            out.push(b'[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(x, out);
            }
            out.push(b']');
        }
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort_unstable();
            out.push(b'{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_string(k, out);
                out.push(b':');
                if let Some(x) = o.get(k) {
                    write_canonical(x, out);
                }
            }
            out.push(b'}');
        }
    }
}

fn write_string(s: &str, out: &mut Vec<u8>) {
    // serde_json maskiert eindeutig (RFC 8259); ein String kann nicht scheitern.
    match serde_json::to_vec(s) {
        Ok(b) => out.extend_from_slice(&b),
        Err(_) => out.extend_from_slice(b"\"\""),
    }
}

fn hash_with(domain: &[u8], scope: &Scope, group: &str, body: &Value) -> Hash32 {
    let mut buf = Vec::with_capacity(512);
    buf.extend_from_slice(domain);
    buf.extend_from_slice(scope.to_string().as_bytes());
    buf.push(0);
    buf.extend_from_slice(group.as_bytes());
    buf.push(0);
    write_canonical(body, &mut buf);
    Hash32(*blake3::hash(&buf).as_bytes())
}

/// Exakter Schlüssel.
pub fn exact_key(scope: &Scope, model_group: &str, req: &ChatRequest) -> Hash32 {
    hash_with(EXACT_DOMAIN, scope, model_group, &normalized(req))
}

/// Exakter Schlüssel einer Embedding-Anfrage (F174): Eingaben, Dimension und unbekannte
/// Felder; ohne Modellname (kanonische Gruppe separat), `user`, Ausgabeformat (die IR-Antwort
/// wird je Anfrage kodiert) und Ursprungsformat.
pub fn embeddings_key(
    scope: &Scope,
    model_group: &str,
    req: &aigw_core::ir::embeddings::EmbeddingsRequest,
) -> Hash32 {
    let body = serde_json::json!({
        "input": req.input,
        "dimensions": req.dimensions,
        "extra": req.extra,
    });
    hash_with(EMBEDDINGS_DOMAIN, scope, model_group, &body)
}

/// Text der letzten Nachricht, falls sie eine reine Text-Nutzernachricht ist (sonst `None`:
/// Bilder, Dateien, Tool-Ergebnisse werden nur exakt gecacht).
pub fn semantic_text(req: &ChatRequest) -> Option<String> {
    let last = req.messages.last()?;
    if last.role != Role::User {
        return None;
    }
    let text = match &last.content {
        Content::Text(t) => t.clone(),
        Content::Parts(parts) => {
            let mut out = String::new();
            for p in parts {
                match &p.kind {
                    PartKind::Text { text } => {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(text);
                    }
                    _ => return None,
                }
            }
            out
        }
    };
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Partition des semantischen Caches (harter Filter, siehe Moduldoku).
pub fn partition(scope: &Scope, model_group: &str, req: &ChatRequest) -> Hash32 {
    let mut v = normalized(req);
    if let Some(Value::Array(msgs)) = v.get_mut("messages") {
        msgs.pop();
    }
    hash_with(PARTITION_DOMAIN, scope, model_group, &v)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use aigw_core::ir::{Message, Part, StreamOptions};
    use aigw_core::wire::{WireFormat, anthropic, openai};

    use super::*;

    fn req(text: &str) -> ChatRequest {
        let mut r = ChatRequest {
            model: "m".into(),
            messages: vec![
                Message::text(Role::System, "sys"),
                Message::text(Role::User, text),
            ],
            ..ChatRequest::default()
        };
        r.sampling.temperature = Some(0.0);
        r
    }

    #[test]
    fn ignores_user_stream_metadata_model_and_field_order() {
        let s = Scope::Team("t1".into());
        let a = req("hallo");
        let mut b = a.clone();
        b.model = "alias".into();
        b.user = Some("u".into());
        b.stream = Some(true);
        b.stream_options = Some(StreamOptions::default());
        b.metadata = Some(serde_json::Map::new());
        b.source = Some(WireFormat::AnthropicMessages);
        b.hints.legacy_max_tokens = true;
        assert_eq!(exact_key(&s, "g", &a), exact_key(&s, "g", &b));

        // Reihenfolge im Client-Body egal.
        let x = openai::decode_request(
            br#"{"model":"m","temperature":0,"messages":[{"role":"user","content":"hi"}],"response_format":{"type":"json_schema","json_schema":{"name":"n","schema":{"b":1,"a":2}}}}"#,
        )
        .unwrap();
        let y = openai::decode_request(
            br#"{"response_format":{"json_schema":{"schema":{"a":2,"b":1},"name":"n"},"type":"json_schema"},"messages":[{"content":"hi","role":"user"}],"temperature":0,"model":"m"}"#,
        )
        .unwrap();
        assert_eq!(exact_key(&s, "g", &x), exact_key(&s, "g", &y));
    }

    #[test]
    fn differs_by_scope_group_content_and_params() {
        let s = Scope::Team("t1".into());
        let a = req("hallo");
        let k = exact_key(&s, "g", &a);
        assert_ne!(k, exact_key(&Scope::Team("t2".into()), "g", &a));
        assert_ne!(k, exact_key(&Scope::Key("t1".into()), "g", &a));
        assert_ne!(k, exact_key(&s, "h", &a));
        assert_ne!(k, exact_key(&s, "g", &req("hallo!")));
        let mut b = a.clone();
        b.max_tokens = Some(10);
        assert_ne!(k, exact_key(&s, "g", &b));
        let mut c = a.clone();
        c.hints.anthropic_beta = vec!["x".into()];
        assert_ne!(k, exact_key(&s, "g", &c));
    }

    #[test]
    fn cache_control_ignored_and_formats_share_keys() {
        let s = Scope::Global;
        let mut a = req("x");
        a.messages[1].content = Content::Parts(vec![Part::text("x")]);
        let mut b = a.clone();
        if let Content::Parts(p) = &mut b.messages[1].content {
            p[0].cache_control = Some(serde_json::json!({"type": "ephemeral"}));
        }
        assert_eq!(exact_key(&s, "g", &a), exact_key(&s, "g", &b));

        let o = openai::decode_request(
            br#"{"model":"m","max_tokens":5,"temperature":0,"messages":[{"role":"user","content":"hi"}]}"#,
        )
        .unwrap();
        let an = anthropic::decode_request(
            br#"{"model":"m","max_tokens":5,"temperature":0,"messages":[{"role":"user","content":"hi"}]}"#,
        )
        .unwrap();
        assert_eq!(exact_key(&s, "g", &o), exact_key(&s, "g", &an));
    }

    #[test]
    fn partition_ignores_last_user_message_only() {
        let s = Scope::Team("t".into());
        assert_eq!(
            partition(&s, "g", &req("a")),
            partition(&s, "g", &req("ganz anders"))
        );
        let mut b = req("a");
        b.messages[0] = Message::text(Role::System, "anderer Systemprompt");
        assert_ne!(partition(&s, "g", &req("a")), partition(&s, "g", &b));
        assert_eq!(semantic_text(&req("  hi ")).as_deref(), Some("hi"));
        let mut t = req("a");
        t.messages.push(Message::text(Role::Assistant, "x"));
        assert_eq!(semantic_text(&t), None);
    }

    #[test]
    fn hex_roundtrip() {
        let h = exact_key(&Scope::Anonymous, "g", &req("a"));
        assert_eq!(Hash32::from_hex(&h.to_hex()), Some(h));
        assert_eq!(Hash32::from_hex("zz"), None);
    }
}
