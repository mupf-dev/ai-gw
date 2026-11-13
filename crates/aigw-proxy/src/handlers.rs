// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Handler der Datenebene.

use std::sync::Arc;

use aigw_core::ir::{ApiError, ChatRequest, ErrorKind};
use aigw_core::wire::{Losses, anthropic, openai};
use axum::body::Body;
use axum::extract::rejection::BytesRejection;
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::Response;
use bytes::Bytes;

use crate::ProxyState;
use crate::auth::extract_api_key;
use crate::error::ErrorFormat;
use crate::request_id::RequestId;
use crate::stream::sse_response;

type St = State<Arc<ProxyState>>;

fn json_response(status: StatusCode, body: Vec<u8>) -> Response {
    let mut res = Response::new(Body::from(body));
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
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    chat(&st, &rid, &headers, body, ErrorFormat::OpenAi).await
}

/// `POST /v1/messages`.
pub(crate) async fn messages(
    State(st): St,
    Extension(rid): Extension<RequestId>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    chat(&st, &rid, &headers, body, ErrorFormat::Anthropic).await
}

async fn chat(
    st: &ProxyState,
    rid: &RequestId,
    headers: &HeaderMap,
    body: Result<Bytes, BytesRejection>,
    format: ErrorFormat,
) -> Response {
    match chat_inner(st, headers, body, format).await {
        Ok(res) => res,
        Err(e) => {
            tracing::debug!(request_id = %rid.0, error = %e, "Anfrage abgelehnt");
            format.response(&e, Some(&rid.0))
        }
    }
}

async fn chat_inner(
    st: &ProxyState,
    headers: &HeaderMap,
    body: Result<Bytes, BytesRejection>,
    format: ErrorFormat,
) -> Result<Response, ApiError> {
    let auth = st.auth.authenticate(extract_api_key(headers).as_ref())?;
    let body = body.map_err(|r| body_error(&r, st.config.max_body_bytes))?;
    let req = match format {
        ErrorFormat::OpenAi => openai::decode_request(&body)?,
        ErrorFormat::Anthropic => anthropic::decode_request(&body)?,
    };
    drop(body);
    validate(&req)?;
    // Modellfreigabe (Architektur §3, Schritt 3); die Gruppe nutzt später das Routing.
    let _group = auth.authorize_model(&req.model)?;

    if req.is_stream() {
        let include_usage = req
            .stream_options
            .as_ref()
            .and_then(|o| o.include_usage)
            .unwrap_or(false);
        let events = st.provider.chat_stream(req).await?;
        return Ok(sse_response(format, events, include_usage));
    }

    let resp = st.provider.chat(req).await?;
    let mut losses = Losses::new();
    let out = match format {
        ErrorFormat::OpenAi => openai::encode_response(resp, &mut losses)?,
        ErrorFormat::Anthropic => anthropic::encode_response(resp, &mut losses)?,
    };
    if !losses.is_empty() {
        tracing::debug!(losses = ?losses.items(), "Antwort mit Übersetzungsverlusten");
    }
    Ok(json_response(StatusCode::OK, out))
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
    let models = auth
        .visible_models()
        .unwrap_or_else(|| st.provider.models());
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
