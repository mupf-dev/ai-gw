// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Stream-Hilfen: Ereignisstrom durch die Stream-Haken ([`PluginStream`]), Antwort →
//! Ereignisse (gepufferter Modus, `Respond`), Gleitfenster ([`TextWindow`]).

use std::collections::VecDeque;
use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};

use aigw_core::ir::{ChatResponse, Content, PartKind, StreamEvent, ToolCallStart};
use futures_util::{Stream, StreamExt as _};

use crate::chain::ChunkRunner;

/// Ereignisstrom, dessen Ereignisse die Stream-Haken einer Kette durchlaufen.
///
/// Liest den inneren Strom nur, wenn der Konsument das nächste Ereignis anfordert
/// (Backpressure bleibt erhalten). Beendet ein Plugin den Stream, wird der innere Strom
/// fallen gelassen (Upstream-Abbruch).
pub struct PluginStream<S> {
    inner: Option<S>,
    runner: ChunkRunner,
    queue: VecDeque<StreamEvent>,
    out: Vec<StreamEvent>,
}

impl<S> fmt::Debug for PluginStream<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginStream")
            .field("runner", &self.runner)
            .field("queued", &self.queue.len())
            .finish_non_exhaustive()
    }
}

impl<S> PluginStream<S> {
    /// Umhüllt `inner`.
    pub fn new(inner: S, runner: ChunkRunner) -> Self {
        Self {
            inner: Some(inner),
            runner,
            queue: VecDeque::new(),
            out: Vec::new(),
        }
    }
}

impl<S> Stream for PluginStream<S>
where
    S: Stream<Item = StreamEvent> + Unpin,
{
    type Item = StreamEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<StreamEvent>> {
        let this = self.get_mut();
        loop {
            if let Some(ev) = this.queue.pop_front() {
                return Poll::Ready(Some(ev));
            }
            let Some(inner) = this.inner.as_mut() else {
                return Poll::Ready(None);
            };
            match inner.poll_next_unpin(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(ev)) => {
                    this.runner.push(ev, &mut this.out);
                    if this.runner.is_terminated() {
                        this.inner = None;
                    }
                }
                Poll::Ready(None) => {
                    this.runner.finish(&mut this.out);
                    this.inner = None;
                }
            }
            this.queue.extend(this.out.drain(..));
        }
    }
}

/// Zerlegt eine vollständige Antwort (erste Variante) in Stream-Ereignisse – für den
/// gepufferten Modus und für [`crate::Flow::Respond`] bei gestreamten Anfragen.
pub fn response_events(resp: &ChatResponse) -> Vec<StreamEvent> {
    let mut out = vec![StreamEvent::Start {
        id: resp.id.clone(),
        model: resp.model.clone(),
        created: resp.created,
    }];
    if let Some(choice) = resp.choices.first() {
        let mut index = 0u32;
        match &choice.message.content {
            Content::Text(t) => {
                if !t.is_empty() {
                    out.push(StreamEvent::TextDelta {
                        index,
                        text: t.clone(),
                    });
                }
            }
            Content::Parts(parts) => {
                for p in parts {
                    let before = out.len();
                    match &p.kind {
                        PartKind::Text { text } | PartKind::Refusal { text } => {
                            out.push(StreamEvent::TextDelta {
                                index,
                                text: text.clone(),
                            });
                        }
                        PartKind::Reasoning { text, signature } => {
                            out.push(StreamEvent::ReasoningDelta {
                                index,
                                text: text.clone(),
                            });
                            if let Some(signature) = signature {
                                out.push(StreamEvent::ReasoningSignature {
                                    index,
                                    signature: signature.clone(),
                                });
                            }
                        }
                        PartKind::RedactedReasoning { data } => {
                            out.push(StreamEvent::RedactedReasoning {
                                index,
                                data: data.clone(),
                            });
                        }
                        PartKind::ToolCall {
                            id,
                            name,
                            arguments,
                        } => {
                            out.push(StreamEvent::ToolCallStart(ToolCallStart {
                                index,
                                id: id.clone(),
                                name: name.clone(),
                            }));
                            if !arguments.is_empty() {
                                out.push(StreamEvent::ToolCallDelta {
                                    index,
                                    arguments: arguments.clone(),
                                });
                            }
                        }
                        // Bilder, Audio, Dateien, Tool-Ergebnisse und unbekannte Teile kommen in
                        // Antworten nicht vor bzw. haben kein Stream-Ereignis.
                        _ => {}
                    }
                    if out.len() > before {
                        index += 1;
                    }
                }
            }
        }
        if let Some(reason) = &choice.stop_reason {
            out.push(StreamEvent::Stop {
                reason: reason.clone(),
                stop_sequence: choice.stop_sequence.clone(),
            });
        }
    }
    if let Some(usage) = &resp.usage {
        out.push(StreamEvent::Usage(usage.clone()));
    }
    out
}

/// Gleitfenster über die zuletzt gestreamten Textbytes (für Stream-Plugins, die Muster über
/// Chunk-Grenzen hinweg erkennen müssen). Hält höchstens `capacity` Bytes, schneidet nur an
/// Zeichengrenzen.
#[derive(Debug, Clone, Default)]
pub struct TextWindow {
    buf: String,
    capacity: usize,
}

impl TextWindow {
    /// Fenster mit `capacity` Bytes.
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: String::with_capacity(capacity.min(64 * 1024)),
            capacity,
        }
    }

    /// Kapazität (0 = unbegrenzt ist **nicht** vorgesehen; 0 hält nichts).
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Setzt die Kapazität (z. B. beim ersten Zugriff über `ChunkCtx::state`).
    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity;
        self.trim();
    }

    /// Hängt Text an und verwirft Älteres jenseits der Kapazität.
    pub fn push(&mut self, text: &str) {
        self.buf.push_str(text);
        self.trim();
    }

    /// Aktueller Fensterinhalt.
    pub fn as_str(&self) -> &str {
        &self.buf
    }

    /// Leert das Fenster.
    pub fn clear(&mut self) {
        self.buf.clear();
    }

    fn trim(&mut self) {
        if self.buf.len() <= self.capacity {
            return;
        }
        let mut cut = self.buf.len() - self.capacity;
        while !self.buf.is_char_boundary(cut) {
            cut += 1;
        }
        self.buf.drain(..cut);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_keeps_tail_on_char_boundary() {
        let mut w = TextWindow::new(5);
        w.push("abc");
        w.push("défg");
        assert!(w.as_str().len() <= 5);
        assert!(w.as_str().ends_with("fg"));
        w.set_capacity(2);
        assert_eq!(w.as_str(), "fg");
    }
}
