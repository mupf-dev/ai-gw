// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Gemeinsamer Ablauf der weiteren Endpunkte (AIGW-21: Embeddings, Bilder, Audio, Responses):
//!
//! ```text
//! Recorder ─► Auth (Key) ─► Body (Größe, Payload-Mitschnitt) ─► IR ─► Modellfreigabe
//!          ─► Route + Fähigkeit der Gruppe ─► Routing (exec: Auswahl, Retries, Fallbacks)
//!          ─► Antwort bzw. Fehler im Eingangsformat (+ Routing-Header, Request-Log)
//! ```
//!
//! Limits/Budgets (AIGW-30/31) und die Plugin-Kette (AIGW-36) hängen sich vorerst nur in den
//! Chat-Pfad (`handlers.rs`); die Stellen dafür sind hier [`Pipeline::authenticate`]
//! (nach der Schlüsselprüfung) und [`Pipeline::route`] (vor dem Routing). Ziel ist, dass
//! auch der Chat-Handler über diese Pipeline läuft.

use std::sync::Arc;

use aigw_core::ir::{ApiError, ErrorKind, UpstreamError};
use aigw_core::wire::WireFormat;
use axum::body::Body;
use axum::extract::rejection::BytesRejection;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;

use crate::ProxyState;
use crate::auth::{AuthContext, extract_api_key};
use crate::error::ErrorFormat;
use crate::handlers::{RouteInfo, body_error};
use crate::observe::Recorder;
use crate::provider::Capability;
use crate::request_id::{RequestId, RequestStart};
use crate::routing::GroupRoute;
use crate::routing::exec::{self, Operation, Outcome};

/// Zustand einer Anfrage auf dem Weg durch die Pipeline.
pub(crate) struct Pipeline<'a> {
    st: &'a ProxyState,
    rid: &'a RequestId,
    pub(crate) rec: Recorder,
    info: RouteInfo,
}

impl<'a> Pipeline<'a> {
    /// Beginnt eine Anfrage (der Recorder liefert in jedem Fall genau einen Datensatz).
    pub(crate) fn new(
        st: &'a ProxyState,
        rid: &'a RequestId,
        started: RequestStart,
        ingress: WireFormat,
    ) -> Self {
        Self {
            st,
            rid,
            rec: Recorder::new(st.observer.clone(), rid.0.clone(), started, ingress),
            info: RouteInfo::default(),
        }
    }

    /// Zustand der Datenebene.
    pub(crate) fn state(&self) -> &ProxyState {
        self.st
    }

    /// Schlüsselprüfung (Architektur §3, Schritt 1).
    pub(crate) fn authenticate(
        &mut self,
        headers: &HeaderMap,
    ) -> Result<AuthContext, UpstreamError> {
        let auth = self
            .st
            .auth
            .authenticate(extract_api_key(headers).as_ref())?;
        self.rec.set_key(&auth);
        Ok(auth)
    }

    /// Body übernehmen (Grenze `limit`; größer → `413`) und ggf. mitschneiden.
    pub(crate) fn body(
        &mut self,
        headers: &HeaderMap,
        body: Result<Bytes, BytesRejection>,
        limit: usize,
    ) -> Result<Bytes, UpstreamError> {
        let body = body.map_err(|r| body_error(&r, self.st.config.max_body_bytes))?;
        if body.len() > limit {
            return Err(UpstreamError::from(ApiError::new(
                ErrorKind::RequestTooLarge,
                format!("Request-Body ist größer als {limit} Bytes"),
            )));
        }
        self.rec.set_request_payload(headers, &body);
        Ok(body)
    }

    /// Modellfreigabe des Keys, Route der Modellgruppe und deren Fähigkeit.
    pub(crate) fn route(
        &mut self,
        auth: &AuthContext,
        model: &str,
        cap: Capability,
    ) -> Result<Arc<GroupRoute>, UpstreamError> {
        if model.trim().is_empty() {
            return Err(UpstreamError::from(
                ApiError::invalid_request("model darf nicht leer sein").with_param("model"),
            ));
        }
        let _group = auth.authorize_model(model)?;
        let route = self.st.routes.route(model).ok_or_else(|| {
            ApiError::new(
                ErrorKind::NotFound,
                format!("Modell {model:?} ist nicht konfiguriert"),
            )
            .with_param("model")
            .with_code("model_not_found")
        })?;
        if !route.supports(cap) {
            return Err(UpstreamError::from(route.unsupported_error(cap)));
        }
        Ok(route)
    }

    /// Führt die Anfrage über das Routing aus (Retries, Fallbacks, Breaker).
    pub(crate) async fn execute<O: Operation>(
        &mut self,
        route: Arc<GroupRoute>,
        op: &O,
        req: O::Req,
    ) -> Result<Outcome<O::Out>, UpstreamError> {
        let cap = exec::CapacityCtx {
            counters: self.st.limits.counters(),
            tokens: 0,
        };
        let routed = exec::execute_op(self.st.routes.as_ref(), route, op, req, cap).await;
        if let Some(m) = &routed.last {
            let d = m.provider().describe();
            self.rec.set_deployment(d);
            self.info.deployment = Some(d.deployment.to_owned());
        }
        self.info.attempts = routed.attempts.len();
        self.info.fallback.clone_from(&routed.fallback);
        self.rec.set_attempts(routed.attempts);
        routed.result
    }

    fn headers(&self, res: &mut Response) {
        if self.st.config.routing_headers {
            self.info.apply(res);
        }
    }

    /// Erfolgreiche, vollständige Antwort.
    pub(crate) fn reply(mut self, content_type: &'static str, body: Vec<u8>) -> Response {
        let body = Bytes::from(body);
        if self.rec.captures() {
            self.rec.set_response_payload(body.clone());
        }
        self.rec.finish(false);
        let mut res = Response::new(Body::from(body));
        res.headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
        self.headers(&mut res);
        res
    }

    /// Gestreamte Antwort: `build` erhält den Recorder (er gibt den Datensatz am Ende des
    /// Stroms ab) und baut die Antwort.
    pub(crate) fn stream(self, build: impl FnOnce(Recorder) -> Response) -> Response {
        let Self { st, info, rec, .. } = self;
        let mut res = build(rec);
        if st.config.routing_headers {
            info.apply(&mut res);
        }
        res
    }

    /// Fehlerantwort im OpenAI-Format (alle weiteren Endpunkte sind OpenAI-Endpunkte).
    pub(crate) fn fail(mut self, e: UpstreamError) -> Response {
        tracing::debug!(request_id = %self.rid.0, error = %e, "Anfrage abgelehnt");
        let mut res = ErrorFormat::OpenAi.response(&e.error, Some(&self.rid.0));
        if let Some(after) = e.retry_after {
            let secs = after.as_secs() + u64::from(after.subsec_nanos() > 0);
            if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
                res.headers_mut().insert(header::RETRY_AFTER, v);
            }
        }
        self.rec.fail(res.status().as_u16(), &e);
        self.rec.finish(false);
        self.headers(&mut res);
        res
    }
}

/// `Content-Type` eines Requests (leer, wenn nicht gesetzt).
pub(crate) fn content_type(headers: &HeaderMap) -> &str {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
}

/// Status `200` mit Body und `Content-Type`.
pub(crate) fn ok_with(content_type: &str, body: Body) -> Response {
    let mut res = Response::new(body);
    *res.status_mut() = StatusCode::OK;
    if let Ok(v) = HeaderValue::from_str(content_type) {
        res.headers_mut().insert(header::CONTENT_TYPE, v);
    }
    res
}
