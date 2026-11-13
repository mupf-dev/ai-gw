// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Weitere OpenAI-Endpunkte (AIGW-21): Embeddings, Bilder, Audio und die Responses API.
//!
//! Alle laufen über die gemeinsame [`Pipeline`](crate::pipeline::Pipeline) und dasselbe
//! Routing wie Chat (Auswahl, Retries, Fallbacks, Circuit Breaker); die Modellgruppe muss die
//! jeweilige Fähigkeit in `settings.capabilities` freigeben (sonst `400`).

use std::sync::Arc;

use aigw_core::ir::audio::{AudioTask, SpeechRequest, TranscriptionRequest, TranscriptionResponse};
use aigw_core::ir::embeddings::{EmbeddingsRequest, EmbeddingsResponse};
use aigw_core::ir::images::{ImageRequest, ImageResponse};
use aigw_core::ir::{ApiError, ErrorKind, UpstreamError, Usage};
use aigw_core::wire::openai::responses::{
    ResponsesEcho, ResponsesStreamEncoder, decode_responses_request, encode_responses_response,
};
use aigw_core::wire::openai::{audio, embeddings, images};
use aigw_core::wire::{Losses, WireFormat};
use axum::body::Body;
use axum::extract::rejection::BytesRejection;
use axum::extract::{Extension, State};
use axum::http::HeaderMap;
use axum::response::Response;
use bytes::Bytes;
use tracing::Instrument as _;

use crate::ProxyState;
use crate::limits::TokenEstimate;
use crate::observe::{ObservedBytes, ObservedStream, estimate_tokens};
use crate::pipeline::{Pipeline, content_type, ok_with};
use crate::plugins::{PluginPhase, PluginRun, Pre, response_stream};
use crate::provider::{BoxFuture, Capability, ChatProvider, EventStream, SpeechStream};
use crate::request_id::{RequestId, RequestStart};
use crate::routing::exec::{ChatOp, Operation, Outcome};
use crate::stream::responses_sse_response;
use aigw_core::observe::error_type;
use aigw_plugins::ChunkMode;

type St = State<Arc<ProxyState>>;

/// Höchstens so viele Bilder je Anfrage.
const MAX_IMAGES: u32 = 10;

fn losses_log(losses: &Losses) {
    if !losses.is_empty() {
        tracing::debug!(losses = ?losses.items(), "Antwort mit Übersetzungsverlusten");
    }
}

fn unexpected_stream() -> UpstreamError {
    UpstreamError::from(ApiError::new(
        ErrorKind::Internal,
        "unerwartete Stream-Antwort",
    ))
}

// ---------------------------------------------------------------------------------------------
// Operationen für das Routing
// ---------------------------------------------------------------------------------------------

pub(crate) struct EmbeddingsOp;

impl Operation for EmbeddingsOp {
    type Req = EmbeddingsRequest;
    type Out = EmbeddingsResponse;
    const CAPABILITY: Capability = Capability::Embeddings;

    fn call(
        &self,
        provider: Arc<dyn ChatProvider>,
        req: EmbeddingsRequest,
    ) -> BoxFuture<Result<Outcome<EmbeddingsResponse>, UpstreamError>> {
        let fut = provider.embeddings(req);
        Box::pin(async move { fut.await.map(Outcome::Json) })
    }

    fn tokens(out: &EmbeddingsResponse) -> u64 {
        out.usage.input_tokens
    }
}

struct ImagesOp;

impl Operation for ImagesOp {
    type Req = ImageRequest;
    type Out = ImageResponse;
    const CAPABILITY: Capability = Capability::Images;

    fn call(
        &self,
        provider: Arc<dyn ChatProvider>,
        req: ImageRequest,
    ) -> BoxFuture<Result<Outcome<ImageResponse>, UpstreamError>> {
        let fut = provider.images(req);
        Box::pin(async move { fut.await.map(Outcome::Json) })
    }

    fn tokens(out: &ImageResponse) -> u64 {
        out.usage.total_tokens()
    }
}

struct TranscriptionOp;

impl Operation for TranscriptionOp {
    type Req = TranscriptionRequest;
    type Out = TranscriptionResponse;
    const CAPABILITY: Capability = Capability::AudioTranscription;

    fn call(
        &self,
        provider: Arc<dyn ChatProvider>,
        req: TranscriptionRequest,
    ) -> BoxFuture<Result<Outcome<TranscriptionResponse>, UpstreamError>> {
        let fut = provider.transcription(req);
        Box::pin(async move { fut.await.map(Outcome::Json) })
    }

    fn tokens(out: &TranscriptionResponse) -> u64 {
        out.usage.total_tokens()
    }
}

struct SpeechOp;

impl Operation for SpeechOp {
    type Req = SpeechRequest;
    type Out = SpeechStream;
    const CAPABILITY: Capability = Capability::AudioSpeech;

    fn call(
        &self,
        provider: Arc<dyn ChatProvider>,
        req: SpeechRequest,
    ) -> BoxFuture<Result<Outcome<SpeechStream>, UpstreamError>> {
        let fut = provider.speech(req);
        Box::pin(async move { fut.await.map(Outcome::Json) })
    }

    fn tokens(_out: &SpeechStream) -> u64 {
        0
    }
}

// ---------------------------------------------------------------------------------------------
// POST /v1/embeddings
// ---------------------------------------------------------------------------------------------

/// `POST /v1/embeddings`.
pub(crate) async fn embeddings(
    State(st): St,
    Extension(rid): Extension<RequestId>,
    Extension(started): Extension<RequestStart>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let mut p = Pipeline::new(
        &st,
        &rid,
        started,
        WireFormat::OpenAiEmbeddings,
        &headers,
        "embeddings",
    );
    let span = p.span.clone();
    match embeddings_inner(&mut p, &headers, body)
        .instrument(span)
        .await
    {
        Ok(out) => p.reply("application/json", out),
        Err(e) => p.fail(e),
    }
}

async fn embeddings_inner(
    p: &mut Pipeline<'_>,
    headers: &HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Vec<u8>, UpstreamError> {
    let auth = p.authenticate(headers)?;
    let limit = p.state().config.max_body_bytes;
    let body = p.body(headers, body, limit)?;
    let req = embeddings::decode_embeddings_request(&body)?;
    drop(body);
    p.rec.set_target(&req.model, false, req.input.byte_len());
    let format = req.encoding_format.unwrap_or_default();
    let est = TokenEstimate {
        input: estimate_tokens(req.input.byte_len()),
        output: 0,
    };
    let route = p
        .route(&auth, &req.model, Capability::Embeddings, est)
        .await?;
    let Outcome::Json(resp) = p.execute(route, &EmbeddingsOp, req).await? else {
        return Err(unexpected_stream());
    };
    p.rec.set_usage(resp.usage.clone(), resp.usage_estimated);
    let mut losses = Losses::new();
    let out = embeddings::encode_embeddings_response(resp, format, &mut losses)?;
    losses_log(&losses);
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// POST /v1/images/generations
// ---------------------------------------------------------------------------------------------

/// `POST /v1/images/generations`.
pub(crate) async fn images_generations(
    State(st): St,
    Extension(rid): Extension<RequestId>,
    Extension(started): Extension<RequestStart>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let mut p = Pipeline::new(
        &st,
        &rid,
        started,
        WireFormat::OpenAiImages,
        &headers,
        "image_generation",
    );
    let span = p.span.clone();
    match images_inner(&mut p, &headers, body).instrument(span).await {
        Ok(out) => p.reply("application/json", out),
        Err(e) => p.fail(e),
    }
}

async fn images_inner(
    p: &mut Pipeline<'_>,
    headers: &HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Vec<u8>, UpstreamError> {
    let auth = p.authenticate(headers)?;
    let limit = p.state().config.max_body_bytes;
    let body = p.body(headers, body, limit)?;
    let req = images::decode_image_request(&body)?;
    drop(body);
    if req.prompt.trim().is_empty() {
        return Err(ApiError::invalid_request("prompt darf nicht leer sein")
            .with_param("prompt")
            .into());
    }
    if !(1..=MAX_IMAGES).contains(&req.count()) {
        return Err(ApiError::invalid_request(format!(
            "n muss zwischen 1 und {MAX_IMAGES} liegen"
        ))
        .with_param("n")
        .into());
    }
    p.rec.set_target(&req.model, false, req.prompt.len());
    let requested = req.response_format;
    let est = TokenEstimate {
        input: estimate_tokens(req.prompt.len()),
        output: 0,
    };
    let route = p.route(&auth, &req.model, Capability::Images, est).await?;
    let Outcome::Json(resp) = p.execute(route, &ImagesOp, req).await? else {
        return Err(unexpected_stream());
    };
    p.rec.set_usage(resp.usage.clone(), false);
    let mut losses = Losses::new();
    let out = images::encode_image_response(resp, requested, &mut losses)?;
    losses_log(&losses);
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// POST /v1/audio/transcriptions, /v1/audio/translations
// ---------------------------------------------------------------------------------------------

/// `POST /v1/audio/transcriptions`.
pub(crate) async fn audio_transcriptions(
    State(st): St,
    Extension(rid): Extension<RequestId>,
    Extension(started): Extension<RequestStart>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    audio_text(&st, &rid, started, &headers, body, AudioTask::Transcribe).await
}

/// `POST /v1/audio/translations`.
pub(crate) async fn audio_translations(
    State(st): St,
    Extension(rid): Extension<RequestId>,
    Extension(started): Extension<RequestStart>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    audio_text(&st, &rid, started, &headers, body, AudioTask::Translate).await
}

async fn audio_text(
    st: &ProxyState,
    rid: &RequestId,
    started: RequestStart,
    headers: &HeaderMap,
    body: Result<Bytes, BytesRejection>,
    task: AudioTask,
) -> Response {
    let mut p = Pipeline::new(
        st,
        rid,
        started,
        WireFormat::OpenAiAudio,
        headers,
        "audio_transcription",
    );
    let span = p.span.clone();
    match audio_text_inner(&mut p, headers, body, task)
        .instrument(span)
        .await
    {
        Ok((ct, out)) => p.reply(ct, out),
        Err(e) => p.fail(e),
    }
}

async fn audio_text_inner(
    p: &mut Pipeline<'_>,
    headers: &HeaderMap,
    body: Result<Bytes, BytesRejection>,
    task: AudioTask,
) -> Result<(&'static str, Vec<u8>), UpstreamError> {
    let auth = p.authenticate(headers)?;
    let limit = p.state().config.max_audio_bytes;
    let body = p.body(headers, body, limit)?;
    let req = audio::decode_transcription_request(content_type(headers), &body, task)?;
    p.rec.set_target(&req.model, false, 0);
    // Audio hat keinen Token-Bedarf vor der Antwort; Anfrage-Limits greifen trotzdem.
    let route = p
        .route(
            &auth,
            &req.model,
            Capability::AudioTranscription,
            TokenEstimate::default(),
        )
        .await?;
    let Outcome::Json(resp) = p.execute(route, &TranscriptionOp, req).await? else {
        return Err(unexpected_stream());
    };
    p.rec.set_usage(resp.usage.clone(), false);
    Ok(audio::encode_transcription_response(resp, task)?)
}

// ---------------------------------------------------------------------------------------------
// POST /v1/audio/speech
// ---------------------------------------------------------------------------------------------

/// `POST /v1/audio/speech` – die Audio-Bytes werden ungepuffert durchgereicht.
pub(crate) async fn audio_speech(
    State(st): St,
    Extension(rid): Extension<RequestId>,
    Extension(started): Extension<RequestStart>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let mut p = Pipeline::new(
        &st,
        &rid,
        started,
        WireFormat::OpenAiAudio,
        &headers,
        "audio_speech",
    );
    let span = p.span.clone();
    match speech_inner(&mut p, &headers, body).instrument(span).await {
        Ok(speech) => p.stream(|rec| {
            let SpeechStream { content_type, body } = speech;
            ok_with(
                &content_type,
                Body::from_stream(ObservedBytes::new(body, rec)),
            )
        }),
        Err(e) => p.fail(e),
    }
}

async fn speech_inner(
    p: &mut Pipeline<'_>,
    headers: &HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<SpeechStream, UpstreamError> {
    let auth = p.authenticate(headers)?;
    let limit = p.state().config.max_body_bytes;
    let body = p.body(headers, body, limit)?;
    let req = audio::decode_speech_request(&body)?;
    drop(body);
    if req.input.trim().is_empty() {
        return Err(ApiError::invalid_request("input darf nicht leer sein")
            .with_param("input")
            .into());
    }
    let characters = u64::try_from(req.input.chars().count()).unwrap_or(u64::MAX);
    p.rec.set_target(&req.model, true, 0);
    let est = TokenEstimate {
        input: estimate_tokens(req.input.len()),
        output: 0,
    };
    let route = p
        .route(&auth, &req.model, Capability::AudioSpeech, est)
        .await?;
    let Outcome::Json(speech) = p.execute(route, &SpeechOp, req).await? else {
        return Err(unexpected_stream());
    };
    p.rec.set_usage(
        Usage {
            characters: Some(characters),
            ..Usage::default()
        },
        false,
    );
    Ok(speech)
}

// ---------------------------------------------------------------------------------------------
// POST /v1/responses
// ---------------------------------------------------------------------------------------------

enum ResponsesReply {
    Json(Vec<u8>),
    Stream(EventStream, Box<ResponsesEcho>),
}

/// `POST /v1/responses` (OpenAI Responses API, abgebildet auf die Chat-IR).
pub(crate) async fn responses(
    State(st): St,
    Extension(rid): Extension<RequestId>,
    Extension(started): Extension<RequestStart>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let mut p = Pipeline::new(
        &st,
        &rid,
        started,
        WireFormat::OpenAiResponses,
        &headers,
        "chat",
    );
    let span = p.span.clone();
    match responses_inner(&mut p, &headers, body)
        .instrument(span)
        .await
    {
        Ok(ResponsesReply::Json(out)) => p.reply("application/json", out),
        Ok(ResponsesReply::Stream(events, echo)) => p.stream(|rec| {
            let events: EventStream = Box::pin(ObservedStream::new(events, rec));
            responses_sse_response(events, ResponsesStreamEncoder::new(*echo))
        }),
        Err(e) => p.fail(e),
    }
}

async fn responses_inner(
    p: &mut Pipeline<'_>,
    headers: &HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<ResponsesReply, UpstreamError> {
    let auth = p.authenticate(headers)?;
    let limit = p.state().config.max_body_bytes;
    let body = p.body(headers, body, limit)?;
    let (req, echo) = decode_responses_request(&body)?;
    drop(body);
    if req.messages.is_empty() {
        return Err(ApiError::invalid_request("input darf nicht leer sein")
            .with_param("input")
            .into());
    }
    p.rec.set_request(&req);
    let est = TokenEstimate::of(&req, p.state().limits.default_max_tokens());
    let route = p.route(&auth, &req.model, Capability::Chat, est).await?;

    // Plugin-Kette (AIGW-36) wie im Chat-Pfad: nach der Admission, vor dem Routing.
    let req_stream = req.is_stream();
    let mut plugins = PluginRun::new(
        p.state(),
        &auth,
        Some(route.name()),
        p.request_id(),
        WireFormat::OpenAiResponses,
        req_stream,
    );
    let span = crate::trace::plugins_span("request", plugins.hooks(PluginPhase::Request));
    let pre = plugins
        .run_pre_plugins(req, headers, &mut p.info_mut().plugin_headers)
        .instrument(span.clone())
        .await
        .inspect_err(|e| crate::trace::fail(&span, error_type(e.kind)))?;
    let mut req = match pre {
        Pre::Continue(req) => req,
        Pre::Respond(resp) => {
            p.rec.set_plugin_response(&resp);
            if req_stream {
                return Ok(ResponsesReply::Stream(
                    response_stream(&resp),
                    Box::new(echo),
                ));
            }
            return encode(resp, &echo).map(ResponsesReply::Json);
        }
    };
    let buffered = plugins.buffers(&req);
    if buffered {
        req.stream = Some(false);
        req.stream_options = None;
        p.info_mut().buffered = true;
    }

    let op = ChatOp {
        stream: req.is_stream(),
    };
    let resp = match p.execute(route, &op, req).await? {
        Outcome::Stream(events) => {
            return Ok(ResponsesReply::Stream(
                plugins.wrap_stream(events, ChunkMode::Live),
                Box::new(echo),
            ));
        }
        Outcome::Json(resp) => resp,
    };
    let resp = if plugins.hooks(PluginPhase::Response) > 0 {
        let span = crate::trace::plugins_span("response", plugins.hooks(PluginPhase::Response));
        plugins
            .run_post_plugins(resp, &mut p.info_mut().plugin_headers)
            .instrument(span.clone())
            .await
            .inspect_err(|e| crate::trace::fail(&span, error_type(e.kind)))?
    } else {
        resp
    };
    if buffered {
        let events = plugins.wrap_stream(response_stream(&resp), ChunkMode::Buffered);
        return Ok(ResponsesReply::Stream(events, Box::new(echo)));
    }
    p.rec.set_response(&resp);
    encode(resp, &echo).map(ResponsesReply::Json)
}

fn encode(
    resp: aigw_core::ir::ChatResponse,
    echo: &ResponsesEcho,
) -> Result<Vec<u8>, UpstreamError> {
    let mut losses = Losses::new();
    let out = encode_responses_response(resp, echo, &mut losses)?;
    losses_log(&losses);
    Ok(out)
}
