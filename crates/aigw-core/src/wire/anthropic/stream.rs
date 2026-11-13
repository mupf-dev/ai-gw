// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Anthropic-Streaming: IR-Ereignisse → benannte SSE-Ereignisse und zurück.
//!
//! Ausgabe: `message_start`, je Block `content_block_start` / `content_block_delta`* /
//! `content_block_stop`, dann `message_delta` (Grund + Usage) und `message_stop`.

use serde_json::{Map, Value, json};

use super::convert::{
    encode_error, stop_reason_from_wire, stop_reason_to_wire, usage_from_wire, usage_to_wire,
};
use super::{Event, MessageDelta, MessagesResponse, Usage as WireUsage};
use crate::ir::{ApiError, ErrorKind, StopReason, StreamEvent, ToolCallStart, Usage};
use crate::wire::{Losses, sse};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking,
    ToolUse,
}

/// Zustandsbehafteter Encoder IR-Ereignisse → Anthropic-SSE.
#[derive(Debug)]
pub struct StreamEncoder {
    id: String,
    model: String,
    pending_start: bool,
    started: bool,
    /// Offener Block: (IR-Index, Ausgabe-Index, Art).
    open: Option<(u32, u32, BlockKind)>,
    next_out: u32,
    stop: Option<(StopReason, Option<String>)>,
    usage: Option<Usage>,
    finished: bool,
}

impl Default for StreamEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamEncoder {
    /// Neuer Encoder.
    pub fn new() -> Self {
        Self {
            id: String::new(),
            model: String::new(),
            pending_start: false,
            started: false,
            open: None,
            next_out: 0,
            stop: None,
            usage: None,
            finished: false,
        }
    }

    fn write(out: &mut Vec<u8>, ev: &Event) {
        if let Ok(json) = serde_json::to_vec(ev) {
            sse::write_event(out, ev.name(), &json);
        }
    }

    fn flush_start(&mut self, out: &mut Vec<u8>) {
        if self.started {
            return;
        }
        self.started = true;
        self.pending_start = false;
        let mut losses = Losses::new();
        let usage = self
            .usage
            .clone()
            .map(|u| usage_to_wire(u, false, &mut losses))
            .unwrap_or_default();
        let message = MessagesResponse {
            id: self.id.clone(),
            kind: "message".to_owned(),
            role: "assistant".to_owned(),
            model: self.model.clone(),
            content: Vec::new(),
            stop_reason: None,
            stop_sequence: None,
            usage,
            extra: Map::new(),
        };
        Self::write(out, &Event::MessageStart { message });
    }

    fn close_block(&mut self, out: &mut Vec<u8>) {
        if let Some((_, idx, _)) = self.open.take() {
            Self::write(out, &Event::ContentBlockStop { index: idx });
        }
    }

    /// Stellt sicher, dass der IR-Block `index` als `kind` offen ist; liefert den Ausgabe-Index.
    fn ensure_block(
        &mut self,
        index: u32,
        kind: BlockKind,
        start: impl FnOnce() -> Map<String, Value>,
        out: &mut Vec<u8>,
    ) -> u32 {
        self.flush_start(out);
        if let Some((ir, idx, k)) = self.open
            && ir == index
            && k == kind
        {
            return idx;
        }
        self.close_block(out);
        let idx = self.next_out;
        self.next_out += 1;
        self.open = Some((index, idx, kind));
        Self::write(
            out,
            &Event::ContentBlockStart {
                index: idx,
                content_block: start(),
            },
        );
        idx
    }

    fn delta(out: &mut Vec<u8>, index: u32, delta: Value) {
        if let Value::Object(delta) = delta {
            Self::write(out, &Event::ContentBlockDelta { index, delta });
        }
    }

    /// Kodiert ein Ereignis und hängt die SSE-Frames an `out` an.
    pub fn encode(&mut self, ev: StreamEvent, out: &mut Vec<u8>) {
        if self.finished {
            return;
        }
        match ev {
            StreamEvent::Start { id, model, .. } => {
                self.id = id;
                self.model = model;
                // `message_start` erst beim nächsten Ereignis, damit eine direkt folgende
                // Usage (Eingabe-Tokens) noch hineinpasst.
                self.pending_start = true;
            }
            StreamEvent::TextDelta { index, text } => {
                let idx = self.ensure_block(
                    index,
                    BlockKind::Text,
                    || obj(json!({"type": "text", "text": ""})),
                    out,
                );
                Self::delta(out, idx, json!({"type": "text_delta", "text": text}));
            }
            StreamEvent::ReasoningDelta { index, text } => {
                let idx = self.ensure_block(
                    index,
                    BlockKind::Thinking,
                    || obj(json!({"type": "thinking", "thinking": ""})),
                    out,
                );
                Self::delta(
                    out,
                    idx,
                    json!({"type": "thinking_delta", "thinking": text}),
                );
            }
            StreamEvent::ReasoningSignature { index, signature } => {
                let idx = self.ensure_block(
                    index,
                    BlockKind::Thinking,
                    || obj(json!({"type": "thinking", "thinking": ""})),
                    out,
                );
                Self::delta(
                    out,
                    idx,
                    json!({"type": "signature_delta", "signature": signature}),
                );
            }
            StreamEvent::RedactedReasoning { data, .. } => {
                self.flush_start(out);
                self.close_block(out);
                let idx = self.next_out;
                self.next_out += 1;
                Self::write(
                    out,
                    &Event::ContentBlockStart {
                        index: idx,
                        content_block: obj(json!({"type": "redacted_thinking", "data": data})),
                    },
                );
                Self::write(out, &Event::ContentBlockStop { index: idx });
            }
            StreamEvent::ToolCallStart(ToolCallStart { index, id, name }) => {
                self.flush_start(out);
                self.close_block(out);
                self.ensure_block(
                    index,
                    BlockKind::ToolUse,
                    || obj(json!({"type": "tool_use", "id": id, "name": name, "input": {}})),
                    out,
                );
            }
            StreamEvent::ToolCallDelta { index, arguments } => {
                let idx = self.ensure_block(
                    index,
                    BlockKind::ToolUse,
                    || obj(json!({"type": "tool_use", "id": "", "name": "", "input": {}})),
                    out,
                );
                Self::delta(
                    out,
                    idx,
                    json!({"type": "input_json_delta", "partial_json": arguments}),
                );
            }
            StreamEvent::Stop {
                reason,
                stop_sequence,
            } => {
                self.stop = Some((reason, stop_sequence));
            }
            StreamEvent::Usage(u) => {
                self.usage = Some(u);
                if self.pending_start {
                    self.flush_start(out);
                }
            }
            StreamEvent::Error(e) => {
                self.flush_start(out);
                write_error(out, &e);
                self.finished = true;
            }
            // Anthropic kennt nur eine Variante (`n` gibt es nur bei OpenAI-Clients).
            StreamEvent::Choice { .. } => {}
        }
    }

    /// Schließt den Stream ab (`content_block_stop`, `message_delta`, `message_stop`).
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        if self.finished {
            return;
        }
        self.flush_start(out);
        self.close_block(out);
        let (reason, seq) = match self.stop.take() {
            Some((r, s)) => (Some(stop_reason_to_wire(&r)), s),
            None => (None, None),
        };
        let mut losses = Losses::new();
        let usage = self
            .usage
            .take()
            .map(|u| usage_to_wire(u, false, &mut losses));
        Self::write(
            out,
            &Event::MessageDelta {
                delta: MessageDelta {
                    stop_reason: reason,
                    stop_sequence: seq,
                },
                usage: Some(usage.unwrap_or_default()),
            },
        );
        Self::write(out, &Event::MessageStop);
        self.finished = true;
    }
}

fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// Schreibt ein `error`-Ereignis.
pub fn write_error(out: &mut Vec<u8>, e: &ApiError) {
    let body = encode_error(e, None);
    if let Ok(json) = serde_json::to_vec(&body) {
        sse::write_event(out, "error", &json);
    }
}

/// Zustandsbehafteter Decoder Anthropic-SSE → IR-Ereignisse.
#[derive(Debug, Default)]
pub struct StreamDecoder {
    usage: Option<Usage>,
    done: bool,
}

impl StreamDecoder {
    /// Neuer Decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// `true`, nachdem `message_stop` gelesen wurde.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Verarbeitet ein SSE-Ereignis (`event`-Name optional, maßgeblich ist `type` im JSON).
    pub fn decode(&mut self, data: &str) -> Result<Vec<StreamEvent>, serde_json::Error> {
        let mut out = Vec::new();
        let raw: Map<String, Value> = serde_json::from_str(data)?;
        let known = [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
            "ping",
            "error",
        ];
        if !raw
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| known.contains(&t))
        {
            return Ok(out);
        }
        let ev: Event = serde_json::from_value(Value::Object(raw))?;
        match ev {
            Event::MessageStart { message } => {
                out.push(StreamEvent::Start {
                    id: message.id,
                    model: message.model,
                    created: None,
                });
                let u = usage_from_wire(message.usage);
                self.usage = Some(u.clone());
                out.push(StreamEvent::Usage(u));
            }
            Event::ContentBlockStart {
                index,
                mut content_block,
            } => match content_block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(Value::String(t)) = content_block.remove("text")
                        && !t.is_empty()
                    {
                        out.push(StreamEvent::TextDelta { index, text: t });
                    }
                }
                Some("thinking") => {
                    if let Some(Value::String(t)) = content_block.remove("thinking")
                        && !t.is_empty()
                    {
                        out.push(StreamEvent::ReasoningDelta { index, text: t });
                    }
                }
                Some("redacted_thinking") => {
                    if let Some(Value::String(d)) = content_block.remove("data") {
                        out.push(StreamEvent::RedactedReasoning { index, data: d });
                    }
                }
                Some("tool_use") => {
                    let s = |k: &str| {
                        content_block
                            .get(k)
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned()
                    };
                    out.push(StreamEvent::ToolCallStart(ToolCallStart {
                        index,
                        id: s("id"),
                        name: s("name"),
                    }));
                    if let Some(Value::Object(input)) = content_block.get("input")
                        && !input.is_empty()
                    {
                        out.push(StreamEvent::ToolCallDelta {
                            index,
                            arguments: Value::Object(input.clone()).to_string(),
                        });
                    }
                }
                _ => {}
            },
            Event::ContentBlockDelta { index, mut delta } => {
                let mut take = |k: &str| match delta.remove(k) {
                    Some(Value::String(s)) => s,
                    _ => String::new(),
                };
                match take("type").as_str() {
                    "text_delta" => out.push(StreamEvent::TextDelta {
                        index,
                        text: take("text"),
                    }),
                    "thinking_delta" => out.push(StreamEvent::ReasoningDelta {
                        index,
                        text: take("thinking"),
                    }),
                    "signature_delta" => out.push(StreamEvent::ReasoningSignature {
                        index,
                        signature: take("signature"),
                    }),
                    "input_json_delta" => out.push(StreamEvent::ToolCallDelta {
                        index,
                        arguments: take("partial_json"),
                    }),
                    _ => {}
                }
            }
            Event::ContentBlockStop { .. } | Event::Ping => {}
            Event::MessageDelta { delta, usage } => {
                if let Some(r) = delta.stop_reason {
                    out.push(StreamEvent::Stop {
                        reason: stop_reason_from_wire(&r),
                        stop_sequence: delta.stop_sequence,
                    });
                }
                if let Some(u) = usage {
                    let merged = merge_usage(self.usage.take(), u);
                    self.usage = Some(merged.clone());
                    out.push(StreamEvent::Usage(merged));
                }
            }
            Event::MessageStop => self.done = true,
            Event::Error { error } => {
                let kind = match error.kind.as_str() {
                    "overloaded_error" => ErrorKind::Overloaded,
                    "rate_limit_error" => ErrorKind::RateLimit,
                    "invalid_request_error" => ErrorKind::InvalidRequest,
                    "authentication_error" => ErrorKind::Authentication,
                    "permission_error" => ErrorKind::PermissionDenied,
                    "not_found_error" => ErrorKind::NotFound,
                    "request_too_large" => ErrorKind::RequestTooLarge,
                    "timeout_error" => ErrorKind::Timeout,
                    _ => ErrorKind::Internal,
                };
                out.push(StreamEvent::Error(
                    ApiError::new(kind, error.message).with_code(error.kind),
                ));
                self.done = true;
            }
        }
        Ok(out)
    }
}

/// `message_delta.usage` ist kumulativ, enthält aber nicht immer alle Felder.
fn merge_usage(start: Option<Usage>, delta: WireUsage) -> Usage {
    let start = start.unwrap_or_default();
    let has_input = delta.input_tokens > 0
        || delta.cache_read_input_tokens.is_some()
        || delta.cache_creation_input_tokens.is_some();
    let mut u = usage_from_wire(delta);
    if !has_input {
        u.input_tokens = start.input_tokens;
        u.cache_read_tokens = start.cache_read_tokens;
        u.cache_write_tokens = start.cache_write_tokens;
    }
    let mut extra = start.extra;
    extra.extend(u.extra);
    u.extra = extra;
    u
}
