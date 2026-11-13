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
use crate::observe::{ObservedStream, Recorder};
use crate::provider::EventStream;
use crate::request_id::{RequestId, RequestStart};
use crate::routing::exec::{self, Outcome};
use crate::stream::sse_response;
use crate::{ATTEMPTS_HEADER, DEPLOYMENT_HEADER, FALLBACK_HEADER};

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

fn body_error(rejection: &BytesRejection, limit: usize) -> ApiError {
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

/// Routing-Informationen für die Antwort-Header.
#[derive(Default)]
struct RouteInfo {
    deployment: Option<String>,
    attempts: usize,
    fallback: Option<String>,
}

impl RouteInfo {
    fn apply(&self, res: &mut Response) {
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
    let mut res = match chat_inner(st, headers, body, format, &mut rec, &mut info)
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
                let secs = after.as_secs() + u64::from(after.subsec_nanos() > 0);
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
    res
}

async fn chat_inner(
    st: &ProxyState,
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
    let _group = auth.authorize_model(&req.model)?;
    rec.set_request(&req);
    // Platzhalter-Span, bis die Plugin-Kette (Architektur §9) eingehängt wird.
    drop(crate::trace::plugins_span());

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

    // Auswahl, Retries, Fallbacks (AIGW-24/25/26).
    let routed = exec::execute(st.routes.as_ref(), route, req).await;
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

    let resp = match routed.result? {
        Outcome::Stream(events) => return Ok(Reply::Stream(events, include_usage)),
        Outcome::Json(resp) => resp,
    };
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
