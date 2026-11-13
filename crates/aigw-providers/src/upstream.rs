// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Ausführung einer Anfrage gegen ein Deployment (Architektur §6 Timeouts, §7 Streaming).
//!
//! Streaming: Upstream-Body-Frames → [`StreamDecoder`] → IR-Ereignisse. Gelesen wird nur,
//! wenn der Aufrufer das nächste Ereignis anfordert (Backpressure); es gibt keinen Puffer
//! außer dem Rest einer unvollständigen SSE-Zeile und den Ereignissen eines einzelnen
//! Frames. Wird der [`EventStream`] fallen gelassen (Client-Abbruch), wird der Upstream-Body
//! sofort mit fallen gelassen – hyper schließt dann die HTTP/1.1-Verbindung bzw. setzt den
//! HTTP/2-Stream zurück.

use std::collections::VecDeque;
use std::fmt;
use std::future::Future as _;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use aigw_core::ir::{
    ApiError, ChatRequest, ChatResponse, ErrorClass, ErrorKind, StreamEvent, UpstreamError,
};
use aigw_core::resource::Id;
use aigw_core::snapshot::DeploymentCtx;
use aigw_core::types::ProviderKind;
use aigw_core::wire::Losses;
use bytes::{Bytes, BytesMut};
use futures_util::Stream;
use http_body_util::BodyExt as _;
use hyper::body::{Body as _, Incoming};
use tokio::time::{Instant, Sleep};
use tracing::Instrument as _;
use tracing::field::Empty;

use crate::client::UpstreamClient;
use crate::settings::Settings;
use crate::{ConfigError, Provider, Registry, StreamDecoder};

/// Strom von IR-Ereignissen einer gestreamten Antwort.
pub type EventStream = Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>>;

/// Zeitlimits je Deployment (`settings.timeouts`, Architektur §6).
///
/// Der Verbindungsaufbau ist im [`ClientConfig`](crate::ClientConfig) geregelt (gilt für den
/// gemeinsamen Pool).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// Bis zu den Antwort-Headern (nur Streams; nicht gestreamte Antworten kommen erst nach
    /// der vollständigen Generierung, dort gilt nur `total`).
    pub first_byte: Duration,
    /// Größte Pause zwischen zwei Body-Frames.
    pub idle: Duration,
    /// Gesamtzeit der Anfrage bis zum letzten Byte.
    pub total: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            first_byte: Duration::from_secs(120),
            idle: Duration::from_secs(120),
            total: Duration::from_secs(600),
        }
    }
}

/// Beschreibung eines Deployments (für Log, Metriken, Request-Log).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamInfo {
    /// Name des Deployments.
    pub deployment: String,
    /// ID des Deployments (aus dem Snapshot; `None` bei programmatisch gebauten).
    pub deployment_id: Option<Id>,
    /// Modellgruppe des Deployments.
    pub model_group: Option<String>,
    /// Anbieter.
    pub provider: ProviderKind,
    /// Modellname beim Anbieter.
    pub provider_model: String,
}

/// Größte gelesene Fehlerantwort (Rest wird verworfen).
const MAX_ERROR_BODY: usize = 64 * 1024;
/// Größte nicht gestreamte Antwort.
const MAX_RESPONSE_BODY: usize = 64 * 1024 * 1024;

/// Ein Deployment: Adapter + Client + Zeitlimits.
#[derive(Clone)]
pub struct Upstream {
    info: UpstreamInfo,
    provider: Arc<dyn Provider>,
    client: UpstreamClient,
    timeouts: Timeouts,
}

impl fmt::Debug for Upstream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Upstream")
            .field("info", &self.info)
            .field("timeouts", &self.timeouts)
            .finish_non_exhaustive()
    }
}

impl Upstream {
    /// Neues Deployment.
    pub fn new(
        deployment: impl Into<String>,
        provider: Arc<dyn Provider>,
        client: UpstreamClient,
        timeouts: Timeouts,
    ) -> Self {
        Self {
            info: UpstreamInfo {
                deployment: deployment.into(),
                deployment_id: None,
                model_group: None,
                provider: provider.kind(),
                provider_model: provider.provider_model().to_owned(),
            },
            provider,
            client,
            timeouts,
        }
    }

    /// Baut das Deployment aus dem Snapshot (Adapter über die Registry, Zeitlimits aus
    /// `settings.timeouts`).
    pub fn from_deployment(
        dep: &DeploymentCtx,
        registry: &Registry,
        client: &UpstreamClient,
    ) -> Result<Self, ConfigError> {
        let provider = registry.build(dep)?;
        let timeouts = Settings::of(dep).timeouts()?;
        let mut up = Self::new(
            dep.deployment.metadata.name.clone(),
            provider,
            client.clone(),
            timeouts,
        );
        up.info.deployment_id = Some(dep.deployment.id);
        up.info.model_group = Some(dep.deployment.spec.model_group.clone());
        Ok(up)
    }

    /// Beschreibung.
    pub fn info(&self) -> &UpstreamInfo {
        &self.info
    }

    /// Adapter.
    pub fn provider(&self) -> &Arc<dyn Provider> {
        &self.provider
    }

    /// Zeitlimits.
    pub fn timeouts(&self) -> Timeouts {
        self.timeouts
    }

    /// Asynchrone Vorbereitung des Adapters (z. B. OAuth-Token), begrenzt durch `deadline`.
    async fn prepare(&self, deadline: Instant) -> Result<(), UpstreamError> {
        let Some(fut) = self.provider.prepare() else {
            return Ok(());
        };
        tokio::time::timeout_at(deadline, fut)
            .await
            .unwrap_or_else(|_| Err(timeout_error("Anmeldung beim Anbieter")))
    }

    fn build(&self, req: ChatRequest) -> Result<crate::UpstreamRequest, UpstreamError> {
        let mut losses = Losses::new();
        let http_req = self
            .provider
            .build_request(req, &mut losses)
            .map_err(UpstreamError::from)?;
        if !losses.is_empty() {
            tracing::debug!(
                deployment = %self.info.deployment,
                losses = ?losses.items(),
                "Anfrage mit Übersetzungsverlusten"
            );
        }
        Ok(http_req)
    }

    /// Span eines Upstream-Versuchs (OpenTelemetry, AIGW-45; ohne aktives Tracing
    /// `Span::none()`).
    fn attempt_span(&self, stream: bool) -> tracing::Span {
        let info = &self.info;
        let span = tracing::info_span!(
            target: aigw_core::observe::TRACE_TARGET,
            "upstream",
            otel.name = Empty,
            otel.kind = "client",
            otel.status_code = Empty,
            gen_ai.operation.name = "chat",
            gen_ai.system = aigw_core::observe::gen_ai_system(info.provider),
            gen_ai.request.model = info.provider_model.as_str(),
            server.address = Empty,
            server.port = Empty,
            http.response.status_code = Empty,
            error.type = Empty,
            aigw.deployment = info.deployment.as_str(),
            aigw.stream = stream,
        );
        if !span.is_disabled() {
            span.record("otel.name", format!("chat {}", info.provider_model));
        }
        span
    }

    /// Ergebnis eines Versuchs am Span vermerken.
    fn record_attempt<T>(span: &tracing::Span, res: &Result<T, UpstreamError>) {
        if let Err(e) = res {
            span.record("otel.status_code", "error");
            span.record("error.type", e.class.as_str());
        }
    }

    async fn send(
        &self,
        mut req: crate::UpstreamRequest,
        deadline: Instant,
    ) -> Result<http::Response<Incoming>, UpstreamError> {
        // W3C-Trace-Kontext weitergeben (nur bei aktivem Tracing und `propagate`).
        if let Some(tp) = aigw_core::observe::current_traceparent()
            && let Ok(v) = http::HeaderValue::from_str(&tp)
        {
            req.headers_mut().insert("traceparent", v);
        }
        let span = tracing::Span::current();
        if !span.is_disabled() {
            if let Some(host) = req.uri().host() {
                span.record("server.address", host);
            }
            if let Some(port) = req.uri().port_u16() {
                span.record("server.port", port);
            }
        }
        let res = tokio::time::timeout_at(deadline, self.client.send(req)).await;
        if let Ok(Ok(resp)) = &res {
            span.record("http.response.status_code", resp.status().as_u16());
        }
        match res {
            Err(_) => Err(timeout_error("keine Antwort-Header")),
            Ok(Err(e)) => {
                tracing::warn!(
                    deployment = %self.info.deployment,
                    error = %error_chain(&e),
                    "Upstream nicht erreichbar"
                );
                Err(UpstreamError::new(
                    ErrorClass::Connect,
                    ApiError::new(
                        ErrorKind::BadGateway,
                        "Anbieter nicht erreichbar (Verbindungsfehler)",
                    )
                    .with_code("upstream_connect_error"),
                ))
            }
            Ok(Ok(resp)) if resp.status().is_success() => Ok(resp),
            Ok(Ok(resp)) => Err(self.error_response(resp, deadline).await),
        }
    }

    async fn error_response(
        &self,
        resp: http::Response<Incoming>,
        deadline: Instant,
    ) -> UpstreamError {
        let (parts, body) = resp.into_parts();
        // Fehler-Bodies sind klein; zu große oder zu langsame werden abgeschnitten.
        let body = read_limited(body, deadline, self.timeouts.idle, MAX_ERROR_BODY)
            .await
            .unwrap_or_else(|partial| partial);
        let err = self
            .provider
            .classify_error(parts.status, &parts.headers, &body);
        tracing::debug!(
            deployment = %self.info.deployment,
            status = parts.status.as_u16(),
            class = %err.class,
            "Upstream-Fehler"
        );
        err
    }

    /// Nicht gestreamte Anfrage.
    pub async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, UpstreamError> {
        let span = self.attempt_span(false);
        let res = self.chat_attempt(req).instrument(span.clone()).await;
        Self::record_attempt(&span, &res);
        res
    }

    async fn chat_attempt(&self, req: ChatRequest) -> Result<ChatResponse, UpstreamError> {
        let deadline = Instant::now() + self.timeouts.total;
        self.prepare(deadline).await?;
        let http_req = self.build(req)?;
        let resp = self.send(http_req, deadline).await?;
        let body = read_limited(
            resp.into_body(),
            deadline,
            self.timeouts.idle,
            MAX_RESPONSE_BODY,
        )
        .await
        .map_err(|_| {
            UpstreamError::new(
                ErrorClass::Timeout,
                ApiError::new(
                    ErrorKind::Timeout,
                    "Anbieter-Antwort unvollständig (Zeitlimit oder Abbruch)",
                ),
            )
        })?;
        self.provider.parse_response(&body).map_err(|e| {
            // Inhaltsfilter in einer 200-Antwort (z. B. Gemini `promptFeedback.blockReason`).
            if e.code.as_deref() == Some("content_filter") {
                return UpstreamError::new(ErrorClass::ContentFilter, e);
            }
            UpstreamError::new(
                ErrorClass::Protocol,
                ApiError::new(
                    ErrorKind::BadGateway,
                    format!("Anbieter-Antwort nicht lesbar: {}", e.message),
                )
                .with_code("upstream_protocol_error"),
            )
        })
    }

    /// Gestreamte Anfrage. Fehler bis zu den Antwort-Headern kommen als `Err`; alles danach
    /// als [`StreamEvent::Error`] (danach endet der Strom).
    pub async fn chat_stream(&self, req: ChatRequest) -> Result<EventStream, UpstreamError> {
        // Der Span endet mit den Antwort-Headern; den Stream deckt der Wurzel-Span ab.
        let span = self.attempt_span(true);
        let res = self.chat_stream_attempt(req).instrument(span.clone()).await;
        Self::record_attempt(&span, &res);
        res
    }

    async fn chat_stream_attempt(&self, req: ChatRequest) -> Result<EventStream, UpstreamError> {
        let start = Instant::now();
        let total = start + self.timeouts.total;
        let first_byte = total.min(start + self.timeouts.first_byte);
        self.prepare(first_byte).await?;
        let http_req = self.build(req)?;
        let resp = self.send(http_req, first_byte).await?;
        Ok(Box::pin(UpstreamEvents::new(
            resp.into_body(),
            self.provider.stream_decoder(),
            total,
            self.timeouts.idle,
        )))
    }
}

fn timeout_error(what: &str) -> UpstreamError {
    UpstreamError::new(
        ErrorClass::Timeout,
        ApiError::new(
            ErrorKind::Timeout,
            format!("Zeitlimit beim Anbieter überschritten ({what})"),
        )
        .with_code("upstream_timeout"),
    )
}

/// Ursachenkette eines Fehlers (für Logs).
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        s.push_str(": ");
        s.push_str(&c.to_string());
        cur = c.source();
    }
    s
}

/// Liest einen Body bis `limit` Bytes (Rest wird verworfen). `Err` enthält das bis zum
/// Zeitlimit/Abbruch Gelesene.
async fn read_limited(
    mut body: Incoming,
    deadline: Instant,
    idle: Duration,
    limit: usize,
) -> Result<Bytes, Bytes> {
    let mut buf = BytesMut::new();
    loop {
        let until = deadline.min(Instant::now() + idle);
        match tokio::time::timeout_at(until, body.frame()).await {
            Ok(Some(Ok(frame))) => {
                if let Ok(data) = frame.into_data() {
                    let room = limit.saturating_sub(buf.len());
                    buf.extend_from_slice(&data[..data.len().min(room)]);
                    if buf.len() >= limit {
                        return Ok(buf.freeze());
                    }
                }
            }
            Ok(None) => return Ok(buf.freeze()),
            Ok(Some(Err(_))) | Err(_) => return Err(buf.freeze()),
        }
    }
}

/// Ereignisstrom über einem Upstream-Body.
struct UpstreamEvents {
    body: Option<Incoming>,
    decoder: Box<dyn StreamDecoder>,
    scratch: Vec<StreamEvent>,
    queue: VecDeque<StreamEvent>,
    timer: Pin<Box<Sleep>>,
    total: Instant,
    idle: Duration,
    done: bool,
}

impl UpstreamEvents {
    fn new(
        body: Incoming,
        decoder: Box<dyn StreamDecoder>,
        total: Instant,
        idle: Duration,
    ) -> Self {
        let first = total.min(Instant::now() + idle);
        Self {
            body: Some(body),
            decoder,
            scratch: Vec::with_capacity(8),
            queue: VecDeque::with_capacity(8),
            timer: Box::pin(tokio::time::sleep_until(first)),
            total,
            idle,
            done: false,
        }
    }

    fn flush_scratch(&mut self) {
        self.queue.extend(self.scratch.drain(..));
    }

    /// Bricht mit einem Fehlerereignis ab (der Upstream-Body wird sofort geschlossen).
    fn fail(&mut self, err: ApiError) {
        self.body = None;
        self.queue.push_back(StreamEvent::Error(err));
    }
}

impl Stream for UpstreamEvents {
    type Item = StreamEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<StreamEvent>> {
        let this = self.get_mut();
        loop {
            if let Some(ev) = this.queue.pop_front() {
                if matches!(ev, StreamEvent::Error(_)) {
                    // Nach einem Fehler kommt nichts mehr.
                    this.queue.clear();
                    this.body = None;
                    this.done = true;
                }
                return Poll::Ready(Some(ev));
            }
            if this.done {
                return Poll::Ready(None);
            }
            let Some(body) = this.body.as_mut() else {
                this.done = true;
                continue;
            };
            match Pin::new(body).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data() {
                        this.decoder.push(&data, &mut this.scratch);
                        this.flush_scratch();
                    }
                    let next = this.total.min(Instant::now() + this.idle);
                    this.timer.as_mut().reset(next);
                }
                Poll::Ready(Some(Err(e))) => {
                    tracing::debug!(error = %error_chain(&e), "Upstream-Stream abgebrochen");
                    this.decoder.finish(&mut this.scratch);
                    let ended_cleanly = this.decoder.is_done()
                        && !this
                            .scratch
                            .iter()
                            .any(|e| matches!(e, StreamEvent::Error(_)));
                    if ended_cleanly {
                        this.flush_scratch();
                        this.body = None;
                    } else {
                        this.scratch.clear();
                        this.fail(
                            ApiError::new(
                                ErrorKind::BadGateway,
                                "Verbindung zum Anbieter mitten im Stream abgebrochen",
                            )
                            .with_code("upstream_stream_interrupted"),
                        );
                    }
                }
                Poll::Ready(None) => {
                    this.decoder.finish(&mut this.scratch);
                    this.flush_scratch();
                    this.body = None;
                }
                Poll::Pending => {
                    ready!(this.timer.as_mut().poll(cx));
                    let what = if Instant::now() >= this.total {
                        "Gesamtzeit"
                    } else {
                        "Pause zwischen Chunks"
                    };
                    this.fail(
                        ApiError::new(
                            ErrorKind::Timeout,
                            format!("Zeitlimit beim Anbieter überschritten ({what})"),
                        )
                        .with_code("upstream_timeout"),
                    );
                }
            }
        }
    }
}
