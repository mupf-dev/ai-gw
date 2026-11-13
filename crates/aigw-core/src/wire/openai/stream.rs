// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! OpenAI-Streaming: IR-Ereignisse → `data: {chunk}` / `data: [DONE]` und zurück.

use serde_json::Map;

use super::convert::{
    encode_error, stop_reason_from_wire, stop_reason_to_wire, usage_from_wire, usage_to_wire,
};
use super::{ChatCompletionChunk, ChunkChoice, ChunkDelta, FunctionCallChunk, ToolCallChunk};
use crate::ir::{StreamEvent, ToolCallStart, Usage};
use crate::wire::{Losses, sse};

/// Zustandsbehafteter Encoder IR-Ereignisse → OpenAI-SSE.
#[derive(Debug)]
pub struct StreamEncoder {
    id: String,
    model: String,
    created: i64,
    include_usage: bool,
    usage: Option<Usage>,
    /// Blockindex → Tool-Aufruf-Index.
    tools: Vec<(u32, u32)>,
    started: bool,
    finished: bool,
}

impl StreamEncoder {
    /// Neuer Encoder. `include_usage` entspricht `stream_options.include_usage` des Clients.
    pub fn new(include_usage: bool) -> Self {
        Self {
            id: String::new(),
            model: String::new(),
            created: crate::wire::unix_now(),
            include_usage,
            usage: None,
            tools: Vec::new(),
            started: false,
            finished: false,
        }
    }

    fn chunk(&self, delta: ChunkDelta, finish_reason: Option<String>) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: self.id.clone(),
            object: "chat.completion.chunk".to_owned(),
            created: self.created,
            model: self.model.clone(),
            choices: vec![ChunkChoice {
                index: 0,
                delta,
                finish_reason,
                extra: Map::new(),
            }],
            usage: None,
            extra: Map::new(),
        }
    }

    fn write(out: &mut Vec<u8>, chunk: &ChatCompletionChunk) {
        // Serialisieren von Wire-Typen ohne fremde Map-Schlüssel kann nicht fehlschlagen.
        if let Ok(json) = serde_json::to_vec(chunk) {
            sse::write_data(out, &json);
        }
    }

    fn ensure_started(&mut self, out: &mut Vec<u8>) {
        if !self.started {
            self.started = true;
            let delta = ChunkDelta {
                role: Some("assistant".to_owned()),
                content: Some(String::new()),
                ..ChunkDelta::default()
            };
            let c = self.chunk(delta, None);
            Self::write(out, &c);
        }
    }

    /// Kodiert ein Ereignis und hängt die SSE-Frames an `out` an.
    pub fn encode(&mut self, ev: StreamEvent, out: &mut Vec<u8>) {
        if self.finished {
            return;
        }
        match ev {
            StreamEvent::Start { id, model, created } => {
                self.id = id;
                self.model = model;
                if let Some(c) = created {
                    self.created = c;
                }
                self.ensure_started(out);
            }
            StreamEvent::TextDelta { text, .. } => {
                self.ensure_started(out);
                let c = self.chunk(
                    ChunkDelta {
                        content: Some(text),
                        ..ChunkDelta::default()
                    },
                    None,
                );
                Self::write(out, &c);
            }
            StreamEvent::ReasoningDelta { .. }
            | StreamEvent::ReasoningSignature { .. }
            | StreamEvent::RedactedReasoning { .. } => {
                // Chat Completions kennt kein Reasoning im Stream (siehe Doku 03).
            }
            StreamEvent::ToolCallStart(ToolCallStart { index, id, name }) => {
                self.ensure_started(out);
                let tool_index = u32::try_from(self.tools.len()).unwrap_or(u32::MAX);
                self.tools.push((index, tool_index));
                let c = self.chunk(
                    ChunkDelta {
                        tool_calls: Some(vec![ToolCallChunk {
                            index: tool_index,
                            id: Some(id),
                            kind: Some("function".to_owned()),
                            function: Some(FunctionCallChunk {
                                name: Some(name),
                                arguments: Some(String::new()),
                            }),
                        }]),
                        ..ChunkDelta::default()
                    },
                    None,
                );
                Self::write(out, &c);
            }
            StreamEvent::ToolCallDelta { index, arguments } => {
                self.ensure_started(out);
                let Some(&(_, tool_index)) = self.tools.iter().find(|(b, _)| *b == index) else {
                    return;
                };
                let c = self.chunk(
                    ChunkDelta {
                        tool_calls: Some(vec![ToolCallChunk {
                            index: tool_index,
                            id: None,
                            kind: None,
                            function: Some(FunctionCallChunk {
                                name: None,
                                arguments: Some(arguments),
                            }),
                        }]),
                        ..ChunkDelta::default()
                    },
                    None,
                );
                Self::write(out, &c);
            }
            StreamEvent::Stop { reason, .. } => {
                self.ensure_started(out);
                let c = self.chunk(ChunkDelta::default(), Some(stop_reason_to_wire(&reason)));
                Self::write(out, &c);
            }
            StreamEvent::Usage(u) => self.usage = Some(u),
            StreamEvent::Error(e) => {
                if let Ok(json) = serde_json::to_vec(&encode_error(&e)) {
                    sse::write_data(out, &json);
                }
                self.finished = true;
            }
        }
    }

    /// Schließt den Stream ab (Usage-Chunk falls angefordert, dann `[DONE]`).
    ///
    /// Nach einem Fehlerereignis (oder zweitem Aufruf) wird nichts mehr geschrieben.
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        if self.finished {
            return;
        }
        self.finished = true;
        if self.include_usage
            && let Some(u) = self.usage.take()
        {
            let mut losses = Losses::new();
            let chunk = ChatCompletionChunk {
                id: self.id.clone(),
                object: "chat.completion.chunk".to_owned(),
                created: self.created,
                model: self.model.clone(),
                choices: Vec::new(),
                usage: Some(usage_to_wire(u, false, &mut losses)),
                extra: Map::new(),
            };
            Self::write(out, &chunk);
        }
        sse::write_data(out, b"[DONE]");
    }
}

/// Zustandsbehafteter Decoder OpenAI-SSE-Daten → IR-Ereignisse.
#[derive(Debug, Default)]
pub struct StreamDecoder {
    started: bool,
    next_block: u32,
    /// Offener Textblock.
    text_block: Option<u32>,
    /// Offener Reasoning-Block.
    reasoning_block: Option<u32>,
    /// Tool-Aufruf-Index → Blockindex.
    tools: Vec<(u32, u32)>,
    done: bool,
}

impl StreamDecoder {
    /// Neuer Decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// `true`, nachdem `[DONE]` gelesen wurde.
    pub fn is_done(&self) -> bool {
        self.done
    }

    fn new_block(&mut self) -> u32 {
        let i = self.next_block;
        self.next_block += 1;
        self.text_block = None;
        self.reasoning_block = None;
        i
    }

    /// Verarbeitet den `data`-Inhalt eines SSE-Ereignisses.
    pub fn decode(&mut self, data: &str) -> Result<Vec<StreamEvent>, serde_json::Error> {
        let mut out = Vec::new();
        if data.trim() == "[DONE]" {
            self.done = true;
            return Ok(out);
        }
        let chunk: ChatCompletionChunk = serde_json::from_str(data)?;
        // Azure OpenAI schickt vorab einen Chunk nur mit `prompt_filter_results` (leere ID,
        // keine Varianten); er darf den Stream nicht mit leerer ID/Modell eröffnen.
        if !self.started && chunk.id.is_empty() && chunk.choices.is_empty() && chunk.usage.is_none()
        {
            return Ok(out);
        }
        if !self.started {
            self.started = true;
            out.push(StreamEvent::Start {
                id: chunk.id.clone(),
                model: chunk.model.clone(),
                created: Some(chunk.created),
            });
        }
        for choice in chunk.choices.into_iter().filter(|c| c.index == 0) {
            let mut delta = choice.delta;
            let reasoning = delta
                .extra
                .remove("reasoning_content")
                .or_else(|| delta.extra.remove("reasoning"));
            if let Some(serde_json::Value::String(r)) = reasoning
                && !r.is_empty()
            {
                let index = match self.reasoning_block {
                    Some(i) => i,
                    None => {
                        let i = self.new_block();
                        self.reasoning_block = Some(i);
                        i
                    }
                };
                out.push(StreamEvent::ReasoningDelta { index, text: r });
            }
            if let Some(text) = delta.content.filter(|t| !t.is_empty()) {
                let index = match self.text_block {
                    Some(i) => i,
                    None => {
                        let i = self.new_block();
                        self.text_block = Some(i);
                        i
                    }
                };
                out.push(StreamEvent::TextDelta { index, text });
            }
            for tc in delta.tool_calls.unwrap_or_default() {
                let known = self.tools.iter().find(|(t, _)| *t == tc.index).map(|x| x.1);
                let (name, args) = match tc.function {
                    Some(f) => (f.name, f.arguments),
                    None => (None, None),
                };
                let index = match (known, tc.id) {
                    (Some(b), _) => b,
                    (None, id) => {
                        let b = self.new_block();
                        self.tools.push((tc.index, b));
                        out.push(StreamEvent::ToolCallStart(ToolCallStart {
                            index: b,
                            id: id.unwrap_or_default(),
                            name: name.unwrap_or_default(),
                        }));
                        b
                    }
                };
                if let Some(a) = args.filter(|a| !a.is_empty()) {
                    out.push(StreamEvent::ToolCallDelta {
                        index,
                        arguments: a,
                    });
                }
            }
            if let Some(r) = choice.finish_reason {
                out.push(StreamEvent::Stop {
                    reason: stop_reason_from_wire(&r),
                    stop_sequence: None,
                });
            }
        }
        if let Some(u) = chunk.usage {
            out.push(StreamEvent::Usage(usage_from_wire(u)));
        }
        Ok(out)
    }
}
