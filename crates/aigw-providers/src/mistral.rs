// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Mistral (`provider: mistral`, AIGW-20) – Profil des Adapters der OpenAI-Familie.
//!
//! Mistral spricht Chat Completions mit wenigen Abweichungen; ein eigener Adapter würde
//! Stream-Decoder, Antwort-Parser und Fehlerbehandlung verdoppeln. Daher ein Profil von
//! [`OpenAiAdapter`](crate::openai::OpenAiAdapter), das nur die Unterschiede behandelt:
//!
//! | | OpenAI | Mistral (Profil) |
//! |---|---|---|
//! | Seed | `seed` | `random_seed` |
//! | `tool_choice` „mindestens ein Tool“ | `required` | `any` |
//! | Tool-Aufruf-IDs | beliebig | genau 9 Zeichen `[A-Za-z0-9]` – fremde IDs (z. B. `toolu_…` aus Anthropic-Verläufen) werden deterministisch umgeschrieben |
//! | Ausgabegrenze | `max_completion_tokens` | `max_tokens` |
//! | Usage im Stream | `stream_options.include_usage` | kommt immer im letzten Chunk; `stream_options` wird nicht gesendet |
//! | nicht unterstützte Felder | – | `user`, `metadata`, `reasoning_effort`, `stream_options` werden verworfen (Mistral lehnt unbekannte Felder mit 422 ab) |
//! | `safe_prompt` | – | `settings.safePrompt` (Standard: nicht gesetzt); von OpenAI-Clients als unbekanntes Feld durchgereicht |
//! | Endgrund | `length` | zusätzlich `model_length` (→ `max_tokens`) und `error` |
//! | Fehler | `{"error":{…}}` | `{"object":"error","message":…,"type":…}` bzw. `{"detail":[…]}` (422) |
//!
//! Standard-Endpunkt `https://api.mistral.ai/v1`, `Authorization: Bearer` (Pflicht).

use aigw_core::ir::{ChatResponse, StopReason, UpstreamError};
use aigw_core::wire::Losses;
use aigw_core::wire::openai::ChatCompletionRequest;
use http::{HeaderMap, StatusCode};
use serde_json::{Value, json};

use crate::openai::classify_openai_error;

/// Standard-Endpunkt von Mistral.
pub const MISTRAL_DEFAULT_ENDPOINT: &str = "https://api.mistral.ai/v1";

/// Passt eine OpenAI-Wire-Anfrage an Mistral an.
pub(crate) fn adjust_request(
    wire: &mut ChatCompletionRequest,
    seed: Option<i64>,
    safe_prompt: Option<bool>,
    losses: &mut Losses,
) {
    if let Some(s) = seed {
        wire.extra.insert("random_seed".into(), Value::from(s));
    }
    if let Some(Value::String(tc)) = &wire.tool_choice
        && tc == "required"
    {
        wire.tool_choice = Some(json!("any"));
    }
    if wire.max_completion_tokens.is_some() && wire.max_tokens.is_none() {
        wire.max_tokens = wire.max_completion_tokens.take();
    }
    wire.stream_options = None;
    if wire.user.take().is_some() {
        losses.note("user wird von Mistral nicht unterstützt");
    }
    if wire.metadata.take().is_some() {
        losses.note("metadata wird von Mistral nicht unterstützt");
    }
    if let Some(e) = wire.reasoning_effort.take() {
        losses.note(format!(
            "reasoning_effort '{e}' wird von Mistral nicht unterstützt"
        ));
    }
    if let Some(sp) = safe_prompt {
        wire.extra.entry("safe_prompt").or_insert(Value::Bool(sp));
    }
    for m in &mut wire.messages {
        if let Some(calls) = m.tool_calls.as_mut() {
            for c in calls {
                normalize_tool_id(&mut c.id);
            }
        }
        if let Some(id) = m.tool_call_id.as_mut() {
            normalize_tool_id(id);
        }
    }
}

fn valid_tool_id(id: &str) -> bool {
    id.len() == 9 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Schreibt eine Tool-Aufruf-ID auf das von Mistral verlangte Format um (9 alphanumerische
/// Zeichen). Deterministisch (FNV-1a), damit Aufruf und Ergebnis dieselbe ID bekommen.
fn normalize_tool_id(id: &mut String) {
    if valid_tool_id(id) {
        return;
    }
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in id.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    let mut out = String::with_capacity(9);
    for _ in 0..9 {
        let i = usize::try_from(h % 62).unwrap_or(0);
        out.push(char::from(ALPHABET[i]));
        h /= 62;
    }
    *id = out;
}

/// Mistral-Endgründe, die die gemeinsame Abbildung nicht kennt.
pub(crate) fn map_stop(reason: &mut StopReason) {
    if let StopReason::Other(r) = reason
        && r == "model_length"
    {
        *reason = StopReason::MaxTokens;
    }
}

/// Nachbearbeitung einer nicht gestreamten Antwort.
pub(crate) fn adjust_response(resp: &mut ChatResponse) {
    for c in &mut resp.choices {
        if let Some(r) = c.stop_reason.as_mut() {
            map_stop(r);
        }
    }
}

/// Klassifiziert eine Mistral-Fehlerantwort (Formate in OpenAI-Form gebracht).
pub fn classify_mistral_error(
    status: StatusCode,
    headers: &HeaderMap,
    body: &[u8],
) -> UpstreamError {
    let normalized = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| {
            if v.get("error").is_some() {
                return None;
            }
            let o = v.as_object()?;
            if let Some(m) = o.get("message") {
                let message = match m {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                return Some(json!({"error": {
                    "message": message,
                    "type": o.get("type").cloned().unwrap_or(Value::Null),
                    "code": o.get("code").cloned().unwrap_or(Value::Null),
                    "param": o.get("param").cloned().unwrap_or(Value::Null),
                }}));
            }
            let detail = o.get("detail")?;
            let message = match detail {
                Value::String(s) => s.clone(),
                Value::Array(items) => items
                    .iter()
                    .map(|d| {
                        let loc = d
                            .get("loc")
                            .and_then(Value::as_array)
                            .map(|l| {
                                l.iter()
                                    .map(|p| match p {
                                        Value::String(s) => s.clone(),
                                        other => other.to_string(),
                                    })
                                    .collect::<Vec<_>>()
                                    .join(".")
                            })
                            .unwrap_or_default();
                        let msg = d.get("msg").and_then(Value::as_str).unwrap_or_default();
                        format!("{loc}: {msg}")
                    })
                    .collect::<Vec<_>>()
                    .join("; "),
                other => other.to_string(),
            };
            Some(json!({"error": {"message": message, "type": "invalid_request_error"}}))
        })
        .map(|v| v.to_string().into_bytes());
    classify_openai_error(status, headers, normalized.as_deref().unwrap_or(body))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use aigw_core::ir::{ErrorClass, ErrorKind};

    use super::*;

    #[test]
    fn tool_ids_are_normalized_deterministically() {
        let mut a = "toolu_01ABCDEF".to_owned();
        let mut b = "toolu_01ABCDEF".to_owned();
        normalize_tool_id(&mut a);
        normalize_tool_id(&mut b);
        assert_eq!(a, b);
        assert!(valid_tool_id(&a), "{a}");
        let mut ok = "abc123XYZ".to_owned();
        normalize_tool_id(&mut ok);
        assert_eq!(ok, "abc123XYZ");
    }

    #[test]
    fn classifies_mistral_error_shapes() {
        let h = HeaderMap::new();
        let e = classify_mistral_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            &h,
            br#"{"detail":[{"type":"extra_forbidden","loc":["body","foo"],"msg":"Extra inputs are not permitted"}]}"#,
        );
        assert_eq!(e.class, ErrorClass::InvalidRequest);
        assert_eq!(e.error.message, "body.foo: Extra inputs are not permitted");

        let e = classify_mistral_error(
            StatusCode::BAD_REQUEST,
            &h,
            br#"{"object":"error","message":"Prompt contains 40000 tokens, too large for model with 32768 maximum context length","type":"invalid_request_error","param":null,"code":null}"#,
        );
        assert_eq!(e.class, ErrorClass::ContextLength);

        let e = classify_mistral_error(
            StatusCode::TOO_MANY_REQUESTS,
            &h,
            br#"{"message":"Requests rate limit exceeded"}"#,
        );
        assert_eq!(e.class, ErrorClass::RateLimit);
        assert_eq!(e.error.kind, ErrorKind::RateLimit);
        assert_eq!(e.error.message, "Requests rate limit exceeded");
    }
}
