// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Mehrere Antwortvarianten (`n > 1`, F12) im Ereignisstrom.
//!
//! Varianten kommen verschränkt ([`StreamEvent::Choice`] schaltet um). Die OpenAI-Chat-
//! Ausgabe kann das direkt abbilden (Chunks mit `index`). Die Responses API und Stream-Plugins
//! brauchen die Varianten **nacheinander**: [`sequential`] gibt die erste eintreffende
//! Variante live weiter und puffert die übrigen, bis sie an der Reihe sind.

use std::collections::{BTreeMap, VecDeque};
use std::pin::Pin;
use std::task::{Context, Poll};

use aigw_core::ir::{ChatResponse, StreamEvent};
use futures_util::{Stream, StreamExt as _};

use crate::provider::EventStream;

/// Höchstzahl der Antwortvarianten je Anfrage (`n`).
pub(crate) const MAX_CHOICES: u32 = 16;

/// Ordnet einen verschränkten Strom so, dass jede Variante zusammenhängend erscheint.
pub(crate) fn sequential(events: EventStream) -> EventStream {
    Box::pin(Sequential {
        inner: events,
        incoming: 0,
        live: Some(0),
        announced: 0,
        buffers: BTreeMap::new(),
        out: VecDeque::new(),
        usage: None,
        done: false,
    })
}

struct Sequential {
    inner: EventStream,
    /// Variante der gerade eintreffenden Ereignisse.
    incoming: u32,
    /// Variante, die live weitergegeben wird (`None` = die nächste eintreffende).
    live: Option<u32>,
    /// Variante, die der Empfänger zuletzt per `Choice` gesehen hat.
    announced: u32,
    /// Gepufferte Ereignisse anderer Varianten.
    buffers: BTreeMap<u32, Vec<StreamEvent>>,
    out: VecDeque<StreamEvent>,
    usage: Option<StreamEvent>,
    done: bool,
}

impl Sequential {
    fn emit(&mut self, choice: u32, ev: StreamEvent) {
        if choice != self.announced {
            self.announced = choice;
            self.out.push_back(StreamEvent::Choice { index: choice });
        }
        self.out.push_back(ev);
    }

    /// Die Live-Variante ist fertig: nächste gepufferte Variante ausgeben (ggf. mehrere, wenn
    /// sie schon vollständig sind).
    fn advance(&mut self) {
        loop {
            let Some((&next, _)) = self.buffers.iter().next() else {
                self.live = None;
                return;
            };
            let buffered = self.buffers.remove(&next).unwrap_or_default();
            let complete = buffered
                .iter()
                .any(|e| matches!(e, StreamEvent::Stop { .. }));
            for ev in buffered {
                self.emit(next, ev);
            }
            if !complete {
                self.live = Some(next);
                return;
            }
        }
    }

    fn push(&mut self, ev: StreamEvent) {
        match ev {
            StreamEvent::Choice { index } => self.incoming = index,
            StreamEvent::Start { .. } => self.out.push_back(ev),
            StreamEvent::Usage(_) => self.usage = Some(ev),
            StreamEvent::Error(_) => {
                self.out.push_back(ev);
                self.done = true;
            }
            ev => {
                let choice = self.incoming;
                let live = *self.live.get_or_insert(choice);
                if live == choice {
                    let stop = matches!(ev, StreamEvent::Stop { .. });
                    self.emit(choice, ev);
                    if stop {
                        self.advance();
                    }
                } else {
                    self.buffers.entry(choice).or_default().push(ev);
                }
            }
        }
    }

    fn flush(&mut self) {
        while let Some((choice, buffered)) = self.buffers.pop_first() {
            for ev in buffered {
                self.emit(choice, ev);
            }
        }
        if let Some(u) = self.usage.take() {
            self.out.push_back(u);
        }
    }
}

impl Stream for Sequential {
    type Item = StreamEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<StreamEvent>> {
        let this = self.get_mut();
        loop {
            if let Some(ev) = this.out.pop_front() {
                return Poll::Ready(Some(ev));
            }
            if this.done {
                return Poll::Ready(None);
            }
            match this.inner.poll_next_unpin(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(ev)) => this.push(ev),
                Poll::Ready(None) => {
                    this.done = true;
                    this.flush();
                }
            }
        }
    }
}

/// Ereignisse einer vollständigen Antwort mit beliebig vielen Varianten (gepufferter Modus,
/// Cache-Treffer, Plugin-Antwort). Eine Variante: wie [`aigw_plugins::response_events`].
pub(crate) fn response_events(resp: &ChatResponse) -> Vec<StreamEvent> {
    if resp.choices.len() <= 1 {
        return aigw_plugins::response_events(resp);
    }
    let mut out = Vec::new();
    for (i, choice) in resp.choices.iter().enumerate() {
        let single = ChatResponse {
            choices: vec![choice.clone()],
            usage: None,
            ..resp.clone()
        };
        let index = u32::try_from(i).unwrap_or(u32::MAX);
        if i > 0 {
            out.push(StreamEvent::Choice { index });
        }
        out.extend(
            aigw_plugins::response_events(&single)
                .into_iter()
                .filter(|e| i == 0 || !matches!(e, StreamEvent::Start { .. }))
                .filter(|e| !matches!(e, StreamEvent::Usage(_))),
        );
    }
    if let Some(u) = &resp.usage {
        out.push(StreamEvent::Usage(u.clone()));
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use aigw_core::ir::{StopReason, Usage};

    fn text(i: u32, t: &str) -> StreamEvent {
        let _ = i;
        StreamEvent::TextDelta {
            index: 0,
            text: t.into(),
        }
    }

    fn stop() -> StreamEvent {
        StreamEvent::Stop {
            reason: StopReason::EndTurn,
            stop_sequence: None,
        }
    }

    #[tokio::test]
    async fn orders_interleaved_choices() {
        let input = vec![
            StreamEvent::Start {
                id: "c".into(),
                model: "m".into(),
                created: None,
            },
            text(0, "a1"),
            StreamEvent::Choice { index: 1 },
            text(1, "b1"),
            StreamEvent::Choice { index: 0 },
            text(0, "a2"),
            StreamEvent::Choice { index: 2 },
            text(2, "c1"),
            stop(),
            StreamEvent::Choice { index: 1 },
            text(1, "b2"),
            StreamEvent::Choice { index: 0 },
            stop(),
            StreamEvent::Choice { index: 1 },
            stop(),
            StreamEvent::Usage(Usage {
                output_tokens: 3,
                ..Usage::default()
            }),
        ];
        let out: Vec<StreamEvent> = sequential(Box::pin(futures_util::stream::iter(input)))
            .collect()
            .await;
        let compact: Vec<String> = out
            .iter()
            .map(|e| match e {
                StreamEvent::Start { .. } => "S".into(),
                StreamEvent::Choice { index } => format!("C{index}"),
                StreamEvent::TextDelta { text, .. } => text.clone(),
                StreamEvent::Stop { .. } => "X".into(),
                StreamEvent::Usage(_) => "U".into(),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            compact,
            [
                "S", "a1", "a2", "X", "C1", "b1", "b2", "X", "C2", "c1", "X", "U"
            ]
        );
    }
}
