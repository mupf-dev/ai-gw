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

use aigw_core::ir::audio::{SpeechRequest, TranscriptionRequest, TranscriptionResponse};
use aigw_core::ir::embeddings::{EmbeddingsRequest, EmbeddingsResponse};
use aigw_core::ir::images::{ImageRequest, ImageResponse};
use aigw_core::ir::{
    ApiError, ChatRequest, ChatResponse, ErrorClass, ErrorKind, StreamEvent, UpstreamError,
};
use aigw_core::resource::Id;
use aigw_core::snapshot::DeploymentCtx;
use aigw_core::types::ProviderKind;
use aigw_core::wire::Losses;
use bytes::{Bytes, BytesMut};
use futures_util::{Stream, StreamExt as _};
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

/// Strom roher Antwort-Bytes (Sprachausgabe). Ein `Err` beendet den Strom.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, ApiError>> + Send + 'static>>;

/// Antwort der Sprachausgabe: `Content-Type` des Anbieters und die Audio-Bytes als Strom.
pub struct SpeechStream {
    /// `Content-Type` (z. B. `audio/mpeg`).
    pub content_type: String,
    /// Audio-Bytes.
    pub body: ByteStream,
}

impl fmt::Debug for SpeechStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpeechStream")
            .field("content_type", &self.content_type)
            .finish_non_exhaustive()
    }
}

/// Höchstens so viele Teilanfragen einer Embedding-Anfrage laufen gleichzeitig.
const EMBEDDING_CONCURRENCY: usize = 8;

/// Zeitlimits je Deployment (`settings.timeouts`, Architektur §6; an der Modellgruppe
/// gesetzt, gelten sie für alle ihre Deployments ohne eigene Angabe, F44).
///
/// `connect` wirkt je Deployment über eine eigene Pool-Variante des Clients (F187), sobald
/// `connectMs` gesetzt ist und vom Standard des Clients abweicht.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// Verbindungsaufbau (`connectMs`, Standard 10 s).
    pub connect: Duration,
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
            connect: Duration::from_secs(10),
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

/// Standard der Trace-Weitergabe je Anbieterart (F98): an für selbst betriebene Ziele
/// (`vllm`, `ollama`, `openai-compatible`, `mock`), aus für externe Cloud-Anbieter (die den
/// Header nicht auswerten; Trace-IDs sollen das eigene Netz nicht verlassen). Überschreibbar
/// je Deployment bzw. Modellgruppe mit `settings.propagateTraceContext`.
pub fn default_propagation(kind: ProviderKind) -> bool {
    matches!(
        kind,
        ProviderKind::Vllm
            | ProviderKind::Ollama
            | ProviderKind::OpenAiCompatible
            | ProviderKind::Mock
    )
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
    /// W3C-`traceparent` an diesen Anbieter weitergeben (F98).
    propagate_trace: bool,
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
            propagate_trace: default_propagation(provider.kind()),
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
        let settings = Settings::of(dep);
        let timeouts = settings.timeouts()?;
        let propagate = settings.bool("propagateTraceContext")?;
        // Eigene CA je Deployment (`settings.caFile`, F48): Variante mit eigenem Pool.
        let client = match settings.str("caFile")? {
            Some(path) if !path.trim().is_empty() => client
                .for_ca_file(std::path::Path::new(path.trim()))
                .map_err(|e| settings.invalid(format!("settings.caFile: {e}")))?,
            _ => client.clone(),
        };
        // Eigenes Zeitlimit für den Verbindungsaufbau (`timeouts.connectMs`, F187): Variante
        // mit eigenem Pool – nur wenn ausdrücklich gesetzt und abweichend.
        let connect_set = settings
            .object("timeouts")?
            .is_some_and(|o| o.contains_key("connectMs"));
        let client = if connect_set && timeouts.connect != client.outbound().connect_timeout() {
            client
                .for_connect_timeout(timeouts.connect)
                .map_err(|e| settings.invalid(format!("settings.timeouts.connectMs: {e}")))?
        } else {
            client
        };
        let mut up = Self::new(
            dep.deployment.metadata.name.clone(),
            provider,
            client,
            timeouts,
        );
        up.info.deployment_id = Some(dep.deployment.id);
        up.info.model_group = Some(dep.deployment.spec.model_group.clone());
        if let Some(p) = propagate {
            up.propagate_trace = p;
        }
        Ok(up)
    }

    /// `traceparent` an den Anbieter weitergeben (überschreibt den Standard der Anbieterart).
    #[must_use]
    pub fn with_trace_propagation(mut self, on: bool) -> Self {
        self.propagate_trace = on;
        self
    }

    /// Wird der Trace-Kontext an den Anbieter weitergegeben (F98)?
    pub fn propagates_trace(&self) -> bool {
        self.propagate_trace
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
        let Some(fut) = self.provider.prepare(&self.client) else {
            return Ok(());
        };
        tokio::time::timeout_at(deadline, fut)
            .await
            .unwrap_or_else(|_| Err(timeout_error("Anmeldung beim Anbieter")))
    }

    fn build(&self, mut req: ChatRequest) -> Result<crate::UpstreamRequest, UpstreamError> {
        let extra_headers = std::mem::take(&mut req.hints.upstream_headers);
        let mut http_req = self.build_with(|p, losses| p.build_request(req, losses))?;
        // Header aus Plugins (AIGW-36); Header des Adapters (Auth, Signatur, Format) haben
        // Vorrang, den Trace-Kontext setzt `send` (AIGW-45) – Plugins nie.
        if !extra_headers.is_empty() {
            let own = http_req.headers().clone();
            for (name, value) in extra_headers {
                if let (Ok(n), Ok(v)) = (
                    http::HeaderName::from_bytes(name.as_bytes()),
                    http::HeaderValue::from_str(&value),
                ) && !own.contains_key(&n)
                    && n != "traceparent"
                    && n != "tracestate"
                {
                    http_req.headers_mut().append(n, v);
                }
            }
        }
        Ok(http_req)
    }

    fn build_with<T>(
        &self,
        f: impl FnOnce(&dyn Provider, &mut Losses) -> Result<T, ApiError>,
    ) -> Result<T, UpstreamError> {
        let mut losses = Losses::new();
        let http_req = f(self.provider.as_ref(), &mut losses).map_err(UpstreamError::from)?;
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
            gen_ai.provider.name = aigw_core::observe::gen_ai_provider_name(info.provider),
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
        // W3C-Trace-Kontext weitergeben (nur bei aktivem Tracing, `observe.otel.propagate` und
        // für diesen Anbieter freigegeben, F98).
        if self.propagate_trace
            && let Some(tp) = aigw_core::observe::current_traceparent()
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
            Ok(Err(e)) if e.is_blocked() => {
                // SSRF-Sperre nach der Namensauflösung (AIGW-72): Konfigurations-/Angriffsfall,
                // keine Wiederholung am selben Deployment.
                tracing::warn!(
                    deployment = %self.info.deployment,
                    error = %e,
                    "Upstream-Adresse durch SSRF-Regeln gesperrt"
                );
                Err(UpstreamError::new(
                    ErrorClass::Connect,
                    ApiError::new(
                        ErrorKind::BadGateway,
                        "Anbieter nicht erreichbar (Zieladresse nicht erlaubt)",
                    )
                    .with_code("upstream_address_blocked"),
                ))
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    deployment = %self.info.deployment,
                    error = %e,
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
        self.unary(http_req, deadline, |p, _, body| p.parse_response(body))
            .await
    }

    /// Sendet eine fertige Anfrage, liest die Antwort vollständig und parst sie.
    async fn unary<T>(
        &self,
        http_req: crate::UpstreamRequest,
        deadline: Instant,
        parse: impl FnOnce(&dyn Provider, &http::HeaderMap, &[u8]) -> Result<T, ApiError>,
    ) -> Result<T, UpstreamError> {
        let resp = self.send(http_req, deadline).await?;
        let (parts, body) = resp.into_parts();
        let body = read_limited(body, deadline, self.timeouts.idle, MAX_RESPONSE_BODY)
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
        parse(self.provider.as_ref(), &parts.headers, &body).map_err(parse_error)
    }

    /// Embeddings (ggf. mehrere Teilanfragen, z. B. Bedrock Titan: eine je Eingabe).
    pub async fn embeddings(
        &self,
        req: EmbeddingsRequest,
    ) -> Result<EmbeddingsResponse, UpstreamError> {
        let deadline = Instant::now() + self.timeouts.total;
        self.prepare(deadline).await?;
        let requests = self.build_with(|p, losses| p.build_embeddings(req, losses))?;
        let parse = |p: &dyn Provider, h: &http::HeaderMap, b: &[u8]| p.parse_embeddings(h, b);
        let mut parts = futures_util::stream::iter(
            requests.into_iter().map(|r| self.unary(r, deadline, parse)),
        )
        .buffered(EMBEDDING_CONCURRENCY);
        let mut merged: Option<EmbeddingsResponse> = None;
        while let Some(part) = parts.next().await {
            let part = part?;
            match merged.as_mut() {
                None => merged = Some(part),
                Some(m) => {
                    let offset = u32::try_from(m.data.len()).unwrap_or(u32::MAX);
                    m.merge(part, offset);
                }
            }
        }
        merged.ok_or_else(|| {
            UpstreamError::from(ApiError::new(
                ErrorKind::Internal,
                "Adapter erzeugte keine Embedding-Anfrage",
            ))
        })
    }

    /// Bilderzeugung.
    pub async fn images(&self, req: ImageRequest) -> Result<ImageResponse, UpstreamError> {
        let deadline = Instant::now() + self.timeouts.total;
        self.prepare(deadline).await?;
        let http_req = self.build_with(|p, losses| p.build_images(req, losses))?;
        self.unary(http_req, deadline, |p, _, body| p.parse_images(body))
            .await
    }

    /// Transkription/Übersetzung.
    pub async fn transcription(
        &self,
        req: TranscriptionRequest,
    ) -> Result<TranscriptionResponse, UpstreamError> {
        let deadline = Instant::now() + self.timeouts.total;
        self.prepare(deadline).await?;
        let format = req.format();
        let http_req = self.build_with(|p, losses| p.build_transcription(req, losses))?;
        self.unary(http_req, deadline, |p, _, body| {
            p.parse_transcription(format, body)
        })
        .await
    }

    /// Sprachausgabe: Fehler bis zu den Antwort-Headern als `Err`, danach werden die
    /// Audio-Bytes ohne Puffern (mit Backpressure und Zeitlimits) weitergereicht.
    pub async fn speech(&self, req: SpeechRequest) -> Result<SpeechStream, UpstreamError> {
        let start = Instant::now();
        let total = start + self.timeouts.total;
        let first_byte = total.min(start + self.timeouts.first_byte);
        self.prepare(first_byte).await?;
        let http_req = self.build_with(|p, losses| p.build_speech(req, losses))?;
        let resp = self.send(http_req, first_byte).await?;
        let content_type = resp
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_owned();
        Ok(SpeechStream {
            content_type,
            body: Box::pin(UpstreamBytes::new(
                resp.into_body(),
                total,
                self.timeouts.idle,
            )),
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

/// Nicht lesbare Erfolgsantwort → Protokollfehler (Inhaltsfilter bleiben Inhaltsfilter).
fn parse_error(e: ApiError) -> UpstreamError {
    // Inhaltsfilter in einer 200-Antwort (z. B. Gemini `promptFeedback.blockReason`).
    if e.code.as_deref() == Some("content_filter") {
        return UpstreamError::new(ErrorClass::ContentFilter, e);
    }
    // Vom Adapter abgelehnte Anfrage (nicht unterstützte Fähigkeit).
    if e.code.as_deref() == Some("unsupported_capability") {
        return UpstreamError::from(e);
    }
    if e.code.as_deref() == Some("upstream_protocol_error") {
        return UpstreamError::new(ErrorClass::Protocol, e);
    }
    UpstreamError::new(
        ErrorClass::Protocol,
        ApiError::new(
            ErrorKind::BadGateway,
            format!("Anbieter-Antwort nicht lesbar: {}", e.message),
        )
        .with_code("upstream_protocol_error"),
    )
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

/// Roher Byte-Strom über einem Upstream-Body (Zeitlimits wie bei Ereignisströmen).
struct UpstreamBytes {
    body: Option<Incoming>,
    timer: Pin<Box<Sleep>>,
    total: Instant,
    idle: Duration,
}

impl UpstreamBytes {
    fn new(body: Incoming, total: Instant, idle: Duration) -> Self {
        let first = total.min(Instant::now() + idle);
        Self {
            body: Some(body),
            timer: Box::pin(tokio::time::sleep_until(first)),
            total,
            idle,
        }
    }
}

impl Stream for UpstreamBytes {
    type Item = Result<Bytes, ApiError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            let Some(body) = this.body.as_mut() else {
                return Poll::Ready(None);
            };
            match Pin::new(body).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    let next = this.total.min(Instant::now() + this.idle);
                    this.timer.as_mut().reset(next);
                    if let Ok(data) = frame.into_data()
                        && !data.is_empty()
                    {
                        return Poll::Ready(Some(Ok(data)));
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    tracing::debug!(error = %error_chain(&e), "Upstream-Audio abgebrochen");
                    this.body = None;
                    return Poll::Ready(Some(Err(ApiError::new(
                        ErrorKind::BadGateway,
                        "Verbindung zum Anbieter mitten im Stream abgebrochen",
                    )
                    .with_code("upstream_stream_interrupted"))));
                }
                Poll::Ready(None) => {
                    this.body = None;
                    return Poll::Ready(None);
                }
                Poll::Pending => {
                    ready!(this.timer.as_mut().poll(cx));
                    this.body = None;
                    return Poll::Ready(Some(Err(ApiError::new(
                        ErrorKind::Timeout,
                        "Zeitlimit beim Anbieter überschritten (Audio-Stream)",
                    )
                    .with_code("upstream_timeout"))));
                }
            }
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
