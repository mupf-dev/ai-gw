// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Payload-Aufbereitung: sensible Header entfernen, kürzen, maskieren.
//!
//! [`PayloadMasker`] ist die Schnittstelle der Maskierung; Standard ist [`PiiMasker`] mit
//! derselben PII-Engine wie das Guardrail-Plugin `pii` (`aigw-pii`, AIGW-37): E-Mail,
//! Telefon, IBAN, Kreditkarte, Steuer-ID, SVNR, Ausweis/Pass, IP-Adressen, Zugangsdaten in
//! URLs und API-Keys werden durch `[LABEL]` ersetzt. Alles läuft im Writer, nie im Hot Path.

use std::borrow::Cow;
use std::fmt;

use aigw_core::observe::{PayloadMode, RawPayload, is_sensitive_header};
use aigw_pii::Engine;
use serde_json::{Map, Value, json};

/// Maskiert personenbezogene Daten in Text.
pub trait PayloadMasker: Send + Sync + fmt::Debug + 'static {
    /// Liefert `text` mit ersetzten Fundstellen (unverändert ausgeliehen, wenn nichts fiel).
    fn mask<'a>(&self, text: &'a str) -> Cow<'a, str>;
}

/// Maskierung über die PII-Engine: jede Fundstelle wird zu `[LABEL]` (`[EMAIL]`, `[CARD]`,
/// `[IBAN]`, `[PHONE]`, `[API_KEY]` …).
#[derive(Debug, Clone, Default)]
pub struct PiiMasker {
    engine: Engine,
}

impl PiiMasker {
    /// Alle Kategorien inkl. Entropie-Heuristik für API-Keys.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mit eigener Engine (z. B. andere Kategorien oder Allowlist).
    pub fn with_engine(engine: Engine) -> Self {
        Self { engine }
    }
}

impl PayloadMasker for PiiMasker {
    fn mask<'a>(&self, text: &'a str) -> Cow<'a, str> {
        self.engine.mask_labels(text)
    }
}

/// Zeichen, die in keinem maskierten Muster vorkommen: Kürzen an diesen Stellen kann keine
/// Fundstelle zerteilen.
fn is_hard_delimiter(c: char) -> bool {
    matches!(
        c,
        '"' | '\n' | '\r' | ',' | '{' | '}' | '[' | ']' | '<' | '>' | ';'
    )
}

/// Größter Index ≤ `max` auf einer Zeichengrenze.
fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut i = max;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Aufbereiteter Body: Text (ggf. maskiert und gekürzt) bzw. Kennzeichnung als binär.
fn body_value(
    body: &[u8],
    mode: PayloadMode,
    max_bytes: usize,
    masker: &dyn PayloadMasker,
) -> Value {
    let mut obj = Map::new();
    obj.insert("bytes".into(), json!(body.len()));
    if mode == PayloadMode::Metadata {
        return Value::Object(obj);
    }
    let Ok(text) = std::str::from_utf8(body) else {
        obj.insert("binary".into(), Value::Bool(true));
        return Value::Object(obj);
    };
    let (text, truncated) = match mode {
        PayloadMode::Masked => {
            // Erst großzügig vorkürzen (an einem harten Trenner, damit keine Fundstelle
            // zerteilt wird), dann maskieren, dann auf die Zielgröße kürzen.
            let pre = floor_char_boundary(text, max_bytes.saturating_mul(4));
            let pre = if pre < text.len() {
                text[..pre].rfind(is_hard_delimiter).unwrap_or(0)
            } else {
                pre
            };
            let masked = masker.mask(&text[..pre]);
            let cut = floor_char_boundary(&masked, max_bytes);
            let truncated = cut < masked.len() || pre < text.len();
            (masked[..cut].to_owned(), truncated)
        }
        _ => {
            let cut = floor_char_boundary(text, max_bytes);
            (text[..cut].to_owned(), cut < text.len())
        }
    };
    obj.insert("body".into(), Value::String(text));
    if truncated {
        obj.insert("truncated".into(), Value::Bool(true));
    }
    Value::Object(obj)
}

/// Baut den zu speichernden Payload (JSON) für `mode`; `None` bei [`PayloadMode::Off`].
///
/// Header aus [`aigw_core::observe::SENSITIVE_HEADERS`] erscheinen in keinem Modus, auch nicht
/// als Name. Im Modus `metadata` nur Header-Namen und Größen, im Modus `masked` werden auch
/// Header-Werte maskiert.
pub fn build_payload(
    raw: &RawPayload,
    mode: PayloadMode,
    max_bytes: usize,
    masker: &dyn PayloadMasker,
) -> Option<Value> {
    if mode == PayloadMode::Off {
        return None;
    }
    let headers = raw
        .request_headers
        .iter()
        .filter(|(name, _)| !is_sensitive_header(name));
    let headers = match mode {
        PayloadMode::Metadata => Value::Array(
            headers
                .map(|(n, _)| Value::String(n.to_ascii_lowercase()))
                .collect(),
        ),
        _ => Value::Object(
            headers
                .map(|(n, v)| {
                    let v = if mode == PayloadMode::Masked {
                        masker.mask(v).into_owned()
                    } else {
                        v.clone()
                    };
                    (n.to_ascii_lowercase(), Value::String(v))
                })
                .collect(),
        ),
    };
    let mut request = body_value(&raw.request_body, mode, max_bytes, masker);
    if let Value::Object(o) = &mut request {
        o.insert("headers".into(), headers);
    }
    let response = body_value(&raw.response_body, mode, max_bytes, masker);
    Some(json!({ "request": request, "response": response }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask(s: &str) -> String {
        PiiMasker::new().mask(s).into_owned()
    }

    #[test]
    fn email() {
        assert_eq!(
            mask("Mail an alice.b+x@mail.example.org bitte"),
            "Mail an [EMAIL] bitte"
        );
        assert_eq!(mask("kein@ treffer"), "kein@ treffer");
        assert!(matches!(
            PiiMasker::new().mask("nichts zu tun"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn credit_cards_and_iban() {
        assert_eq!(mask("Karte 4111 1111 1111 1111."), "Karte [CARD].");
        assert_eq!(mask("Karte 4111-1111-1111-1111"), "Karte [CARD]");
        assert_eq!(mask("{\"cc\":5555555555554444}"), "{\"cc\":[CARD]}");
        assert_eq!(mask("Nr 4111 1111 1111 1112"), "Nr 4111 1111 1111 1112");
        assert_eq!(mask("4111 1111 1111 1111 22"), "[CARD] 22");
        assert_eq!(mask("ts 1731489600000"), "ts 1731489600000");
        assert_eq!(
            mask("IBAN DE89 3704 0044 0532 0130 00, danke"),
            "IBAN [IBAN], danke"
        );
        assert_eq!(mask("DE89370400440532013001"), "DE89370400440532013001");
    }

    #[test]
    fn same_engine_as_guardrails() {
        // Weitere Kategorien der PII-Engine (AIGW-37) wirken auch im Request-Log.
        assert_eq!(
            mask("Tel +49 30 1234567, Key ghp_1234567890abcdefghijABCDEFGHIJ123456, IP 10.0.0.7"),
            "Tel [PHONE], Key [API_KEY], IP [IP]"
        );
        assert_eq!(
            mask("{\"content\":\"Steuer-ID 86095742719\\n0171 1234567\"}"),
            "{\"content\":\"Steuer-ID [TAX_ID]\\n[PHONE]\"}"
        );
    }

    fn raw() -> RawPayload {
        RawPayload {
            request_headers: vec![
                ("Authorization".into(), "Bearer sk-aigw-geheim".into()),
                ("x-api-key".into(), "sk-ant-geheim".into()),
                ("api-key".into(), "azure-geheim".into()),
                ("Content-Type".into(), "application/json".into()),
                ("X-User".into(), "bob@example.com".into()),
            ],
            request_body: r#"{"messages":[{"role":"user","content":"Ich bin bob@example.com, Karte 4111111111111111"}]}"#.into(),
            response_body: "Antwort an bob@example.com".into(),
            mode: PayloadMode::Masked,
        }
    }

    #[test]
    fn sensitive_headers_never_logged() {
        let m = PiiMasker::new();
        for mode in [
            PayloadMode::Metadata,
            PayloadMode::Masked,
            PayloadMode::Full,
        ] {
            let p = build_payload(&raw(), mode, 1 << 20, &m).unwrap();
            let s = p.to_string().to_ascii_lowercase();
            for needle in ["authorization", "x-api-key", "\"api-key\"", "geheim"] {
                assert!(!s.contains(needle), "{mode:?}: {needle} in {s}");
            }
        }
        assert!(build_payload(&raw(), PayloadMode::Off, 1 << 20, &m).is_none());
    }

    #[test]
    fn modes() {
        let m = PiiMasker::new();
        let meta = build_payload(&raw(), PayloadMode::Metadata, 1 << 20, &m).unwrap();
        assert!(meta["request"].get("body").is_none());
        assert_eq!(
            meta["request"]["headers"],
            json!(["content-type", "x-user"])
        );
        assert_eq!(meta["response"]["bytes"], json!(26));

        let masked = build_payload(&raw(), PayloadMode::Masked, 1 << 20, &m).unwrap();
        let body = masked["request"]["body"].as_str().unwrap();
        assert!(body.contains("Ich bin [EMAIL], Karte [CARD]"), "{body}");
        assert_eq!(masked["request"]["headers"]["x-user"], json!("[EMAIL]"));
        assert_eq!(masked["response"]["body"], json!("Antwort an [EMAIL]"));

        let full = build_payload(&raw(), PayloadMode::Full, 1 << 20, &m).unwrap();
        assert!(
            full["request"]["body"]
                .as_str()
                .unwrap()
                .contains("bob@example.com")
        );
    }

    #[test]
    fn truncation_and_binary() {
        let m = PiiMasker::new();
        let mut r = raw();
        r.response_body = "äöü".repeat(100).into();
        let p = build_payload(&r, PayloadMode::Full, 7, &m).unwrap();
        assert_eq!(p["response"]["body"], json!("äöü")); // auf Zeichengrenze gekürzt
        assert_eq!(p["response"]["truncated"], json!(true));

        // Vorkürzen (4 × max) an hartem Trenner zerteilt keine E-Mail.
        r.response_body = format!("{}\"bob@example.com\"", "x".repeat(30)).into();
        let p = build_payload(&r, PayloadMode::Masked, 10, &m).unwrap();
        let body = p["response"]["body"].as_str().unwrap();
        assert!(!body.contains("bob"), "{body}");

        r.request_body = vec![0xff, 0xfe, 0x00].into();
        let p = build_payload(&r, PayloadMode::Masked, 100, &m).unwrap();
        assert_eq!(p["request"]["binary"], json!(true));
        assert_eq!(p["request"]["bytes"], json!(3));
    }
}
