// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! IR-Ereignisstrom → SSE-Antwort im Eingangsformat.
//!
//! Jedes Ereignis wird sofort kodiert und als eigener Body-Frame weitergereicht (kein Puffern,
//! Q4); gelesen wird nur, wenn hyper den nächsten Frame anfordert (Backpressure).

use std::convert::Infallible;

use aigw_core::ir::StreamEvent;
use aigw_core::wire::{anthropic, openai};
use axum::body::Body;
use axum::http::{HeaderValue, header};
use axum::response::Response;
use bytes::Bytes;
use futures_util::StreamExt;

use crate::error::ErrorFormat;
use crate::provider::EventStream;

enum Encoder {
    OpenAi(openai::StreamEncoder),
    Anthropic(anthropic::StreamEncoder),
}

impl Encoder {
    fn encode(&mut self, ev: StreamEvent, out: &mut Vec<u8>) {
        match self {
            Self::OpenAi(e) => e.encode(ev, out),
            Self::Anthropic(e) => e.encode(ev, out),
        }
    }

    fn finish(&mut self, out: &mut Vec<u8>) {
        match self {
            Self::OpenAi(e) => e.finish(out),
            Self::Anthropic(e) => e.finish(out),
        }
    }
}

/// Baut die SSE-Antwort. `include_usage` gilt nur für OpenAI (`stream_options`).
pub(crate) fn sse_response(
    format: ErrorFormat,
    events: EventStream,
    include_usage: bool,
) -> Response {
    let encoder = match format {
        ErrorFormat::OpenAi => Encoder::OpenAi(openai::StreamEncoder::new(include_usage)),
        ErrorFormat::Anthropic => Encoder::Anthropic(anthropic::StreamEncoder::new()),
    };
    let body = futures_util::stream::unfold(
        (events, encoder, false),
        |(mut events, mut enc, done)| async move {
            if done {
                return None;
            }
            let mut buf = Vec::with_capacity(256);
            let done = match events.next().await {
                Some(ev) => {
                    enc.encode(ev, &mut buf);
                    false
                }
                None => {
                    enc.finish(&mut buf);
                    true
                }
            };
            Some((Bytes::from(buf), (events, enc, done)))
        },
    )
    .filter(|b| std::future::ready(!b.is_empty()))
    .map(Ok::<_, Infallible>);

    let mut res = Response::new(Body::from_stream(body));
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    // Reverse-Proxies (nginx) sollen nicht puffern.
    h.insert("x-accel-buffering", HeaderValue::from_static("no"));
    res
}
