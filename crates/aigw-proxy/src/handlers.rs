// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Handler der Datenebene.

use std::sync::Arc;

use aigw_core::ir::{ApiError, ChatRequest, ErrorKind, StreamEvent, UpstreamError};
use aigw_core::observe::error_type;
use aigw_core::wire::{Losses, WireFormat, anthropic, openai};
use axum::body::Body;
use axum::extract::rejection::BytesRejection;
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::Response;
use bytes::Bytes;
use tracing::Instrument as _;

use crate::ProxyState;
use crate::auth::extract_api_key;
use crate::error::ErrorFormat;
use crate::limits::RateHeaders;
use crate::observe::{ObservedStream, Recorder};
use crate::pipeline::Pipeline;
use crate::plugins::{PluginPhase, PluginRun, Pre, response_stream};
use crate::provider::{Capability, EventStream};
use crate::request_id::{RequestId, RequestStart};
use crate::routing::ResponseModel;
use crate::routing::exec::{self, Outcome};
use crate::stream::sse_response;
use crate::{
    ATTEMPTS_HEADER, DEPLOYMENT_HEADER, FALLBACK_HEADER, FALLBACK_SKIPPED_HEADER, MODEL_HEADER,
};
use aigw_plugins::{BUFFERED_HEADER, ChunkMode, ResponseHeaders};
use futures_util::StreamExt as _;

type St = State<Arc<ProxyState>>;

fn json_response(status: StatusCode, body: impl Into<Body>) -> Response {
    let mut res = Response::new(body.into());
    *res.status_mut() = status;
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    res
}

pub(crate) fn body_error(rejection: &BytesRejection, limit: usize) -> ApiError {
    if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
        ApiError::new(
            ErrorKind::RequestTooLarge,
            format!("Request-Body ist größer als {limit} Bytes"),
        )
    } else {
        ApiError::invalid_request(format!(
            "Request-Body nicht lesbar: {}",
            rejection.body_text()
        ))
    }
}

fn validate(req: &ChatRequest) -> Result<(), ApiError> {
    if req.model.trim().is_empty() {
        return Err(ApiError::invalid_request("model darf nicht leer sein").with_param("model"));
    }
    if req.messages.is_empty() {
        return Err(
            ApiError::invalid_request("messages darf nicht leer sein").with_param("messages")
        );
    }
    validate_choices(req)
}

/// `n` (F12): 1 bis [`MAX_CHOICES`](crate::choices::MAX_CHOICES).
pub(crate) fn validate_choices(req: &ChatRequest) -> Result<(), ApiError> {
    match req.n {
        Some(n) if n == 0 || n > crate::choices::MAX_CHOICES => {
            Err(ApiError::invalid_request(format!(
                "n muss zwischen 1 und {} liegen",
                crate::choices::MAX_CHOICES
            ))
            .with_param("n"))
        }
        _ => Ok(()),
    }
}

/// `POST /v1/chat/completions`.
pub(crate) async fn chat_completions(
    State(st): St,
    Extension(rid): Extension<RequestId>,
    Extension(started): Extension<RequestStart>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    chat(
        &st,
        &rid,
        started,
        &headers,
        body,
        ErrorFormat::OpenAi,
        None,
    )
    .await
}

/// `POST /v1/messages`.
pub(crate) async fn messages(
    State(st): St,
    Extension(rid): Extension<RequestId>,
    Extension(started): Extension<RequestStart>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    chat(
        &st,
        &rid,
        started,
        &headers,
        body,
        ErrorFormat::Anthropic,
        None,
    )
    .await
}

fn ingress(format: ErrorFormat) -> WireFormat {
    match format {
        ErrorFormat::OpenAi => WireFormat::OpenAiChat,
        ErrorFormat::Anthropic => WireFormat::AnthropicMessages,
    }
}

/// Ergebnis einer erfolgreichen Anfrage.
enum Reply {
    /// JSON-Body.
    Json(Vec<u8>),
    /// SSE-Strom (Ereignisse, Client wollte Usage im Stream).
    Stream(EventStream, bool),
}

/// Routing-Informationen und Plugin-Header für die Antwort.
#[derive(Default)]
pub(crate) struct RouteInfo {
    pub(crate) deployment: Option<String>,
    pub(crate) attempts: usize,
    pub(crate) fallback: Option<String>,
    /// Rate-Limit-Header (AIGW-30), bei Ablehnung und – falls eingeschaltet – bei Erfolg.
    pub(crate) ratelimit: Option<RateHeaders>,
    /// Antwort-Header aus Plugins (AIGW-36), zuletzt angewendet.
    pub(crate) plugin_headers: ResponseHeaders,
    /// Antwort wegen Ausgabe-Plugins gepuffert.
    pub(crate) buffered: bool,
    /// Ergebnis des Antwort-Caches (AIGW-39/40), Header `x-aigw-cache`.
    pub(crate) cache: Option<aigw_core::observe::CacheOutcome>,
    /// Grund bei `bypass` (Header `x-aigw-cache-reason`).
    pub(crate) cache_reason: Option<&'static str>,
    /// Übersprungene, nicht freigegebene Fallback-Gruppen (F80).
    pub(crate) fallback_skipped: Vec<String>,
    /// Modell laut Anbieter (Header `x-aigw-model`, F49).
    pub(crate) model: Option<String>,
    /// Reasoning nicht an OpenAI-Clients ausgeben (`settings.exposeReasoning: false` bzw.
    /// Key-Metadatum, F10).
    pub(crate) hide_reasoning: bool,
}

impl RouteInfo {
    pub(crate) fn apply(&self, res: &mut Response) {
        let h = res.headers_mut();
        if let Some(v) = self
            .deployment
            .as_deref()
            .and_then(|d| HeaderValue::from_str(d).ok())
        {
            h.insert(DEPLOYMENT_HEADER, v);
        }
        if self.attempts > 0 {
            h.insert(ATTEMPTS_HEADER, HeaderValue::from(self.attempts));
        }
        if let Some(v) = self
            .fallback
            .as_deref()
            .and_then(|g| HeaderValue::from_str(g).ok())
        {
            h.insert(FALLBACK_HEADER, v);
        }
        if !self.fallback_skipped.is_empty()
            && let Ok(v) = HeaderValue::from_str(&self.fallback_skipped.join(","))
        {
            h.insert(FALLBACK_SKIPPED_HEADER, v);
        }
        if let Some(v) = self
            .model
            .as_deref()
            .filter(|m| !m.is_empty())
            .and_then(|m| HeaderValue::from_str(m).ok())
        {
            h.insert(MODEL_HEADER, v);
        }
    }

    pub(crate) fn apply_plugins(&self, res: &mut Response) {
        if let Some(c) = self.cache {
            res.headers_mut().insert(
                crate::cache::CACHE_HEADER,
                HeaderValue::from_static(c.as_str()),
            );
        }
        if let Some(r) = self.cache_reason {
            res.headers_mut().insert(
                crate::cache::CACHE_REASON_HEADER,
                HeaderValue::from_static(r),
            );
        }
        if self.buffered {
            res.headers_mut()
                .insert(BUFFERED_HEADER, HeaderValue::from_static("plugins"));
        }
        if !self.plugin_headers.is_empty() {
            self.plugin_headers.apply(res.headers_mut());
        }
    }

    /// Routing-Ergebnis übernehmen (Deployment, Versuche, Fallback, übersprungene Gruppen).
    pub(crate) fn take_routed<T>(
        &mut self,
        routed: &mut exec::Routed<T>,
        rec: &mut Recorder,
        span: &tracing::Span,
    ) {
        if let Some(m) = &routed.last {
            let d = m.provider().describe();
            rec.set_deployment(d);
            self.deployment = Some(d.deployment.to_owned());
            if self.model.is_none() {
                self.model = Some(d.provider_model.to_owned());
            }
        }
        self.attempts = routed.attempts.len();
        self.fallback.clone_from(&routed.fallback);
        if let Some(f) = &routed.fallback {
            // Wurzel-Span (OpenTelemetry): Ausweichgruppe.
            span.record("aigw.fallback", f.as_str());
        }
        if !routed.fallback_skipped.is_empty() {
            span.record("aigw.fallback_skipped", routed.fallback_skipped.join(","));
            self.fallback_skipped = std::mem::take(&mut routed.fallback_skipped);
        }
        rec.set_attempts(std::mem::take(&mut routed.attempts));
    }
}

/// Modellname der Antwort (F49): Header `x-aigw-model` = Modell laut Anbieter (sonst das
/// Anbieter-Modell des Deployments); mit `settings.responseModel: requested` trägt der Body
/// den angefragten Namen. Bei Streams wird dafür das erste Ereignis (`Start`) gelesen – es liegt
/// bereits vor (das Routing wartet es ab).
pub(crate) async fn apply_response_model(
    outcome: Outcome,
    requested: &str,
    mode: ResponseModel,
    info: &mut RouteInfo,
) -> Outcome {
    match outcome {
        Outcome::Json(mut resp) => {
            if !resp.model.is_empty() {
                info.model = Some(resp.model.clone());
            }
            if mode == ResponseModel::Requested {
                requested.clone_into(&mut resp.model);
            }
            Outcome::Json(resp)
        }
        Outcome::Stream(mut events) => {
            let first = events.next().await;
            if let Some(StreamEvent::Start { model, .. }) = &first
                && !model.is_empty()
            {
                info.model = Some(model.clone());
            }
            let head = futures_util::stream::iter(first);
            let events: EventStream = if mode == ResponseModel::Requested {
                let requested = requested.to_owned();
                Box::pin(head.chain(events).map(move |ev| match ev {
                    StreamEvent::Start { id, created, .. } => StreamEvent::Start {
                        id,
                        model: requested.clone(),
                        created,
                    },
                    ev => ev,
                }))
            } else {
                Box::pin(head.chain(events))
            };
            Outcome::Stream(events)
        }
    }
}

/// Chat/Messages über die gemeinsame [`Pipeline`] (AIGW-78): Fehler und Rate-Limit-Header im
/// Eingangsformat. `preset`: vorab geprüfter Key (Playground, AIGW-54), sonst aus den Headern.
pub(crate) async fn chat(
    st: &ProxyState,
    rid: &RequestId,
    started: RequestStart,
    headers: &HeaderMap,
    body: Result<Bytes, BytesRejection>,
    format: ErrorFormat,
    preset: Option<crate::auth::AuthContext>,
) -> Response {
    let mut p = Pipeline::new(st, rid, started, ingress(format), headers, "chat")
        .with_error_format(format)
        .with_auth(preset);
    let span = p.span.clone();
    match chat_inner(&mut p, headers, body, format)
        .instrument(span)
        .await
    {
        Ok(Reply::Json(out)) => p.reply("application/json", out),
        Ok(Reply::Stream(events, include_usage)) => {
            let reasoning = !p.info().hide_reasoning;
            p.stream(|rec| {
                let events: EventStream = Box::pin(ObservedStream::new(events, rec));
                sse_response(format, events, include_usage, reasoning)
            })
        }
        Err(e) => p.fail(e),
    }
}

async fn chat_inner(
    p: &mut Pipeline<'_>,
    headers: &HeaderMap,
    body: Result<Bytes, BytesRejection>,
    format: ErrorFormat,
) -> Result<Reply, UpstreamError> {
    let auth = p.authenticate(headers)?;
    let limit = p.state().config.max_body_bytes;
    let body = p.body(headers, body, limit)?;
    let mut req = match format {
        ErrorFormat::OpenAi => openai::decode_request(&body)?,
        ErrorFormat::Anthropic => anthropic::decode_request(&body)?,
    };
    if format == ErrorFormat::Anthropic {
        // Beta-Kennungen für Anthropic-Ziele (AIGW-16); andere Ziele ignorieren sie.
        req.hints.anthropic_beta = headers
            .get_all("anthropic-beta")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .map(str::to_owned)
            .collect();
    }
    drop(body);
    validate(&req)?;
    // Modellfreigabe (Architektur §3, Schritt 3) und Schätzung des Token-Bedarfs mit dem
    // Tokenizer der Modellgruppe (F46); `route` prüft die Freigabe verbindlich.
    let group = auth.authorize_model(&req.model)?;
    p.rec.set_request(&req);
    // Tool-Richtlinien und Tool-Inventar (AIGW-87): nach der Modellfreigabe, vor Limits und
    // Plugin-Kette – entfernte Tools zählen nicht zur Schätzung, Guardrails sehen sie nicht.
    let tool_group = group
        .as_ref()
        .map_or(req.model.as_str(), |g| g.group.metadata.name.as_str())
        .to_owned();
    let tool_guard = crate::tools::begin(
        &p.state().tools,
        &auth,
        &tool_group,
        headers,
        &mut req,
        p.rec.payload_mode(),
        p.request_id(),
    )?;
    let est = p.state().limits.estimate(&req, group.as_ref());
    // Route, Fähigkeit `chat` und Limits/Budgets (Schritt 4): genau eine Reservierung je
    // Client-Anfrage – Retries und Fallbacks laufen darin; abgeschlossen im Recorder.
    let route = p.route(&auth, &req.model, Capability::Chat, est).await?;
    let st = p.state();
    let rid = p.request_id().to_owned();
    let policy = route.policy();
    // anthropic-beta des Clients nach settings.clientBetas (F60).
    let dropped = policy.client_betas.filter(&mut req.hints.anthropic_beta);
    if !dropped.is_empty() {
        tracing::debug!(request_id = %rid, betas = ?dropped, "anthropic-beta des Clients verworfen");
    }
    p.info_mut().hide_reasoning = !policy.expose_reasoning || auth.hides_reasoning();
    let response_model = policy.response_model;
    let requested = req.model.clone();
    let choices = req.choices();
    let include_usage = req
        .stream_options
        .as_ref()
        .and_then(|o| o.include_usage)
        .unwrap_or(false);

    // Schritt 5: Pre-Plugins (AIGW-36) – nach der Admission (Schritt 4), vor dem Routing.
    // Kette je (Org, Team, Key, Modellgruppe) vorberechnet. Reject/Respond schließen die
    // Admission über den Recorder ab (Reject: Verbrauch 0; Respond: Usage des Plugins, sonst 0)
    // – es bleibt kein reserviertes Kontingent stehen.
    let model_group = group
        .as_ref()
        .map_or_else(|| route.name(), |g| g.group.metadata.name.as_str())
        .to_owned();
    let req_stream = req.is_stream();
    let mut plugins = PluginRun::new(
        st,
        &auth,
        Some(&model_group),
        &rid,
        ingress(format),
        req_stream,
    );
    if let Some(sinks) = plugins.plugin_sinks() {
        p.rec.set_plugin_sinks(sinks);
    }
    let span = crate::trace::plugins_span("request", plugins.hooks(PluginPhase::Request));
    let mut req = match plugins
        .run_pre_plugins(req, headers, &mut p.info_mut().plugin_headers)
        .instrument(span.clone())
        .await
        .inspect_err(|e| crate::trace::fail(&span, error_type(e.kind)))?
    {
        Pre::Continue(req) => req,
        Pre::Respond(resp) => {
            p.info_mut().model = Some(resp.model.clone());
            return plugin_reply(resp, req_stream, format, p, include_usage);
        }
    };
    // Gepufferter Modus: Upstream nicht streamen, Antwort-Plugins auf der ganzen Antwort.
    let buffered = plugins.buffers(&req);
    if buffered {
        tracing::debug!(request_id = %rid, "Antwort wird für Ausgabe-Plugins gepuffert");
        req.stream = Some(false);
        req.stream_options = None;
        p.info_mut().buffered = true;
    }

    // Schritt 6: Antwort-Cache (AIGW-39/40) – nach allen Pre-Plugins, vor dem Routing.
    // Treffer laufen wie Upstream-Antworten durch Post-/Stream-Plugins; gespeichert wird die
    // rohe Upstream-Antwort (vor den Post-Plugins).
    // Mehrere Varianten (`n > 1`) werden nicht gecacht.
    let mut cache = match st.cache.as_ref().filter(|_| choices <= 1) {
        Some(c) => {
            crate::cache::CacheStep::begin(
                Some(c),
                plugins.cache_policy(),
                &auth,
                &rid,
                &model_group,
                &req,
                headers,
            )
            .await
        }
        None => None,
    };
    if let Some(c) = &cache {
        p.info_mut().cache = Some(c.outcome());
        p.info_mut().cache_reason = c.reason();
        p.rec.set_cache(c.outcome());
    }
    let outcome = match cache.as_mut().and_then(crate::cache::CacheStep::take_hit) {
        Some(hit) => {
            p.rec.set_cache_hit(&model_group, &hit);
            if req.is_stream() {
                let chunk = cache
                    .as_ref()
                    .map_or(0, crate::cache::CacheStep::chunk_chars);
                let events = aigw_cache::chunk_events(aigw_plugins::response_events(&hit), chunk);
                Outcome::Stream(Box::pin(futures_util::stream::iter(events)))
            } else {
                Outcome::Json(hit)
            }
        }
        None => {
            // Auswahl, Retries, Fallbacks (AIGW-24/25/26); Fallbacks nur auf für Key/Team
            // freigegebene Gruppen (F80).
            let op = exec::ChatOp {
                stream: req.is_stream(),
            };
            match (
                p.execute(Arc::clone(&route), &op, req).await?,
                cache.as_mut(),
            ) {
                (Outcome::Stream(events), Some(c)) => Outcome::Stream(c.tee(events)),
                (Outcome::Json(resp), Some(c)) => {
                    c.store_response(&resp);
                    Outcome::Json(resp)
                }
                (o, None) => o,
            }
        }
    };

    let outcome = apply_response_model(outcome, &requested, response_model, p.info_mut()).await;
    let resp = match outcome {
        // Schritt 10: Stream-Plugins je Ereignis vor dem Encoder; mehrere Varianten dafür
        // nacheinander (F12).
        Outcome::Stream(mut events) => {
            if choices > 1 && plugins.streams_live() {
                events = crate::choices::sequential(events);
            }
            // Tool-Aufrufe zählen, nicht erlaubte unterbinden (AIGW-87) – vor den
            // Stream-Plugins (innerste Schicht).
            if let Some(g) = tool_guard {
                events = g.wrap_stream(events);
            }
            return Ok(Reply::Stream(
                plugins.wrap_stream(events, ChunkMode::Live),
                include_usage,
            ));
        }
        Outcome::Json(resp) => resp,
    };
    // Tool-Aufrufe zählen, nicht erlaubte unterbinden (AIGW-87), vor den Post-Plugins.
    let mut resp = resp;
    if let Some(g) = &tool_guard
        && g.check_response(&mut resp)
    {
        crate::tools::guardrail_header(&mut p.info_mut().plugin_headers);
    }
    // Schritt 10: Post-Plugins auf der ganzen Antwort, vor dem Encoder.
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
        return Ok(Reply::Stream(events, include_usage));
    }
    p.rec.set_response(&resp);
    let mut losses = Losses::new();
    let reasoning = !p.info().hide_reasoning;
    let out = match format {
        ErrorFormat::OpenAi => openai::encode_response_with(resp, reasoning, &mut losses)?,
        ErrorFormat::Anthropic => anthropic::encode_response(resp, &mut losses)?,
    };
    if !losses.is_empty() {
        tracing::debug!(losses = ?losses.items(), "Antwort mit Übersetzungsverlusten");
    }
    Ok(Reply::Json(out))
}

/// Antwort eines Pre-Plugins (`Respond`): ohne Upstream, ohne Antwort-Plugins, im
/// Eingangsformat – gestreamt, wenn der Client gestreamt angefragt hat.
fn plugin_reply(
    resp: aigw_core::ir::ChatResponse,
    stream: bool,
    format: ErrorFormat,
    p: &mut Pipeline<'_>,
    include_usage: bool,
) -> Result<Reply, UpstreamError> {
    p.rec.set_plugin_response(&resp);
    if stream {
        return Ok(Reply::Stream(response_stream(&resp), include_usage));
    }
    let mut losses = Losses::new();
    let out = match format {
        ErrorFormat::OpenAi => {
            openai::encode_response_with(resp, !p.info().hide_reasoning, &mut losses)?
        }
        ErrorFormat::Anthropic => anthropic::encode_response(resp, &mut losses)?,
    };
    Ok(Reply::Json(out))
}

/// `GET /v1/models` (Anthropic-Format bei gesetztem `anthropic-version`).
pub(crate) async fn models(
    State(st): St,
    Extension(rid): Extension<RequestId>,
    headers: HeaderMap,
) -> Response {
    let format = ErrorFormat::detect("/v1/models", &headers);
    let auth = match st.auth.authenticate(extract_api_key(&headers).as_ref()) {
        Ok(a) => a,
        Err(e) => return format.response(&e, Some(&rid.0)),
    };
    // Mit Snapshot: nur die für den Key freigegebenen Modellgruppen.
    let models = auth.visible_models().unwrap_or_else(|| st.routes.models());
    let body = match format {
        ErrorFormat::OpenAi => serde_json::to_vec(&openai::encode_models(&models)),
        ErrorFormat::Anthropic => serde_json::to_vec(&anthropic::encode_models(&models)),
    };
    match body {
        Ok(b) => json_response(StatusCode::OK, b),
        Err(e) => format.response(
            &ApiError::new(ErrorKind::Internal, e.to_string()),
            Some(&rid.0),
        ),
    }
}

/// Unbekannter Pfad unter `/v1/`.
pub(crate) async fn not_found(
    Extension(rid): Extension<RequestId>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let format = ErrorFormat::detect(uri.path(), &headers);
    let err = ApiError::new(
        ErrorKind::NotFound,
        format!("Unbekannter Endpunkt {}", uri.path()),
    );
    format.response(&err, Some(&rid.0))
}

/// Falsche Methode auf einem bekannten Endpunkt.
pub(crate) async fn method_not_allowed(
    Extension(rid): Extension<RequestId>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let format = ErrorFormat::detect(uri.path(), &headers);
    let allow = if uri.path() == "/v1/models" {
        "GET, HEAD"
    } else {
        "POST"
    };
    let err = ApiError::new(
        ErrorKind::MethodNotAllowed,
        format!(
            "Methode nicht erlaubt für {} (erlaubt: {allow})",
            uri.path()
        ),
    );
    let mut res = format.response(&err, Some(&rid.0));
    res.headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static(allow));
    res
}
