// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Gemeinsamer Ablauf aller Endpunkte der Datenebene (Chat/Messages, Responses, Embeddings,
//! Bilder, Audio; AIGW-21/AIGW-78):
//!
//! ```text
//! Recorder ─► Auth (Key) ─► Body (Größe, Payload-Mitschnitt) ─► IR ─► Modellfreigabe
//!          ─► Route + Fähigkeit der Gruppe ─► Limits/Budgets ─► [Plugins, Cache]
//!          ─► Routing (exec: Auswahl, Retries, Fallbacks)
//!          ─► Antwort bzw. Fehler im Eingangsformat (+ Routing-Header, Request-Log)
//! ```
//!
//! Limits und Budgets (AIGW-30/31) greifen mit genau einer Reservierung je Client-Anfrage
//! ([`Pipeline::route`]; Tokens geschätzt je Endpunkt, abgerechnet mit der tatsächlichen
//! Usage). Die Plugin-Kette (AIGW-36) arbeitet auf der Chat-IR und läuft für Chat/Messages
//! und `/v1/responses`; Embeddings, Bilder und Audio haben die Anfrage-Haken über eine
//! Chat-Sicht der Eingabe ([`crate::plugins::PluginRun::check_input`]).

use std::sync::Arc;
use std::time::SystemTime;

use aigw_core::ir::{ApiError, ChatRequest, ErrorKind, UpstreamError};
use aigw_core::observe::error_type;
use aigw_core::snapshot::ModelGroupCtx;
use aigw_core::wire::WireFormat;
use axum::body::Body;
use axum::extract::rejection::BytesRejection;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use tracing::Span;

use crate::ProxyState;
use crate::auth::{AuthContext, extract_api_key};
use crate::error::ErrorFormat;
use crate::handlers::{RouteInfo, body_error};
use crate::limits::{TokenEstimate, retry_after_secs};
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
    /// Wurzel-Span (OpenTelemetry, AIGW-45); der Handler führt die Anfrage darin aus.
    pub(crate) span: Span,
    /// Geschätzter Token-Bedarf (Limits und Deployment-Kapazität).
    est: TokenEstimate,
    /// Geprüfter Key (Freigaben der Fallback-Gruppen, F80).
    auth: Option<AuthContext>,
    /// Format der Fehlerantworten und Rate-Limit-Header (Chat: OpenAI oder Anthropic).
    format: ErrorFormat,
    /// Eingangsformat (für Plugins).
    ingress: WireFormat,
    /// Vorab geprüfter Key (Playground der Admin-UI, AIGW-54): ersetzt die Prüfung des
    /// Headers, alle übrigen Schritte (Freigaben, Limits, Budgets, Plugins) bleiben gleich.
    preset: Option<AuthContext>,
}

impl<'a> Pipeline<'a> {
    /// Beginnt eine Anfrage (der Recorder liefert in jedem Fall genau einen Datensatz).
    pub(crate) fn new(
        st: &'a ProxyState,
        rid: &'a RequestId,
        started: RequestStart,
        ingress: WireFormat,
        headers: &HeaderMap,
        operation: &'static str,
    ) -> Self {
        let span = crate::trace::request_span_op(&rid.0, ingress, headers, operation);
        let mut rec = Recorder::new(st.observer.clone(), rid.0.clone(), started, ingress);
        rec.set_span(span.clone());
        rec.set_client_request_id(headers);
        Self {
            st,
            rid,
            rec,
            info: RouteInfo::default(),
            span,
            est: TokenEstimate::default(),
            auth: None,
            format: ErrorFormat::OpenAi,
            ingress,
            preset: None,
        }
    }

    /// Key-Kontext vorgeben statt ihn aus den Headern zu prüfen (AIGW-54).
    #[must_use]
    pub(crate) fn with_auth(mut self, auth: Option<AuthContext>) -> Self {
        self.preset = auth;
        self
    }

    /// Fehler und Rate-Limit-Header in diesem Format (Standard OpenAI).
    #[must_use]
    pub(crate) fn with_error_format(mut self, format: ErrorFormat) -> Self {
        self.format = format;
        self
    }

    /// Zustand der Datenebene.
    pub(crate) fn state(&self) -> &'a ProxyState {
        self.st
    }

    /// Request-ID.
    pub(crate) fn request_id(&self) -> &str {
        &self.rid.0
    }

    /// Routing-/Plugin-Informationen für die Antwort-Header.
    pub(crate) fn info_mut(&mut self) -> &mut RouteInfo {
        &mut self.info
    }

    /// Schlüsselprüfung (Architektur §3, Schritt 1).
    pub(crate) fn authenticate(
        &mut self,
        headers: &HeaderMap,
    ) -> Result<AuthContext, UpstreamError> {
        let auth = if let Some(auth) = self.preset.take() {
            auth
        } else {
            let span = crate::trace::auth_span();
            let _g = span.enter();
            self.st
                .auth
                .authenticate(extract_api_key(headers).as_ref())
                .inspect_err(|e| crate::trace::fail(&span, error_type(e.kind)))?
        };
        self.rec.set_key(&auth);
        self.auth = Some(auth.clone());
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

    /// Modellfreigabe des Keys, Route der Modellgruppe und deren Fähigkeit, danach Limits und
    /// Budgets (eine Reservierung je Anfrage mit dem geschätzten Bedarf `est`).
    pub(crate) async fn route(
        &mut self,
        auth: &AuthContext,
        model: &str,
        cap: Capability,
        est: TokenEstimate,
    ) -> Result<Arc<GroupRoute>, UpstreamError> {
        if model.trim().is_empty() {
            return Err(UpstreamError::from(
                ApiError::invalid_request("model darf nicht leer sein").with_param("model"),
            ));
        }
        let group = auth.authorize_model(model)?;
        let route_span = crate::trace::route_span(model);
        let route = route_span
            // S-31: Route zur freigegebenen Gruppe aus derselben Konfigurationsversion.
            .in_scope(|| {
                self.st
                    .routes
                    .route_for(model, auth.snapshot.as_deref(), group.as_deref())
            })
            .ok_or_else(|| {
                crate::trace::fail(&route_span, "not_found");
                ApiError::new(
                    ErrorKind::NotFound,
                    format!("Modell {model:?} ist nicht konfiguriert"),
                )
                .with_param("model")
                .with_code("model_not_found")
            })?;
        route_span.record("aigw.model_group", route.name());
        if !route.supports(cap) {
            crate::trace::fail(&route_span, "unsupported_capability");
            return Err(UpstreamError::from(route.unsupported_error(cap)));
        }
        drop(route_span);
        // Limits und Budgets (Architektur §3, Schritt 4; AIGW-30/31), abgeschlossen im
        // Recorder (Kosten, Budget, Abgleich mit der Usage).
        match self
            .st
            .limits
            .admit(auth, group.as_ref(), est, SystemTime::now())
            .await
        {
            Ok(admission) => {
                if self.st.limits.response_headers() && !admission.headers().is_empty() {
                    self.info.ratelimit = Some(admission.headers().clone());
                }
                self.rec.set_admission(admission);
            }
            Err(denied) => {
                let denied = *denied;
                self.info.ratelimit = (!denied.headers.is_empty()).then_some(denied.headers);
                return Err(denied.error);
            }
        }
        self.est = est;
        Ok(route)
    }

    /// S-38: Nach den Pre-Plugins die Eingabe neu schätzen; ist sie gewachsen, die Differenz
    /// nachreservieren (siehe [`Admission::grow_input`](crate::limits::Admission::grow_input)).
    /// Abgelehnt wird mit derselben Meldung wie bei der Admission.
    pub(crate) async fn grow_input(
        &mut self,
        req: &ChatRequest,
        group: Option<&Arc<ModelGroupCtx>>,
    ) -> Result<(), UpstreamError> {
        let Some(admission) = self.rec.admission_mut() else {
            return Ok(());
        };
        let est = self.st.limits.estimate(req, group);
        if let Err(denied) = admission.grow_input(est.input, SystemTime::now()).await {
            let denied = *denied;
            self.info.ratelimit = (!denied.headers.is_empty()).then_some(denied.headers);
            return Err(denied.error);
        }
        self.est.input = self.est.input.max(est.input);
        Ok(())
    }

    /// Anfrage-Haken der Plugin-Kette auf den Eingabetexten eines Endpunkts ohne Chat-Ausgabe
    /// (nach der Admission, vor dem Routing; siehe
    /// [`PluginRun::check_input`](crate::plugins::PluginRun::check_input)). Liefert die ggf.
    /// veränderten Texte.
    pub(crate) async fn input_plugins(
        &mut self,
        auth: &AuthContext,
        route: &GroupRoute,
        model: &str,
        texts: Vec<String>,
        headers: &HeaderMap,
    ) -> Result<Vec<String>, UpstreamError> {
        let mut plugins = crate::plugins::PluginRun::new(
            self.st,
            auth,
            Some(route.name()),
            &self.rid.0,
            self.ingress,
            false,
        );
        let hooks = plugins.hooks(crate::plugins::PluginPhase::Request);
        if hooks == 0 {
            return Ok(texts);
        }
        if let Some(sinks) = plugins.plugin_sinks() {
            self.rec.set_plugin_sinks(sinks);
        }
        let span = crate::trace::plugins_span("request", hooks);
        let out = {
            use tracing::Instrument as _;
            plugins
                .check_input(model, texts, headers, &mut self.info.plugin_headers)
                .instrument(span.clone())
                .await
                .inspect_err(|e| crate::trace::fail(&span, error_type(e.kind)))?
        };
        Ok(out)
    }

    /// Wirksame Cache-Richtlinie (Plugin `cache`) für Key und Modellgruppe.
    pub(crate) fn cache_policy(
        &self,
        auth: &AuthContext,
        route: &GroupRoute,
    ) -> Option<Arc<aigw_plugins::builtin::cache::CachePolicy>> {
        crate::plugins::PluginRun::new(
            self.st,
            auth,
            Some(route.name()),
            &self.rid.0,
            self.ingress,
            false,
        )
        .cache_policy()
    }

    /// Führt die Anfrage über das Routing aus (Retries, Fallbacks, Breaker).
    pub(crate) async fn execute<O: Operation>(
        &mut self,
        route: Arc<GroupRoute>,
        op: &O,
        req: O::Req,
    ) -> Result<Outcome<O::Out>, UpstreamError> {
        let auth = self.auth.as_ref();
        let grants = |name: &str| auth.is_none_or(|a| a.allows_group(name));
        let cap = exec::CapacityCtx {
            counters: self.st.limits.counters(),
            tokens: self.est.total(),
            grants: auth
                .is_some_and(|a| a.key.is_some())
                .then_some(&grants as exec::Grants<'_>),
        };
        let mut routed = exec::execute_op(self.st.routes.as_ref(), route, op, req, cap).await;
        self.info
            .take_routed(&mut routed, &mut self.rec, &self.span);
        routed.result
    }

    fn headers(&self, res: &mut Response) {
        apply_headers(self.st, &self.info, self.format, res);
    }

    /// Routing-/Plugin-Informationen (lesend).
    pub(crate) fn info(&self) -> &RouteInfo {
        &self.info
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
        let Self {
            st,
            info,
            rec,
            format,
            ..
        } = self;
        let mut res = build(rec);
        apply_headers(st, &info, format, &mut res);
        res
    }

    /// Fehlerantwort im Format des Endpunkts.
    pub(crate) fn fail(mut self, e: UpstreamError) -> Response {
        tracing::debug!(request_id = %self.rid.0, error = %e, "Anfrage abgelehnt");
        let mut res = self.format.response(&e.error, Some(&self.rid.0));
        if let Some(after) = e.retry_after {
            let secs = retry_after_secs(after);
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

/// Routing-Header (falls eingeschaltet) und Rate-Limit-Header.
fn apply_headers(st: &ProxyState, info: &RouteInfo, format: ErrorFormat, res: &mut Response) {
    if st.config.routing_headers {
        info.apply(res);
    }
    if let Some(h) = &info.ratelimit {
        h.apply(format, res.headers_mut(), SystemTime::now());
    }
    info.apply_plugins(res);
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
    res.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    res
}
