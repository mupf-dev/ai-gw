// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Payload-Aufbereitung: sensible Header entfernen, kürzen, maskieren.
//!
//! [`PayloadMasker`] ist die Schnittstelle für die PII-Engine (AIGW-37). Bis dahin maskiert
//! [`RegexMasker`] als Basis E-Mail-Adressen, Kreditkartennummern (Luhn-geprüft) und IBANs
//! (Prüfziffer mod 97). Alles läuft im Writer, nie im Hot Path.

use std::borrow::Cow;
use std::fmt;

use aigw_core::observe::{PayloadMode, RawPayload, is_sensitive_header};
use regex::Regex;
use serde_json::{Map, Value, json};

/// Maskiert personenbezogene Daten in Text (Schnittstelle für die PII-Engine, AIGW-37).
pub trait PayloadMasker: Send + Sync + fmt::Debug + 'static {
    /// Liefert `text` mit ersetzten Fundstellen (unverändert ausgeliehen, wenn nichts fiel).
    fn mask<'a>(&self, text: &'a str) -> Cow<'a, str>;
}

/// Ersatztext für E-Mail-Adressen.
pub const EMAIL_TOKEN: &str = "[EMAIL]";
/// Ersatztext für Kreditkartennummern.
pub const CARD_TOKEN: &str = "[CARD]";
/// Ersatztext für IBANs.
pub const IBAN_TOKEN: &str = "[IBAN]";

/// Basis-Maskierung über reguläre Ausdrücke.
#[derive(Debug, Clone)]
pub struct RegexMasker {
    email: Regex,
    card: Regex,
    iban: Regex,
}

impl Default for RegexMasker {
    fn default() -> Self {
        Self::new()
    }
}

impl RegexMasker {
    /// Erzeugt den Masker (Muster sind fest und geprüft).
    #[allow(clippy::expect_used)] // feste, getestete Muster
    pub fn new() -> Self {
        Self {
            email: Regex::new(
                r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9\-]+(?:\.[A-Za-z0-9\-]+)*\.[A-Za-z]{2,24}",
            )
            .expect("E-Mail-Muster"),
            // 13–19 Ziffern, optional mit Leerzeichen/Bindestrich gruppiert.
            card: Regex::new(r"\b[0-9](?:[ \-]?[0-9]){12,18}\b").expect("Karten-Muster"),
            // Ländercode, Prüfziffern, 11–30 alphanumerische Zeichen (optional in Gruppen).
            iban: Regex::new(r"\b[A-Z]{2}[0-9]{2}(?:[ ]?[A-Z0-9]){11,30}\b").expect("IBAN-Muster"),
        }
    }
}

impl PayloadMasker for RegexMasker {
    fn mask<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let t = self.email.replace_all(text, EMAIL_TOKEN);
        let t = replace_validated(t, &self.iban, 15, is_valid_iban, IBAN_TOKEN);
        replace_validated(t, &self.card, 13, is_card_number, CARD_TOKEN)
    }
}

/// Ersetzt Treffer von `re`, deren längster gültiger Anfang (≥ `min_alnum` Zeichen ohne
/// Trenner) `valid` erfüllt. Gierige Treffer, die angrenzende Ziffern/Buchstaben mitnehmen
/// (z. B. „4111 1111 1111 1111 2024“), werden so von hinten verkürzt.
fn replace_validated<'a>(
    text: Cow<'a, str>,
    re: &Regex,
    min_alnum: usize,
    valid: fn(&str) -> bool,
    token: &str,
) -> Cow<'a, str> {
    let mut out: Option<String> = None;
    let mut last = 0;
    for m in re.find_iter(&text) {
        let s = m.as_str();
        // Kandidaten-Enden: nach jedem alphanumerischen Zeichen, längste zuerst.
        let compact_len = s.bytes().filter(u8::is_ascii_alphanumeric).count();
        let mut hit = None;
        let mut alnum = compact_len;
        for (i, c) in s.char_indices().rev() {
            if !c.is_ascii_alphanumeric() {
                continue;
            }
            if alnum < min_alnum {
                break;
            }
            let end = i + c.len_utf8();
            if valid(&s[..end]) {
                hit = Some(end);
                break;
            }
            alnum -= 1;
        }
        if let Some(end) = hit {
            let buf = out.get_or_insert_with(|| String::with_capacity(text.len()));
            buf.push_str(&text[last..m.start()]);
            buf.push_str(token);
            last = m.start() + end;
        }
    }
    match out {
        None => text,
        Some(mut buf) => {
            buf.push_str(&text[last..]);
            Cow::Owned(buf)
        }
    }
}

/// Luhn-Prüfung über die Ziffern von `s` (Trenner werden ignoriert); 13–19 Ziffern,
/// erste Ziffer 2–6 (Kartennetze Amex, Visa, Mastercard, Discover …).
pub fn is_card_number(s: &str) -> bool {
    let digits: Vec<u32> = s.chars().filter_map(|c| c.to_digit(10)).collect();
    if !(13..=19).contains(&digits.len()) || !matches!(digits.first(), Some(2..=6)) {
        return false;
    }
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &d)| {
            if i % 2 == 1 {
                let x = d * 2;
                if x > 9 { x - 9 } else { x }
            } else {
                d
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

/// IBAN-Prüfung (ISO 13616, mod 97 = 1); Leerzeichen werden ignoriert, 15–34 Zeichen.
pub fn is_valid_iban(s: &str) -> bool {
    let compact: Vec<u8> = s.bytes().filter(|b| *b != b' ').collect();
    if !(15..=34).contains(&compact.len())
        || !compact.iter().all(u8::is_ascii_alphanumeric)
        || !compact[..2].iter().all(u8::is_ascii_uppercase)
        || !compact[2..4].iter().all(u8::is_ascii_digit)
    {
        return false;
    }
    let mut rem: u32 = 0;
    for &b in compact[4..].iter().chain(&compact[..4]) {
        let v = match b {
            b'0'..=b'9' => u32::from(b - b'0'),
            b'A'..=b'Z' => u32::from(b - b'A') + 10,
            _ => return false,
        };
        rem = if v >= 10 {
            (rem * 100 + v) % 97
        } else {
            (rem * 10 + v) % 97
        };
    }
    rem == 1
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
        RegexMasker::new().mask(s).into_owned()
    }

    #[test]
    fn email() {
        assert_eq!(
            mask("Mail an alice.b+x@mail.example.org bitte"),
            "Mail an [EMAIL] bitte"
        );
        assert_eq!(mask("kein@ treffer"), "kein@ treffer");
        assert!(matches!(
            RegexMasker::new().mask("nichts zu tun"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn credit_cards_with_luhn() {
        assert!(is_card_number("4111111111111111"));
        assert!(is_card_number("4111 1111 1111 1111"));
        assert!(is_card_number("378282246310005")); // Amex
        assert!(!is_card_number("4111111111111112")); // Luhn falsch
        assert!(!is_card_number("1111111111111117")); // kein Kartennetz
        assert_eq!(mask("Karte 4111 1111 1111 1111."), "Karte [CARD].");
        assert_eq!(mask("Karte 4111-1111-1111-1111"), "Karte [CARD]");
        assert_eq!(mask("{\"cc\":5555555555554444}"), "{\"cc\":[CARD]}");
        // Luhn-ungültig → bleibt.
        assert_eq!(mask("Nr 4111 1111 1111 1112"), "Nr 4111 1111 1111 1112");
        // Angrenzende Ziffern: gieriger Treffer wird verkürzt.
        assert_eq!(mask("4111 1111 1111 1111 22"), "[CARD] 22");
        // Zeitstempel (13 Ziffern, beginnt mit 1) wird nicht maskiert.
        assert_eq!(mask("ts 1731489600000"), "ts 1731489600000");
    }

    #[test]
    fn iban_mod97() {
        assert!(is_valid_iban("DE89370400440532013000"));
        assert!(is_valid_iban("DE89 3704 0044 0532 0130 00"));
        assert!(is_valid_iban("GB82WEST12345698765432"));
        assert!(!is_valid_iban("DE89370400440532013001"));
        assert_eq!(
            mask("IBAN DE89 3704 0044 0532 0130 00, danke"),
            "IBAN [IBAN], danke"
        );
        assert_eq!(mask("DE89370400440532013000 EUR"), "[IBAN] EUR");
        assert_eq!(mask("DE89370400440532013001"), "DE89370400440532013001");
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
        }
    }

    #[test]
    fn sensitive_headers_never_logged() {
        let m = RegexMasker::new();
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
        let m = RegexMasker::new();
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
        let m = RegexMasker::new();
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
