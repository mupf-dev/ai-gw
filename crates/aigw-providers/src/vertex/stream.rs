// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! `streamGenerateContent?alt=sse` → IR-Ereignisse.
//!
//! Jedes SSE-Ereignis enthält eine `GenerateContentResponse` mit den *neuen* Teilen des ersten
//! Kandidaten. Aufeinanderfolgende Text- bzw. Gedanken-Teile bilden je einen IR-Block;
//! `functionCall`-Teile kommen vollständig und werden zu `ToolCallStart` + einem
//! `ToolCallDelta`. Ein Chunk mit `finishReason` beendet den Stream (`Stop`, danach `Usage`
//! aus `usageMetadata`). Blockaden: `promptFeedback.blockReason` bzw. ein blockierender
//! Endgrund vor jedem Inhalt → `Error` mit Code `content_filter`.

use aigw_core::ir::{ApiError, ErrorKind, StreamEvent, ToolCallStart};
use aigw_core::wire::sse::{SseEvent, SseParser};
use serde_json::Value;

use super::{content_filter_error, finish_reason, is_blocked, prompt_block, tool_call_id, usage};
use crate::StreamDecoder;
use crate::codec::{random_id, unix_now};
use crate::errmap::{protocol_error, truncate, truncated_error};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    Text(u32),
    Thought(u32),
}

/// Decoder für Gemini-SSE.
#[derive(Debug)]
pub struct GeminiStreamDecoder {
    model: String,
    sse: SseParser,
    events: Vec<SseEvent>,
    started: bool,
    open: Option<Block>,
    next_index: u32,
    content_seen: bool,
    tool_calls: bool,
    stopped: bool,
    done: bool,
}

impl GeminiStreamDecoder {
    /// Neuer Decoder; `model` gilt, wenn der Anbieter keine `modelVersion` liefert.
    pub fn new(model: String) -> Self {
        Self {
            model,
            sse: SseParser::new(),
            events: Vec::new(),
            started: false,
            open: None,
            next_index: 0,
            content_seen: false,
            tool_calls: false,
            stopped: false,
            done: false,
        }
    }

    fn fail(&mut self, err: ApiError, out: &mut Vec<StreamEvent>) {
        out.push(StreamEvent::Error(err));
        self.done = true;
    }

    fn new_index(&mut self) -> u32 {
        let i = self.next_index;
        self.next_index += 1;
        i
    }

    fn chunk(&mut self, data: &str, out: &mut Vec<StreamEvent>) {
        let v: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(e) => return self.fail(protocol_error(e), out),
        };
        if let Some(err) = v.get("error") {
            let message = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Anbieter meldet einen Fehler im Stream")
                .to_owned();
            let status = err.get("status").and_then(Value::as_str);
            let kind = match status {
                Some("RESOURCE_EXHAUSTED") => ErrorKind::RateLimit,
                Some("INVALID_ARGUMENT") => ErrorKind::InvalidRequest,
                Some("UNAVAILABLE") => ErrorKind::Overloaded,
                Some("DEADLINE_EXCEEDED") => ErrorKind::Timeout,
                _ => ErrorKind::BadGateway,
            };
            let mut e = ApiError::new(kind, truncate(message));
            if let Some(s) = status {
                e = e.with_code(s);
            }
            return self.fail(e, out);
        }
        if !self.started {
            self.started = true;
            out.push(StreamEvent::Start {
                id: v
                    .get("responseId")
                    .and_then(Value::as_str)
                    .map_or_else(|| random_id("gemini_"), str::to_owned),
                model: v
                    .get("modelVersion")
                    .and_then(Value::as_str)
                    .unwrap_or(&self.model)
                    .to_owned(),
                created: i64::try_from(unix_now()).ok(),
            });
        }
        if let Some(r) = prompt_block(&v) {
            return self.fail(content_filter_error(&r), out);
        }
        let cand = v.pointer("/candidates/0");
        if let Some(parts) = cand
            .and_then(|c| c.pointer("/content/parts"))
            .and_then(Value::as_array)
        {
            for p in parts {
                self.part(p, out);
            }
        }
        if let Some(reason) = cand
            .and_then(|c| c.get("finishReason"))
            .and_then(Value::as_str)
            && !self.stopped
        {
            if is_blocked(reason) && !self.content_seen {
                return self.fail(content_filter_error(reason), out);
            }
            self.stopped = true;
            out.push(StreamEvent::Stop {
                reason: finish_reason(reason, self.tool_calls),
                stop_sequence: None,
            });
            if let Some(u) = v.get("usageMetadata") {
                out.push(StreamEvent::Usage(usage(u)));
            }
        } else if self.stopped
            && let Some(u) = v.get("usageMetadata")
        {
            // Usage nach dem Endgrund (kumulativ, der letzte Wert gilt).
            out.push(StreamEvent::Usage(usage(u)));
        }
    }

    fn part(&mut self, p: &Value, out: &mut Vec<StreamEvent>) {
        let sig = p.get("thoughtSignature").and_then(Value::as_str);
        if let Some(fc) = p.get("functionCall") {
            self.open = None;
            self.content_seen = true;
            self.tool_calls = true;
            let index = self.new_index();
            out.push(StreamEvent::ToolCallStart(ToolCallStart {
                index,
                id: tool_call_id(fc.get("id").and_then(Value::as_str), sig),
                name: fc
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            }));
            out.push(StreamEvent::ToolCallDelta {
                index,
                arguments: fc
                    .get("args")
                    .map_or_else(|| "{}".to_owned(), Value::to_string),
            });
            return;
        }
        let Some(text) = p.get("text").and_then(Value::as_str) else {
            return;
        };
        let thought = p.get("thought").and_then(Value::as_bool) == Some(true);
        let index = match (self.open, thought) {
            (Some(Block::Thought(i)), true) | (Some(Block::Text(i)), false) => i,
            _ => {
                let i = self.new_index();
                self.open = Some(if thought {
                    Block::Thought(i)
                } else {
                    Block::Text(i)
                });
                i
            }
        };
        if thought {
            if !text.is_empty() {
                out.push(StreamEvent::ReasoningDelta {
                    index,
                    text: text.to_owned(),
                });
            }
            if let Some(s) = sig {
                out.push(StreamEvent::ReasoningSignature {
                    index,
                    signature: s.to_owned(),
                });
            }
        } else if !text.is_empty() {
            self.content_seen = true;
            out.push(StreamEvent::TextDelta {
                index,
                text: text.to_owned(),
            });
        }
    }
}

impl StreamDecoder for GeminiStreamDecoder {
    fn push(&mut self, chunk: &[u8], out: &mut Vec<StreamEvent>) {
        if self.done {
            return;
        }
        let mut events = std::mem::take(&mut self.events);
        self.sse.push_into(chunk, &mut events);
        for ev in events.drain(..) {
            if self.done {
                break;
            }
            if ev.data.trim().is_empty() || ev.data.trim() == "[DONE]" {
                continue;
            }
            self.chunk(&ev.data, out);
        }
        self.events = events;
    }

    fn finish(&mut self, out: &mut Vec<StreamEvent>) {
        if self.done {
            return;
        }
        self.done = true;
        if !self.stopped {
            out.push(StreamEvent::Error(truncated_error()));
        }
    }

    fn is_done(&self) -> bool {
        self.done
    }
}
