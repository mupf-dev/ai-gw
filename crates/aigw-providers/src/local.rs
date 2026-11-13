// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Ollama und vLLM (`provider: ollama` / `vllm`, AIGW-17) – Profile des Adapters der
//! OpenAI-Familie über deren OpenAI-kompatible Endpunkte (`/v1/chat/completions`).
//!
//! | | Ollama | vLLM |
//! |---|---|---|
//! | Endpunkt | Standard `http://localhost:11434/v1` | Pflicht (z. B. `http://vllm:8000/v1`) |
//! | Auth | optional `Authorization: Bearer` (z. B. hinter einem Reverse-Proxy) | optional (`--api-key`) |
//! | Ausgabegrenze | `max_tokens` | `max_tokens` |
//! | Usage im Stream | `stream_options.include_usage` wird gesendet; ältere Versionen ignorieren es – die Usage wird dann aus dem letzten Chunk gelesen (jeder Chunk mit `usage` zählt), sonst geschätzt | `stream_options.include_usage` |
//! | Reasoning (Antwort) | `reasoning` im Delta/in der Nachricht | `reasoning_content` (neuere Versionen auch `reasoning`) |
//! | `top_k` | – (verworfen) | als Zusatzparameter `top_k` |
//!
//! Reasoning-Text wird zur IR (`ReasoningDelta` bzw. Teil `Reasoning` ohne Signatur); für
//! OpenAI-Clients bleibt das Originalfeld in nicht gestreamten Antworten erhalten.

use aigw_core::ir::{ChatResponse, Content, Part, PartKind};
use serde_json::Value;

/// Standard-Endpunkt von Ollama (OpenAI-kompatible API).
pub const OLLAMA_DEFAULT_ENDPOINT: &str = "http://localhost:11434/v1";

/// Übernimmt `reasoning_content`/`reasoning` einer nicht gestreamten Antwort als
/// Reasoning-Teil (vor dem Text). Das Originalfeld bleibt in `message.extra`, damit
/// OpenAI-Clients es unverändert zurückbekommen.
pub(crate) fn lift_reasoning(resp: &mut ChatResponse) {
    for c in &mut resp.choices {
        let msg = &mut c.message;
        let text =
            ["reasoning_content", "reasoning"]
                .iter()
                .find_map(|k| match msg.extra.get(*k) {
                    Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
                    _ => None,
                });
        let Some(text) = text else { continue };
        let mut parts = std::mem::take(&mut msg.content).into_parts();
        parts.retain(|p| !matches!(&p.kind, PartKind::Text { text } if text.is_empty()));
        parts.insert(
            0,
            Part::new(PartKind::Reasoning {
                text,
                signature: None,
            }),
        );
        msg.content = Content::Parts(parts);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use aigw_core::wire::openai;
    use serde_json::json;

    use super::*;

    #[test]
    fn lifts_reasoning_content() {
        let mut r = openai::decode_response(
            json!({"id": "c", "object": "chat.completion", "created": 1, "model": "qwen3",
                   "choices": [{"index": 0, "finish_reason": "stop",
                                "message": {"role": "assistant", "content": "42",
                                            "reasoning_content": "nachdenken"}}]})
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        lift_reasoning(&mut r);
        let Content::Parts(parts) = &r.choices[0].message.content else {
            panic!()
        };
        assert!(matches!(&parts[0].kind, PartKind::Reasoning { text, .. } if text == "nachdenken"));
        assert_eq!(r.choices[0].message.content.text(), "42");
        assert!(r.choices[0].message.extra.contains_key("reasoning_content"));
    }
}
