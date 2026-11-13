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
//! | nicht unterstützte Felder | – | `user`, `metadata`, `stream_options` werden verworfen (Mistral lehnt unbekannte Felder mit 422 ab) |
//! | `reasoning_effort` | – | `settings.reasoningMode`: `prompt-mode` (Standard) → `prompt_mode: "reasoning"` (Magistral-System-Prompt; `none` → ohne), `effort` → `reasoning_effort` durchreichen (neuere Modelle), `off` → verwerfen (F61) |
//! | Reasoning (Magistral) | `reasoning_content` | `content` als Liste: `{"type":"thinking","thinking":[{"type":"text",…}]}` + `{"type":"text",…}` → IR-Reasoning + Text, auch im Stream (F61) |
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

/// Abbildung von `reasoning_effort` auf Mistral (`settings.reasoningMode`, F61).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MistralReasoning {
    /// `prompt_mode: "reasoning"` (Magistral-Reasoning-System-Prompt); `none` → ohne.
    #[default]
    PromptMode,
    /// `reasoning_effort` unverändert weitergeben (Modelle mit Effort-Steuerung).
    Effort,
    /// `reasoning_effort` verwerfen.
    Off,
}

impl MistralReasoning {
    /// Aus `settings.reasoningMode` (`prompt-mode` | `effort` | `off`).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "prompt-mode" => Some(Self::PromptMode),
            "effort" => Some(Self::Effort),
            "off" => Some(Self::Off),
            _ => None,
        }
    }
}

/// Fasst eine Magistral-`content`-Liste zusammen: Text → `content` (String), `thinking`-Blöcke
/// → `reasoning_content` (wie bei anderen OpenAI-kompatiblen Reasoning-Servern).
fn normalize_message(msg: &mut serde_json::Map<String, Value>) {
    let Some(Value::Array(blocks)) = msg.get("content") else {
        return;
    };
    let mut text = String::new();
    let mut reasoning = String::new();
    for b in blocks {
        match b.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = b.get("text").and_then(Value::as_str) {
                    text.push_str(t);
                }
            }
            Some("thinking") => match b.get("thinking") {
                Some(Value::String(t)) => reasoning.push_str(t),
                Some(Value::Array(parts)) => {
                    for p in parts {
                        if let Some(t) = p.get("text").and_then(Value::as_str) {
                            reasoning.push_str(t);
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    msg.insert("content".into(), Value::String(text));
    if !reasoning.is_empty() {
        msg.insert("reasoning_content".into(), Value::String(reasoning));
    }
}

/// Nicht gestreamte Antwort mit Magistral-Blöcken normalisieren (`None` = unverändert).
pub(crate) fn normalize_response(body: &[u8]) -> Option<Vec<u8>> {
    let mut v: Value = serde_json::from_slice(body).ok()?;
    let mut changed = false;
    for c in v.get_mut("choices")?.as_array_mut()? {
        if let Some(Value::Object(m)) = c.get_mut("message")
            && matches!(m.get("content"), Some(Value::Array(_)))
        {
            normalize_message(m);
            changed = true;
        }
    }
    changed.then(|| v.to_string().into_bytes())
}

/// Stream-Chunk mit Magistral-Blöcken normalisieren (`None` = unverändert).
pub(crate) fn normalize_chunk(data: &str) -> Option<String> {
    let mut v: Value = serde_json::from_str(data).ok()?;
    let mut changed = false;
    for c in v.get_mut("choices")?.as_array_mut()? {
        if let Some(Value::Object(m)) = c.get_mut("delta")
            && matches!(m.get("content"), Some(Value::Array(_)))
        {
            normalize_message(m);
            changed = true;
        }
    }
    changed.then(|| v.to_string())
}

/// Standard-Endpunkt von Mistral.
pub const MISTRAL_DEFAULT_ENDPOINT: &str = "https://api.mistral.ai/v1";

/// Passt eine OpenAI-Wire-Anfrage an Mistral an.
pub(crate) fn adjust_request(
    wire: &mut ChatCompletionRequest,
    seed: Option<i64>,
    safe_prompt: Option<bool>,
    reasoning: MistralReasoning,
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
        match reasoning {
            MistralReasoning::PromptMode if e != "none" => {
                wire.extra
                    .entry("prompt_mode")
                    .or_insert_with(|| Value::from("reasoning"));
            }
            MistralReasoning::PromptMode => {}
            MistralReasoning::Effort => wire.reasoning_effort = Some(e),
            MistralReasoning::Off => losses.note(format!(
                "reasoning_effort '{e}' verworfen (reasoningMode: off)"
            )),
        }
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
    fn magistral_blocks_become_reasoning() {
        let body = json!({"id": "c", "object": "chat.completion", "created": 1,
            "model": "magistral-medium",
            "choices": [{"index": 0, "finish_reason": "stop", "message": {"role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": [{"type": "text", "text": "erst "},
                                                       {"type": "text", "text": "denken"}]},
                    {"type": "text", "text": "42"}]}}]})
        .to_string();
        let out = normalize_response(body.as_bytes()).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["choices"][0]["message"]["content"], "42");
        assert_eq!(
            v["choices"][0]["message"]["reasoning_content"],
            "erst denken"
        );
        assert!(normalize_response(br#"{"choices":[{"message":{"content":"x"}}]}"#).is_none());
        let chunk = r#"{"id":"c","choices":[{"index":0,"delta":{"content":[{"type":"thinking","thinking":[{"type":"text","text":"hm"}]}]}}]}"#;
        let v: Value = serde_json::from_str(&normalize_chunk(chunk).unwrap()).unwrap();
        assert_eq!(v["choices"][0]["delta"]["reasoning_content"], "hm");
        assert_eq!(v["choices"][0]["delta"]["content"], "");
    }

    #[test]
    fn reasoning_effort_mapping() {
        let wire = |mode: MistralReasoning, effort: &str| {
            let mut w: ChatCompletionRequest = serde_json::from_value(json!({
                "model": "magistral-small", "messages": [], "reasoning_effort": effort
            }))
            .unwrap();
            adjust_request(&mut w, None, None, mode, &mut Losses::new());
            serde_json::to_value(&w).unwrap()
        };
        let v = wire(MistralReasoning::PromptMode, "high");
        assert_eq!(v["prompt_mode"], "reasoning");
        assert!(v.get("reasoning_effort").is_none());
        let v = wire(MistralReasoning::PromptMode, "none");
        assert!(v.get("prompt_mode").is_none());
        let v = wire(MistralReasoning::Effort, "low");
        assert_eq!(v["reasoning_effort"], "low");
        let v = wire(MistralReasoning::Off, "high");
        assert!(v.get("reasoning_effort").is_none() && v.get("prompt_mode").is_none());
        assert_eq!(
            MistralReasoning::parse("effort"),
            Some(MistralReasoning::Effort)
        );
        assert!(MistralReasoning::parse("viel").is_none());
    }

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
