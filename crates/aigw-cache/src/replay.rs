// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Streams und Cache: gestreamte Antworten aufzeichnen ([`StreamCollector`]) und gecachte
//! Antworten als Stream wiedergeben ([`chunk_events`]).
//!
//! Gespeichert wird immer die vollständige IR-Antwort ([`ChatResponse`]); ob der Client
//! gestreamt oder nicht gestreamt fragt, entscheidet erst die Wiedergabe. Für Streams zerlegt
//! [`chunk_events`] die Text-, Reasoning- und Argument-Deltas in kleine Stücke (simuliertes
//! Chunking, an Zeichengrenzen), damit Clients mit inkrementeller Anzeige sich wie beim
//! Upstream verhalten.

use std::collections::BTreeMap;

use aigw_core::ir::{
    ChatResponse, Choice, Content, Message, Part, PartKind, Role, StopReason, StreamEvent, Usage,
};

/// Zerlegt Deltas in Stücke von höchstens `chars` Zeichen (0 = nicht zerlegen).
pub fn chunk_events(events: Vec<StreamEvent>, chars: usize) -> Vec<StreamEvent> {
    if chars == 0 {
        return events;
    }
    let mut out = Vec::with_capacity(events.len() * 4);
    for ev in events {
        match ev {
            StreamEvent::TextDelta { index, text } => {
                for t in split(&text, chars) {
                    out.push(StreamEvent::TextDelta {
                        index,
                        text: t.to_owned(),
                    });
                }
            }
            StreamEvent::ReasoningDelta { index, text } => {
                for t in split(&text, chars) {
                    out.push(StreamEvent::ReasoningDelta {
                        index,
                        text: t.to_owned(),
                    });
                }
            }
            StreamEvent::ToolCallDelta { index, arguments } => {
                for t in split(&arguments, chars) {
                    out.push(StreamEvent::ToolCallDelta {
                        index,
                        arguments: t.to_owned(),
                    });
                }
            }
            other => out.push(other),
        }
    }
    out
}

fn split(s: &str, chars: usize) -> Vec<&str> {
    if s.is_empty() {
        return vec![s];
    }
    let mut out = Vec::new();
    let mut start = 0;
    let mut count = 0;
    for (i, _) in s.char_indices() {
        if count == chars {
            out.push(&s[start..i]);
            start = i;
            count = 0;
        }
        count += 1;
    }
    out.push(&s[start..]);
    out
}

#[derive(Debug)]
enum Block {
    Text(String),
    Reasoning(String, Option<String>),
    Redacted(String),
    Tool {
        id: String,
        name: String,
        args: String,
    },
}

/// Setzt IR-Stream-Ereignisse wieder zu einer vollständigen Antwort zusammen.
#[derive(Debug)]
pub struct StreamCollector {
    id: String,
    model: String,
    created: Option<i64>,
    blocks: BTreeMap<u32, Block>,
    stop: Option<(StopReason, Option<String>)>,
    usage: Option<Usage>,
    bytes: usize,
    limit: usize,
    failed: bool,
    overflow: bool,
}

impl StreamCollector {
    /// Neuer Sammler; Antworten über `limit` Bytes Text werden verworfen.
    pub fn new(limit: usize) -> Self {
        Self {
            id: String::new(),
            model: String::new(),
            created: None,
            blocks: BTreeMap::new(),
            stop: None,
            usage: None,
            bytes: 0,
            limit,
            failed: false,
            overflow: false,
        }
    }

    /// Größe überschritten?
    pub fn overflowed(&self) -> bool {
        self.overflow
    }

    /// Fehlerereignis gesehen?
    pub fn failed(&self) -> bool {
        self.failed
    }

    fn grow(&mut self, n: usize) -> bool {
        self.bytes += n;
        if self.bytes > self.limit {
            self.overflow = true;
            self.blocks.clear();
        }
        !self.overflow
    }

    /// Nimmt ein Ereignis auf.
    pub fn push(&mut self, ev: &StreamEvent) {
        if self.failed || self.overflow {
            return;
        }
        match ev {
            StreamEvent::Start { id, model, created } => {
                self.id.clone_from(id);
                self.model.clone_from(model);
                self.created = *created;
            }
            StreamEvent::TextDelta { index, text } => {
                if self.grow(text.len())
                    && let Block::Text(t) = self
                        .blocks
                        .entry(*index)
                        .or_insert_with(|| Block::Text(String::new()))
                {
                    t.push_str(text);
                }
            }
            StreamEvent::ReasoningDelta { index, text } => {
                if self.grow(text.len())
                    && let Block::Reasoning(t, _) = self
                        .blocks
                        .entry(*index)
                        .or_insert_with(|| Block::Reasoning(String::new(), None))
                {
                    t.push_str(text);
                }
            }
            StreamEvent::ReasoningSignature { index, signature } => {
                if self.grow(signature.len())
                    && let Block::Reasoning(_, s) = self
                        .blocks
                        .entry(*index)
                        .or_insert_with(|| Block::Reasoning(String::new(), None))
                {
                    *s = Some(signature.clone());
                }
            }
            StreamEvent::RedactedReasoning { index, data } => {
                if self.grow(data.len()) {
                    self.blocks.insert(*index, Block::Redacted(data.clone()));
                }
            }
            StreamEvent::ToolCallStart(t) => {
                if self.grow(t.id.len() + t.name.len()) {
                    self.blocks.insert(
                        t.index,
                        Block::Tool {
                            id: t.id.clone(),
                            name: t.name.clone(),
                            args: String::new(),
                        },
                    );
                }
            }
            StreamEvent::ToolCallDelta { index, arguments } => {
                if self.grow(arguments.len())
                    && let Some(Block::Tool { args, .. }) = self.blocks.get_mut(index)
                {
                    args.push_str(arguments);
                }
            }
            StreamEvent::Stop {
                reason,
                stop_sequence,
            } => self.stop = Some((reason.clone(), stop_sequence.clone())),
            StreamEvent::Usage(u) => self.usage = Some(u.clone()),
            StreamEvent::Error(_) => self.failed = true,
        }
    }

    /// Vollständige Antwort, falls der Stream regulär (mit Endgrund, ohne Fehler, innerhalb
    /// der Größe) endete.
    pub fn finish(self) -> Option<ChatResponse> {
        if self.failed || self.overflow {
            return None;
        }
        let (reason, stop_sequence) = self.stop?;
        let mut parts = Vec::with_capacity(self.blocks.len());
        for (_, b) in self.blocks {
            parts.push(Part::new(match b {
                Block::Text(text) => PartKind::Text { text },
                Block::Reasoning(text, signature) => PartKind::Reasoning { text, signature },
                Block::Redacted(data) => PartKind::RedactedReasoning { data },
                Block::Tool { id, name, args } => PartKind::ToolCall {
                    id,
                    name,
                    arguments: args,
                },
            }));
        }
        let single_text =
            parts.len() == 1 && matches!(parts[0].kind, PartKind::Text { .. });
        let content = if single_text {
            match parts.pop().map(|p| p.kind) {
                Some(PartKind::Text { text }) => Content::Text(text),
                _ => Content::Parts(Vec::new()),
            }
        } else {
            Content::Parts(parts)
        };
        Some(ChatResponse {
            id: self.id,
            model: self.model,
            created: self.created,
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    content,
                    name: None,
                    extra: Default::default(),
                },
                stop_reason: Some(reason),
                stop_sequence,
                extra: Default::default(),
            }],
            usage: self.usage,
            extra: Default::default(),
            source: None,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use aigw_core::ir::{ApiError, ToolCallStart};

    use super::*;

    #[test]
    fn split_respects_char_boundaries() {
        assert_eq!(split("äöüß", 3), vec!["äöü", "ß"]);
        assert_eq!(split("abc", 3), vec!["abc"]);
        assert_eq!(split("", 3), vec![""]);
        let ev = chunk_events(
            vec![StreamEvent::TextDelta {
                index: 0,
                text: "Hallo Welt".into(),
            }],
            4,
        );
        assert_eq!(ev.len(), 3);
    }

    #[test]
    fn collector_rebuilds_text_and_tool_calls() {
        let mut c = StreamCollector::new(1 << 20);
        for ev in [
            StreamEvent::Start {
                id: "r1".into(),
                model: "m".into(),
                created: Some(1),
            },
            StreamEvent::TextDelta {
                index: 0,
                text: "Hal".into(),
            },
            StreamEvent::TextDelta {
                index: 0,
                text: "lo".into(),
            },
            StreamEvent::ToolCallStart(ToolCallStart {
                index: 1,
                id: "c1".into(),
                name: "f".into(),
            }),
            StreamEvent::ToolCallDelta {
                index: 1,
                arguments: "{\"a\":".into(),
            },
            StreamEvent::ToolCallDelta {
                index: 1,
                arguments: "1}".into(),
            },
            StreamEvent::Stop {
                reason: StopReason::ToolUse,
                stop_sequence: None,
            },
            StreamEvent::Usage(Usage {
                input_tokens: 3,
                output_tokens: 4,
                ..Usage::default()
            }),
        ] {
            c.push(&ev);
        }
        let r = c.finish().unwrap();
        assert_eq!(r.id, "r1");
        assert_eq!(r.usage.as_ref().unwrap().output_tokens, 4);
        let p = match &r.choices[0].message.content {
            Content::Parts(p) => p.clone(),
            Content::Text(_) => Vec::new(),
        };
        assert_eq!(p.len(), 2);
        assert!(matches!(&p[1].kind, PartKind::ToolCall { arguments, .. } if arguments == "{\"a\":1}"));
    }

    #[test]
    fn collector_rejects_errors_missing_stop_and_overflow() {
        let mut c = StreamCollector::new(100);
        c.push(&StreamEvent::TextDelta {
            index: 0,
            text: "x".into(),
        });
        assert!(c.finish().is_none()); // kein Endgrund
        let mut c = StreamCollector::new(100);
        c.push(&StreamEvent::Error(ApiError::invalid_request("x")));
        c.push(&StreamEvent::Stop {
            reason: StopReason::EndTurn,
            stop_sequence: None,
        });
        assert!(c.finish().is_none());
        let mut c = StreamCollector::new(3);
        c.push(&StreamEvent::TextDelta {
            index: 0,
            text: "abcd".into(),
        });
        c.push(&StreamEvent::Stop {
            reason: StopReason::EndTurn,
            stop_sequence: None,
        });
        assert!(c.overflowed());
        assert!(c.finish().is_none());
        // Reiner Text → Content::Text.
        let mut c = StreamCollector::new(100);
        c.push(&StreamEvent::TextDelta {
            index: 0,
            text: "ok".into(),
        });
        c.push(&StreamEvent::Stop {
            reason: StopReason::EndTurn,
            stop_sequence: None,
        });
        assert_eq!(
            c.finish().unwrap().choices[0].message.content,
            Content::Text("ok".into())
        );
    }
}
