// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! ConverseStream: Eventstream-Nachrichten → IR-Ereignisse.
//!
//! | Eventstream (`:event-type`) | IR |
//! |---|---|
//! | `messageStart` | `Start` (ID wird erzeugt, Bedrock liefert keine) |
//! | `contentBlockStart` mit `start.toolUse` | `ToolCallStart` |
//! | `contentBlockDelta` `delta.text` / `delta.toolUse.input` / `delta.reasoningContent.{text,signature,redactedContent}` | `TextDelta` / `ToolCallDelta` / `ReasoningDelta`, `ReasoningSignature`, `RedactedReasoning` |
//! | `messageStop` | `Stop` |
//! | `metadata` | `Usage` (Ende des Streams) |
//! | `:message-type` `exception` (`throttlingException`, `modelStreamErrorException`, `validationException` …) | `Error` |

use aigw_core::ir::{ApiError, ErrorClass, ErrorKind, StreamEvent, ToolCallStart};
use serde_json::Value;

use super::eventstream::{Decoder, Message};
use super::{class_of, stop_reason, usage};
use crate::StreamDecoder;
use crate::codec::{random_id, unix_now};
use crate::errmap::{protocol_error, truncate, truncated_error};

/// Decoder für `application/vnd.amazon.eventstream` (ConverseStream).
#[derive(Debug)]
pub struct BedrockStreamDecoder {
    model: String,
    frames: Decoder,
    messages: Vec<Message>,
    started: bool,
    stopped: bool,
    done: bool,
}

impl BedrockStreamDecoder {
    /// Neuer Decoder; `model` erscheint im `Start`-Ereignis.
    pub fn new(model: String) -> Self {
        Self {
            model,
            frames: Decoder::new(),
            messages: Vec::new(),
            started: false,
            stopped: false,
            done: false,
        }
    }

    fn start(&mut self, out: &mut Vec<StreamEvent>) {
        if !self.started {
            self.started = true;
            out.push(StreamEvent::Start {
                id: random_id("msg_bdrk_"),
                model: self.model.clone(),
                created: i64::try_from(unix_now()).ok(),
            });
        }
    }

    fn fail(&mut self, err: ApiError, out: &mut Vec<StreamEvent>) {
        out.push(StreamEvent::Error(err));
        self.done = true;
    }

    fn handle(&mut self, msg: &Message, out: &mut Vec<StreamEvent>) {
        match msg.header_str(":message-type").unwrap_or("event") {
            "event" => {}
            "exception" => {
                let ty = msg.header_str(":exception-type").unwrap_or("exception");
                let message = serde_json::from_slice::<Value>(&msg.payload)
                    .ok()
                    .and_then(|v| {
                        v.get("message")
                            .or_else(|| v.get("Message"))
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| String::from_utf8_lossy(&msg.payload).into_owned());
                let err = stream_exception(ty, message);
                self.fail(err, out);
                return;
            }
            _ => {
                let code = msg.header_str(":error-code").unwrap_or("error");
                let message = msg.header_str(":error-message").unwrap_or("Stream-Fehler");
                self.fail(stream_exception(code, message.to_owned()), out);
                return;
            }
        }
        let event_type = msg.header_str(":event-type").unwrap_or_default();
        let v: Value = match serde_json::from_slice(&msg.payload) {
            Ok(v) => v,
            Err(e) => {
                self.fail(protocol_error(format!("{event_type}: {e}")), out);
                return;
            }
        };
        let index = || {
            v.get("contentBlockIndex")
                .and_then(Value::as_u64)
                .and_then(|i| u32::try_from(i).ok())
                .unwrap_or(0)
        };
        match event_type {
            "messageStart" => self.start(out),
            "contentBlockStart" => {
                self.start(out);
                if let Some(tu) = v.pointer("/start/toolUse") {
                    out.push(StreamEvent::ToolCallStart(ToolCallStart {
                        index: index(),
                        id: tu
                            .get("toolUseId")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        name: tu
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    }));
                }
            }
            "contentBlockDelta" => {
                self.start(out);
                let i = index();
                let Some(delta) = v.get("delta") else { return };
                if let Some(t) = delta.get("text").and_then(Value::as_str) {
                    out.push(StreamEvent::TextDelta {
                        index: i,
                        text: t.to_owned(),
                    });
                } else if let Some(a) = delta.pointer("/toolUse/input").and_then(Value::as_str) {
                    out.push(StreamEvent::ToolCallDelta {
                        index: i,
                        arguments: a.to_owned(),
                    });
                } else if let Some(rc) = delta.get("reasoningContent") {
                    if let Some(t) = rc.get("text").and_then(Value::as_str) {
                        out.push(StreamEvent::ReasoningDelta {
                            index: i,
                            text: t.to_owned(),
                        });
                    }
                    if let Some(s) = rc.get("signature").and_then(Value::as_str) {
                        out.push(StreamEvent::ReasoningSignature {
                            index: i,
                            signature: s.to_owned(),
                        });
                    }
                    if let Some(d) = rc.get("redactedContent").and_then(Value::as_str) {
                        out.push(StreamEvent::RedactedReasoning {
                            index: i,
                            data: d.to_owned(),
                        });
                    }
                }
            }
            "messageStop" => {
                self.start(out);
                self.stopped = true;
                out.push(StreamEvent::Stop {
                    reason: stop_reason(
                        v.get("stopReason")
                            .and_then(Value::as_str)
                            .unwrap_or("end_turn"),
                    ),
                    stop_sequence: None,
                });
            }
            "metadata" => {
                if let Some(u) = v.get("usage") {
                    out.push(StreamEvent::Usage(usage(u)));
                }
                if self.stopped {
                    self.done = true;
                }
            }
            // contentBlockStop und unbekannte (neue) Ereignisse ignorieren.
            _ => {}
        }
    }
}

/// Ausnahme mitten im Stream → `ApiError` (Klasse wie bei HTTP-Fehlern).
fn stream_exception(ty: &str, message: String) -> ApiError {
    let class = class_of(ty, &message, 500);
    let kind = match class {
        ErrorClass::RateLimit | ErrorClass::Quota => ErrorKind::RateLimit,
        ErrorClass::InvalidRequest | ErrorClass::ContextLength => ErrorKind::InvalidRequest,
        ErrorClass::Overloaded => ErrorKind::Overloaded,
        ErrorClass::Timeout => ErrorKind::Timeout,
        _ => ErrorKind::BadGateway,
    };
    let code = if class == ErrorClass::ContextLength {
        "context_length_exceeded".to_owned()
    } else {
        ty.to_owned()
    };
    let message = if message.trim().is_empty() {
        format!("Anbieter meldet {ty} im Stream")
    } else {
        truncate(message)
    };
    ApiError::new(kind, message).with_code(code)
}

impl StreamDecoder for BedrockStreamDecoder {
    fn push(&mut self, chunk: &[u8], out: &mut Vec<StreamEvent>) {
        if self.done {
            return;
        }
        let mut msgs = std::mem::take(&mut self.messages);
        if let Err(e) = self.frames.push(chunk, &mut msgs) {
            msgs.clear();
            self.messages = msgs;
            self.fail(protocol_error(e), out);
            return;
        }
        for m in msgs.drain(..) {
            if self.done {
                break;
            }
            self.handle(&m, out);
        }
        self.messages = msgs;
    }

    fn finish(&mut self, out: &mut Vec<StreamEvent>) {
        if self.done {
            return;
        }
        self.done = true;
        // Ohne `metadata` (Usage) ist ein Stream mit `messageStop` dennoch vollständig.
        if !self.stopped {
            out.push(StreamEvent::Error(truncated_error()));
        }
    }

    fn is_done(&self) -> bool {
        self.done
    }
}
