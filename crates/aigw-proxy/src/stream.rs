// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! IR-Ereignisstrom → SSE-Antwort im Eingangsformat.
//!
//! Jedes Ereignis wird sofort kodiert und weitergereicht (kein Puffern, Q4); gelesen wird nur,
//! wenn hyper den nächsten Frame anfordert (Backpressure) – hyper fordert erst dann an, wenn
//! der vorige Frame in den Socket geschrieben ist.

use std::convert::Infallible;

use aigw_core::ir::StreamEvent;
use aigw_core::wire::openai::responses::ResponsesStreamEncoder;
use aigw_core::wire::{anthropic, openai};
use axum::body::Body;
use axum::http::{HeaderValue, header};
use axum::response::Response;
use bytes::Bytes;
use futures_util::{FutureExt as _, StreamExt as _};

use crate::error::ErrorFormat;
use crate::provider::EventStream;

enum Encoder {
    OpenAi(openai::StreamEncoder),
    Anthropic(anthropic::StreamEncoder),
    /// OpenAI Responses API (benannte Ereignisse, AIGW-21).
    Responses(Box<ResponsesStreamEncoder>),
}

impl Encoder {
    fn encode(&mut self, ev: StreamEvent, out: &mut Vec<u8>) {
        match self {
            Self::OpenAi(e) => e.encode(ev, out),
            Self::Anthropic(e) => e.encode(ev, out),
            Self::Responses(e) => e.encode(ev, out),
        }
    }

    fn finish(&mut self, out: &mut Vec<u8>) {
        match self {
            Self::OpenAi(e) => e.finish(out),
            Self::Anthropic(e) => e.finish(out),
            Self::Responses(e) => e.finish(out),
        }
    }
}

/// Höchstens so viele Bytes werden zu einem Body-Frame zusammengefasst.
const MAX_FRAME: usize = 16 * 1024;

/// Baut die SSE-Antwort. `include_usage` gilt nur für OpenAI (`stream_options`), ebenso
/// `expose_reasoning` (`delta.reasoning_content`, F10).
///
/// Jedes Ereignis wird sofort kodiert. Liegen bereits weitere Ereignisse bereit (z. B.
/// mehrere aus einem Upstream-Frame), werden sie ohne Warten in denselben Frame gepackt
/// (weniger Syscalls); es wird nie auf weitere Ereignisse gewartet.
pub(crate) fn sse_response(
    format: ErrorFormat,
    events: EventStream,
    include_usage: bool,
    expose_reasoning: bool,
) -> Response {
    let encoder = match format {
        ErrorFormat::OpenAi => Encoder::OpenAi(
            openai::StreamEncoder::new(include_usage).with_reasoning(expose_reasoning),
        ),
        ErrorFormat::Anthropic => Encoder::Anthropic(anthropic::StreamEncoder::new()),
    };
    encode_sse(events, encoder)
}

/// SSE-Antwort der Responses API (`/v1/responses`, AIGW-21).
pub(crate) fn responses_sse_response(
    events: EventStream,
    encoder: ResponsesStreamEncoder,
) -> Response {
    encode_sse(events, Encoder::Responses(Box::new(encoder)))
}

fn encode_sse(events: EventStream, encoder: Encoder) -> Response {
    let body = futures_util::stream::unfold(
        (events, encoder, false),
        |(mut events, mut enc, done)| async move {
            if done {
                return None;
            }
            let mut buf = Vec::with_capacity(512);
            let mut done = false;
            match events.next().await {
                Some(ev) => enc.encode(ev, &mut buf),
                None => {
                    enc.finish(&mut buf);
                    done = true;
                }
            }
            while !done && buf.len() < MAX_FRAME {
                match events.next().now_or_never() {
                    Some(Some(ev)) => enc.encode(ev, &mut buf),
                    Some(None) => {
                        enc.finish(&mut buf);
                        done = true;
                    }
                    None => break,
                }
            }
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
