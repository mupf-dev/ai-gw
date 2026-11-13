// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Spans für OpenTelemetry-Traces (AIGW-45), nur über `tracing` mit
//! `target: "aigw::otel"` ([`TRACE_TARGET`]).
//!
//! Wurzel-Span `chat {model}` (Art `server`) je Anfrage mit Kind-Spans `auth`, `plugins`
//! (Platzhalter bis zur Plugin-Kette), `route` und – im Adapter – je Upstream-Versuch;
//! Ereignis `first_token` bei Streams. Attribute nach den GenAI-Semantic-Conventions. Ob
//! aufgezeichnet wird, entscheidet `aigw-observe`; ist Tracing aus, sind alle Spans
//! `Span::none()` und kosten praktisch nichts.

use aigw_core::observe::TRACE_TARGET;
use aigw_core::wire::WireFormat;
use axum::http::HeaderMap;
use tracing::Span;
use tracing::field::Empty;

/// Eingehender W3C-Header.
const TRACEPARENT: &str = "traceparent";

fn ingress_name(f: WireFormat) -> &'static str {
    match f {
        WireFormat::OpenAiChat => "openai-chat",
        WireFormat::AnthropicMessages => "anthropic-messages",
        WireFormat::BedrockConverse => "bedrock-converse",
        WireFormat::Gemini => "gemini",
        WireFormat::OpenAiResponses => "openai-responses",
        WireFormat::OpenAiEmbeddings => "openai-embeddings",
        WireFormat::OpenAiImages => "openai-images",
        WireFormat::OpenAiAudio => "openai-audio",
        WireFormat::Mcp => "mcp",
    }
}

/// Wurzel-Span einer Chat-Anfrage.
pub(crate) fn request_span(request_id: &str, ingress: WireFormat, headers: &HeaderMap) -> Span {
    request_span_op(request_id, ingress, headers, "chat")
}

/// Wurzel-Span einer Anfrage mit GenAI-Operation `operation` (`chat`, `embeddings`,
/// `image_generation`, `audio_transcription`, `audio_speech`; AIGW-21).
pub(crate) fn request_span_op(
    request_id: &str,
    ingress: WireFormat,
    headers: &HeaderMap,
    operation: &'static str,
) -> Span {
    let traceparent = headers
        .get(TRACEPARENT)
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.len() <= 128);
    tracing::info_span!(
        target: TRACE_TARGET,
        "request",
        otel.name = operation,
        otel.kind = "server",
        otel.traceparent = traceparent,
        otel.status_code = Empty,
        gen_ai.operation.name = operation,
        gen_ai.system = Empty,
        gen_ai.provider.name = Empty,
        gen_ai.request.model = Empty,
        gen_ai.request.max_tokens = Empty,
        gen_ai.response.model = Empty,
        gen_ai.usage.input_tokens = Empty,
        gen_ai.usage.output_tokens = Empty,
        http.response.status_code = Empty,
        error.type = Empty,
        aigw.request_id = request_id,
        aigw.ingress = ingress_name(ingress),
        aigw.model_group = Empty,
        aigw.deployment = Empty,
        aigw.stream = Empty,
        aigw.usage_estimated = Empty,
        aigw.client_aborted = Empty,
        aigw.ttft_ms = Empty,
        aigw.attempts = Empty,
        aigw.fallback = Empty,
        aigw.fallback_skipped = Empty,
        aigw.client_request_id = Empty,
        aigw.cache = Empty,
        aigw.guardrail_hits = Empty,
    )
}

/// Schlüsselprüfung.
pub(crate) fn auth_span() -> Span {
    tracing::info_span!(
        target: TRACE_TARGET,
        "auth",
        otel.status_code = Empty,
        error.type = Empty,
        aigw.team_id = Empty,
    )
}

/// Plugin-Kette (AIGW-36): eine Phase (`request` bzw. `response`) mit der Zahl der
/// teilnehmenden Plugins. Stream-Haken laufen synchron je Ereignis und bekommen keinen Span.
pub(crate) fn plugins_span(phase: &'static str, count: usize) -> Span {
    tracing::info_span!(
        target: TRACE_TARGET,
        "plugins",
        aigw.plugins.phase = phase,
        aigw.plugins.count = u64::try_from(count).unwrap_or(u64::MAX),
        otel.status_code = Empty,
        error.type = Empty,
    )
}

/// Routing-Auswahl.
pub(crate) fn route_span(model: &str) -> Span {
    tracing::info_span!(
        target: TRACE_TARGET,
        "route",
        gen_ai.request.model = model,
        aigw.model_group = Empty,
        otel.status_code = Empty,
    )
}

/// Markiert einen Span als fehlgeschlagen.
pub(crate) fn fail(span: &Span, error_type: &str) {
    span.record("otel.status_code", "error");
    span.record("error.type", error_type);
}
