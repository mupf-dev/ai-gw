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

/// Zustand einer Antwortvariante im Encoder.
#[derive(Debug, Default)]
struct EncChoice {
    /// Blockindex → Tool-Aufruf-Index.
    tools: Vec<(u32, u32)>,
    /// Erster Chunk (mit `role`) geschrieben.
    started: bool,
}

/// Zustandsbehafteter Encoder IR-Ereignisse → OpenAI-SSE.
///
/// Reasoning geht als `delta.reasoning_content` an den Client (F10, abschaltbar mit
/// [`StreamEncoder::with_reasoning`]). Mehrere Antwortvarianten ([`StreamEvent::Choice`])
/// erscheinen als Chunks mit dem jeweiligen `index` (F12).
#[derive(Debug)]
pub struct StreamEncoder {
    id: String,
    model: String,
    created: i64,
    include_usage: bool,
    reasoning: bool,
    usage: Option<Usage>,
    /// Aktuelle Variante.
    choice: u32,
    /// Zustand je Variante (Index = Variante).
    choices: Vec<EncChoice>,
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
            reasoning: true,
            usage: None,
            choice: 0,
            choices: Vec::new(),
            finished: false,
        }
    }

    /// Reasoning als `reasoning_content` ausgeben (Standard) oder verwerfen.
    #[must_use]
    pub fn with_reasoning(mut self, expose: bool) -> Self {
        self.reasoning = expose;
        self
    }

    fn state(&mut self) -> &mut EncChoice {
        let i = usize::try_from(self.choice).unwrap_or(0);
        if self.choices.len() <= i {
            self.choices.resize_with(i + 1, EncChoice::default);
        }
        &mut self.choices[i]
    }

    fn chunk(&self, delta: ChunkDelta, finish_reason: Option<String>) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: self.id.clone(),
            object: "chat.completion.chunk".to_owned(),
            created: self.created,
            model: self.model.clone(),
            choices: vec![ChunkChoice {
                index: self.choice,
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

    /// Erster Chunk der aktuellen Variante (`role: assistant`).
    fn ensure_started(&mut self, out: &mut Vec<u8>) {
        let st = self.state();
        if !st.started {
            st.started = true;
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
            StreamEvent::Choice { index } => self.choice = index,
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
            StreamEvent::ReasoningDelta { text, .. } => {
                if !self.reasoning || text.is_empty() {
                    return;
                }
                self.ensure_started(out);
                let mut extra = Map::new();
                extra.insert("reasoning_content".into(), serde_json::Value::String(text));
                let c = self.chunk(
                    ChunkDelta {
                        extra,
                        ..ChunkDelta::default()
                    },
                    None,
                );
                Self::write(out, &c);
            }
            StreamEvent::ReasoningSignature { .. } | StreamEvent::RedactedReasoning { .. } => {
                // Signaturen/verschlüsseltes Reasoning: in Chat Completions ohne Entsprechung.
            }
            StreamEvent::ToolCallStart(ToolCallStart { index, id, name }) => {
                self.ensure_started(out);
                let st = self.state();
                let tool_index = u32::try_from(st.tools.len()).unwrap_or(u32::MAX);
                st.tools.push((index, tool_index));
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
                let Some(&(_, tool_index)) = self.state().tools.iter().find(|(b, _)| *b == index)
                else {
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

/// Zustand einer Antwortvariante im Decoder.
#[derive(Debug, Default)]
struct DecChoice {
    next_block: u32,
    /// Offener Textblock.
    text_block: Option<u32>,
    /// Offener Reasoning-Block.
    reasoning_block: Option<u32>,
    /// Tool-Aufruf-Index → Blockindex.
    tools: Vec<(u32, u32)>,
}

impl DecChoice {
    fn new_block(&mut self) -> u32 {
        let i = self.next_block;
        self.next_block += 1;
        self.text_block = None;
        self.reasoning_block = None;
        i
    }
}

/// Zustandsbehafteter Decoder OpenAI-SSE-Daten → IR-Ereignisse.
///
/// Varianten mit `index > 0` (`n > 1`) werden mit [`StreamEvent::Choice`] markiert.
#[derive(Debug, Default)]
pub struct StreamDecoder {
    started: bool,
    /// Zustand je Variante (Index = Variante).
    choices: Vec<DecChoice>,
    /// Variante der zuletzt ausgegebenen Ereignisse.
    current: u32,
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

    fn choice(&mut self, index: u32) -> &mut DecChoice {
        let i = usize::try_from(index).unwrap_or(0);
        if self.choices.len() <= i {
            self.choices.resize_with(i + 1, DecChoice::default);
        }
        &mut self.choices[i]
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
        for choice in chunk.choices {
            let ci = choice.index;
            let mut delta = choice.delta;
            let reasoning = delta
                .extra
                .remove("reasoning_content")
                .or_else(|| delta.extra.remove("reasoning"));
            let reasoning = match reasoning {
                Some(serde_json::Value::String(r)) if !r.is_empty() => Some(r),
                _ => None,
            };
            let text = delta.content.filter(|t| !t.is_empty());
            let calls = delta.tool_calls.unwrap_or_default();
            if reasoning.is_none()
                && text.is_none()
                && calls.is_empty()
                && choice.finish_reason.is_none()
            {
                continue;
            }
            if ci != self.current {
                self.current = ci;
                out.push(StreamEvent::Choice { index: ci });
            }
            let st = self.choice(ci);
            if let Some(r) = reasoning {
                let index = match st.reasoning_block {
                    Some(i) => i,
                    None => {
                        let i = st.new_block();
                        st.reasoning_block = Some(i);
                        i
                    }
                };
                out.push(StreamEvent::ReasoningDelta { index, text: r });
            }
            if let Some(text) = text {
                let index = match st.text_block {
                    Some(i) => i,
                    None => {
                        let i = st.new_block();
                        st.text_block = Some(i);
                        i
                    }
                };
                out.push(StreamEvent::TextDelta { index, text });
            }
            for tc in calls {
                let known = st.tools.iter().find(|(t, _)| *t == tc.index).map(|x| x.1);
                let (name, args) = match tc.function {
                    Some(f) => (f.name, f.arguments),
                    None => (None, None),
                };
                let index = match (known, tc.id) {
                    (Some(b), _) => b,
                    (None, id) => {
                        let b = st.new_block();
                        st.tools.push((tc.index, b));
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::ir::StopReason;

    fn frames(out: &[u8]) -> Vec<serde_json::Value> {
        String::from_utf8_lossy(out)
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter(|d| *d != "[DONE]")
            .map(|d| serde_json::from_str(d).unwrap())
            .collect()
    }

    #[test]
    fn encodes_reasoning_and_choices() {
        let mut enc = StreamEncoder::new(false);
        let mut out = Vec::new();
        for ev in [
            StreamEvent::Start {
                id: "c".into(),
                model: "m".into(),
                created: Some(1),
            },
            StreamEvent::ReasoningDelta {
                index: 0,
                text: "denke".into(),
            },
            StreamEvent::TextDelta {
                index: 1,
                text: "A".into(),
            },
            StreamEvent::Choice { index: 1 },
            StreamEvent::TextDelta {
                index: 0,
                text: "B".into(),
            },
            StreamEvent::Stop {
                reason: StopReason::EndTurn,
                stop_sequence: None,
            },
            StreamEvent::Choice { index: 0 },
            StreamEvent::Stop {
                reason: StopReason::EndTurn,
                stop_sequence: None,
            },
        ] {
            enc.encode(ev, &mut out);
        }
        enc.finish(&mut out);
        let f = frames(&out);
        let idx: Vec<u64> = f
            .iter()
            .map(|c| c["choices"][0]["index"].as_u64().unwrap())
            .collect();
        assert_eq!(idx, [0, 0, 0, 1, 1, 1, 0]);
        assert_eq!(f[1]["choices"][0]["delta"]["reasoning_content"], "denke");
        assert_eq!(f[3]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(f[4]["choices"][0]["delta"]["content"], "B");

        let mut enc = StreamEncoder::new(false).with_reasoning(false);
        let mut out = Vec::new();
        enc.encode(
            StreamEvent::ReasoningDelta {
                index: 0,
                text: "geheim".into(),
            },
            &mut out,
        );
        assert!(!String::from_utf8_lossy(&out).contains("geheim"));
    }

    #[test]
    fn decodes_interleaved_choices() {
        let mut d = StreamDecoder::new();
        let mut evs = Vec::new();
        for data in [
            r#"{"id":"c","model":"m","created":1,"choices":[{"index":0,"delta":{"role":"assistant","content":""}},{"index":1,"delta":{"role":"assistant","content":""}}]}"#,
            r#"{"id":"c","model":"m","created":1,"choices":[{"index":0,"delta":{"content":"A"}}]}"#,
            r#"{"id":"c","model":"m","created":1,"choices":[{"index":1,"delta":{"content":"B"}}]}"#,
            r#"{"id":"c","model":"m","created":1,"choices":[{"index":0,"delta":{},"finish_reason":"stop"},{"index":1,"delta":{},"finish_reason":"length"}]}"#,
        ] {
            evs.extend(d.decode(data).unwrap());
        }
        assert!(matches!(evs[0], StreamEvent::Start { .. }));
        assert_eq!(
            evs[1],
            StreamEvent::TextDelta {
                index: 0,
                text: "A".into()
            }
        );
        assert_eq!(evs[2], StreamEvent::Choice { index: 1 });
        assert_eq!(
            evs[3],
            StreamEvent::TextDelta {
                index: 0,
                text: "B".into()
            }
        );
        assert_eq!(evs[4], StreamEvent::Choice { index: 0 });
        assert!(matches!(evs[5], StreamEvent::Stop { .. }));
        assert_eq!(evs[6], StreamEvent::Choice { index: 1 });
        assert!(matches!(
            evs[7],
            StreamEvent::Stop {
                reason: StopReason::MaxTokens,
                ..
            }
        ));
    }
}
