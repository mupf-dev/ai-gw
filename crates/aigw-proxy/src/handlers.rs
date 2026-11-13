// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Handler der Datenebene.

use std::sync::Arc;

use aigw_core::ir::{ApiError, ChatRequest, ErrorKind, UpstreamError};
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
use crate::limits::{RateHeaders, TokenEstimate, retry_after_secs};
use crate::observe::{ObservedStream, Recorder};
use crate::plugins::{PluginPhase, PluginRun, Pre, response_stream};
use crate::provider::EventStream;
use crate::request_id::{RequestId, RequestStart};
use crate::routing::exec::{self, Outcome};
use crate::stream::sse_response;
use crate::{ATTEMPTS_HEADER, DEPLOYMENT_HEADER, FALLBACK_HEADER};
use aigw_plugins::{BUFFERED_HEADER, ChunkMode, ResponseHeaders};

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
    Ok(())
}

/// `POST /v1/chat/completions`.
pub(crate) async fn chat_completions(
    State(st): St,
    Extension(rid): Extension<RequestId>,
    Extension(started): Extension<RequestStart>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    chat(&st, &rid, started, &headers, body, ErrorFormat::OpenAi).await
}

/// `POST /v1/messages`.
pub(crate) async fn messages(
    State(st): St,
    Extension(rid): Extension<RequestId>,
    Extension(started): Extension<RequestStart>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    chat(&st, &rid, started, &headers, body, ErrorFormat::Anthropic).await
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
    }

    pub(crate) fn apply_plugins(&self, res: &mut Response) {
        if let Some(c) = self.cache {
            res.headers_mut().insert(
                crate::cache::CACHE_HEADER,
                HeaderValue::from_static(c.as_str()),
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
}

async fn chat(
    st: &ProxyState,
    rid: &RequestId,
    started: RequestStart,
    headers: &HeaderMap,
    body: Result<Bytes, BytesRejection>,
    format: ErrorFormat,
) -> Response {
    let mut rec = Recorder::new(st.observer.clone(), rid.0.clone(), started, ingress(format));
    let mut info = RouteInfo::default();
    let span = crate::trace::request_span(&rid.0, ingress(format), headers);
    rec.set_span(span.clone());
    let mut res = match chat_inner(st, rid, headers, body, format, &mut rec, &mut info)
        .instrument(span)
        .await
    {
        Ok(Reply::Json(out)) => {
            let out = Bytes::from(out);
            if rec.captures() {
                rec.set_response_payload(out.clone());
            }
            rec.finish(false);
            json_response(StatusCode::OK, out)
        }
        Ok(Reply::Stream(events, include_usage)) => {
            let events: EventStream = Box::pin(ObservedStream::new(events, rec));
            sse_response(format, events, include_usage)
        }
        Err(e) => {
            tracing::debug!(request_id = %rid.0, error = %e, "Anfrage abgelehnt");
            let mut res = format.response(&e.error, Some(&rid.0));
            if let Some(after) = e.retry_after {
                let secs = retry_after_secs(after);
                if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
                    res.headers_mut().insert(header::RETRY_AFTER, v);
                }
            }
            rec.fail(res.status().as_u16(), &e);
            rec.finish(false);
            res
        }
    };
    if st.config.routing_headers {
        info.apply(&mut res);
    }
    if let Some(h) = &info.ratelimit {
        h.apply(format, res.headers_mut(), std::time::SystemTime::now());
    }
    info.apply_plugins(&mut res);
    res
}

async fn chat_inner(
    st: &ProxyState,
    rid: &RequestId,
    headers: &HeaderMap,
    body: Result<Bytes, BytesRejection>,
    format: ErrorFormat,
    rec: &mut Recorder,
    info: &mut RouteInfo,
) -> Result<Reply, UpstreamError> {
    let auth = {
        let span = crate::trace::auth_span();
        let _g = span.enter();
        st.auth
            .authenticate(extract_api_key(headers).as_ref())
            .inspect_err(|e| crate::trace::fail(&span, error_type(e.kind)))?
    };
    rec.set_key(&auth);
    let body = body.map_err(|r| body_error(&r, st.config.max_body_bytes))?;
    rec.set_request_payload(headers, &body);
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
    // Modellfreigabe (Architektur §3, Schritt 3); die Gruppe nutzt später das Routing.
    let group = auth.authorize_model(&req.model)?;
    rec.set_request(&req);

    // Limits und Budgets (Architektur §3, Schritt 4; AIGW-30/31): genau eine Reservierung je
    // Client-Anfrage – Retries und Fallbacks laufen darin. Abgeschlossen wird im Recorder
    // (Kosten, Budget, Abgleich mit der tatsächlichen Usage).
    let est = TokenEstimate::of(&req, st.limits.default_max_tokens());
    match st
        .limits
        .admit(&auth, group.as_ref(), est, std::time::SystemTime::now())
        .await
    {
        Ok(admission) => {
            if st.limits.response_headers() && !admission.headers().is_empty() {
                info.ratelimit = Some(admission.headers().clone());
            }
            rec.set_admission(admission);
        }
        Err(denied) => {
            let denied = *denied;
            info.ratelimit = (!denied.headers.is_empty()).then_some(denied.headers);
            return Err(denied.error);
        }
    }

    let route_span = crate::trace::route_span(&req.model);
    let route = route_span
        .in_scope(|| st.routes.route(&req.model))
        .ok_or_else(|| {
            crate::trace::fail(&route_span, "not_found");
            ApiError::new(
                ErrorKind::NotFound,
                format!("Modell {:?} ist nicht konfiguriert", req.model),
            )
            .with_param("model")
            .with_code("model_not_found")
        })?;
    route_span.record("aigw.model_group", route.name());
    drop(route_span);
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
        .map_or_else(|| route.name(), |g| g.group.metadata.name.as_str());
    let req_stream = req.is_stream();
    let mut plugins = PluginRun::new(
        st,
        &auth,
        Some(model_group),
        &rid.0,
        ingress(format),
        req_stream,
    );
    if let Some(hits) = plugins.guardrail_hits() {
        rec.set_guardrails(hits);
    }
    let span = crate::trace::plugins_span("request", plugins.hooks(PluginPhase::Request));
    let mut req = match plugins
        .run_pre_plugins(req, headers, &mut info.plugin_headers)
        .instrument(span.clone())
        .await
        .inspect_err(|e| crate::trace::fail(&span, error_type(e.kind)))?
    {
        Pre::Continue(req) => req,
        Pre::Respond(resp) => return plugin_reply(resp, req_stream, format, rec, include_usage),
    };
    // Gepufferter Modus: Upstream nicht streamen, Antwort-Plugins auf der ganzen Antwort.
    let buffered = plugins.buffers(&req);
    if buffered {
        tracing::debug!(request_id = %rid.0, "Antwort wird für Ausgabe-Plugins gepuffert");
        req.stream = Some(false);
        req.stream_options = None;
        info.buffered = true;
    }

    // Schritt 6: Antwort-Cache (AIGW-39/40) – nach allen Pre-Plugins, vor dem Routing.
    // Treffer laufen wie Upstream-Antworten durch Post-/Stream-Plugins; gespeichert wird die
    // rohe Upstream-Antwort (vor den Post-Plugins).
    let mut cache = match st.cache.as_ref() {
        Some(c) => {
            crate::cache::CacheStep::begin(
                Some(c),
                plugins.cache_policy(),
                &auth,
                model_group,
                &req,
                headers,
            )
            .await
        }
        None => None,
    };
    if let Some(c) = &cache {
        info.cache = Some(c.outcome());
        rec.set_cache(c.outcome());
    }
    let outcome = match cache.as_mut().and_then(crate::cache::CacheStep::take_hit) {
        Some(hit) => {
            rec.set_cache_hit(model_group, &hit);
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
            // Auswahl, Retries, Fallbacks (AIGW-24/25/26).
            let routed = exec::execute(
                st.routes.as_ref(),
                route,
                req,
                exec::CapacityCtx {
                    counters: st.limits.counters(),
                    tokens: est.total(),
                },
            )
            .await;
            if let Some(m) = &routed.last {
                let d = m.provider().describe();
                rec.set_deployment(d);
                info.deployment = Some(d.deployment.to_owned());
            }
            info.attempts = routed.attempts.len();
            info.fallback.clone_from(&routed.fallback);
            if let Some(f) = &routed.fallback {
                // Wurzel-Span (OpenTelemetry): Ausweichgruppe.
                tracing::Span::current().record("aigw.fallback", f.as_str());
            }
            rec.set_attempts(routed.attempts);
            match (routed.result?, cache.as_mut()) {
                (Outcome::Stream(events), Some(c)) => Outcome::Stream(c.tee(events)),
                (Outcome::Json(resp), Some(c)) => {
                    c.store_response(&resp);
                    Outcome::Json(resp)
                }
                (o, None) => o,
            }
        }
    };

    let resp = match outcome {
        // Schritt 10: Stream-Plugins je Ereignis vor dem Encoder.
        Outcome::Stream(events) => {
            return Ok(Reply::Stream(
                plugins.wrap_stream(events, ChunkMode::Live),
                include_usage,
            ));
        }
        Outcome::Json(resp) => resp,
    };
    // Schritt 10: Post-Plugins auf der ganzen Antwort, vor dem Encoder.
    let resp = if plugins.hooks(PluginPhase::Response) > 0 {
        let span = crate::trace::plugins_span("response", plugins.hooks(PluginPhase::Response));
        plugins
            .run_post_plugins(resp, &mut info.plugin_headers)
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
    rec.set_response(&resp);
    let mut losses = Losses::new();
    let out = match format {
        ErrorFormat::OpenAi => openai::encode_response(resp, &mut losses)?,
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
    rec: &mut Recorder,
    include_usage: bool,
) -> Result<Reply, UpstreamError> {
    rec.set_plugin_response(&resp);
    if stream {
        return Ok(Reply::Stream(response_stream(&resp), include_usage));
    }
    let mut losses = Losses::new();
    let out = match format {
        ErrorFormat::OpenAi => openai::encode_response(resp, &mut losses)?,
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
