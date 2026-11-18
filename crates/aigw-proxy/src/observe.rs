// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Erfassung der Kennzahlen je Anfrage (Usage, TTFT, Dauer, Fehlerklasse, Abbruch) und
//! Übergabe an den [`RequestObserver`].
//!
//! Der [`Recorder`] liefert genau einen [`RequestRecord`]: regulär am Ende (Antwort gesendet
//! bzw. Stream zu Ende) oder – wenn er vorher fallen gelassen wird, weil der Client die
//! Verbindung geschlossen hat – aus `Drop` mit `client_aborted = true`.
//!
//! Payload-Mitschnitt nur, wenn [`RequestObserver::payload_mode`] es verlangt: Request-Header
//! und -Body (ohne Kopie), JSON-Antwort bzw. bei Streams der zusammengesetzte Antworttext.
//!
//! Usage-Schätzung (fehlende oder unvollständige Usage, z. B. bei Abbruch): mit dem Tokenizer
//! der Modellgruppe (F46, aus der Zulassung der Limits): Eingabe = Prompt-Schätzung der
//! Reservierung, Ausgabe = Zählung der gestreamten Text-/Reasoning-/Tool-Fragmente (bis 64 KiB
//! gepuffert, darüber hochgerechnet). Ohne Zulassung ≈ 1 Token je 4 Bytes. Bekannte Werte des
//! Anbieters haben Vorrang.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use aigw_core::ir::{
    ChatRequest, ChatResponse, Content, PartKind, StreamEvent, UpstreamError, Usage,
};
use aigw_core::observe::{
    GuardrailHits, PluginCalls, RawPayload, RequestObserver, RequestRecord, error_type,
    gen_ai_system,
};
use aigw_core::wire::WireFormat;
use axum::http::HeaderMap;
use bytes::Bytes;
use futures_util::Stream;
use tracing::Span;

use crate::auth::AuthContext;
use crate::provider::{ByteStream, Describe, EventStream};
use crate::request_id::RequestStart;

/// Höchstens so viel Ausgabetext wird für die Usage-Schätzung gepuffert.
const OUTPUT_TEXT_MAX: usize = 64 * 1024;

/// Geschätzte Tokens zu einer Textlänge in Bytes.
pub(crate) fn estimate_tokens(bytes: usize) -> u64 {
    u64::try_from(bytes.div_ceil(4)).unwrap_or(u64::MAX)
}

fn content_bytes(c: &Content) -> usize {
    match c {
        Content::Text(t) => t.len(),
        Content::Parts(parts) => parts
            .iter()
            .map(|p| match &p.kind {
                PartKind::Text { text }
                | PartKind::Reasoning { text, .. }
                | PartKind::Refusal { text } => text.len(),
                PartKind::ToolCall {
                    name, arguments, ..
                } => name.len() + arguments.len(),
                PartKind::ToolResult { content, .. } => content_bytes(content),
                _ => 0,
            })
            .sum(),
    }
}

/// Hängt `text` an, solange der Puffer [`OUTPUT_TEXT_MAX`] nicht überschreitet (an einer
/// Zeichengrenze abgeschnitten).
fn push_capped(buf: &mut String, text: &str) {
    let room = OUTPUT_TEXT_MAX.saturating_sub(buf.len());
    if room == 0 {
        return;
    }
    if text.len() <= room {
        buf.push_str(text);
    } else {
        let mut end = room;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        buf.push_str(text.get(..end).unwrap_or_default());
    }
}

/// Text eines Inhalts (für die Ausgabe-Schätzung).
fn push_content(buf: &mut String, c: &Content) {
    match c {
        Content::Text(t) => push_capped(buf, t),
        Content::Parts(parts) => {
            for p in parts {
                match &p.kind {
                    PartKind::Text { text }
                    | PartKind::Reasoning { text, .. }
                    | PartKind::Refusal { text } => push_capped(buf, text),
                    PartKind::ToolCall {
                        name, arguments, ..
                    } => {
                        push_capped(buf, name);
                        push_capped(buf, arguments);
                    }
                    PartKind::ToolResult { content, .. } => push_content(buf, content),
                    _ => {}
                }
            }
        }
    }
}

/// Textumfang einer Anfrage in Bytes (Grundlage der Eingabe-Schätzung).
pub(crate) fn request_bytes(req: &ChatRequest) -> usize {
    req.messages.iter().map(|m| content_bytes(&m.content)).sum()
}

/// Textumfang einer Antwort in Bytes (Grundlage der Ausgabe-Schätzung).
pub(crate) fn response_bytes(resp: &ChatResponse) -> usize {
    resp.choices
        .iter()
        .map(|c| content_bytes(&c.message.content))
        .sum()
}

/// Sammelt die Kennzahlen einer Anfrage.
pub(crate) struct Recorder {
    observer: Arc<dyn RequestObserver>,
    record: Option<RequestRecord>,
    start: Instant,
    input_bytes: usize,
    output_bytes: usize,
    /// Ausgabetext für die Usage-Schätzung mit dem Tokenizer (bis [`OUTPUT_TEXT_MAX`]).
    output_text: String,
    usage: Option<Usage>,
    /// Usage gilt als vollständig (nach dem Endgrund bzw. aus einer vollständigen Antwort).
    usage_final: bool,
    stopped: bool,
    /// Payload-Mitschnitt (nur wenn der Beobachter ihn verlangt).
    payload: Option<Box<RawPayload>>,
    /// Gestreamter Antworttext (nur bei Payload-Mitschnitt).
    streamed_text: Option<String>,
    /// Wurzel-Span der Anfrage (OpenTelemetry, AIGW-45; `Span::none()`, wenn Tracing aus ist).
    span: Span,
    /// Reservierung der Limits; am Ende Kosten, Budget und Abgleich (AIGW-30/31).
    admission: Option<crate::limits::Admission>,
    /// Antwort aus dem Cache (AIGW-39/40): Usage bleibt 0, Usage-Ereignisse zählen nicht.
    cache_hit: bool,
    /// Erkennungen der Guardrail-Plugins (AIGW-37/38), geteilt mit der Plugin-Kette.
    guardrails: Option<GuardrailHits>,
    /// Modellaufrufe von Plugins (AIGW-89/S-13), geteilt mit der Plugin-Kette.
    plugin_calls: Option<PluginCalls>,
}

impl Recorder {
    pub(crate) fn new(
        observer: Arc<dyn RequestObserver>,
        request_id: String,
        start: RequestStart,
        ingress: WireFormat,
    ) -> Self {
        let mut record = RequestRecord::new(request_id, ingress, 200);
        record.started_at = start.at;
        observer.request_started();
        Self {
            observer,
            record: Some(record),
            start: start.instant,
            input_bytes: 0,
            output_bytes: 0,
            output_text: String::new(),
            usage: None,
            usage_final: false,
            stopped: false,
            // Erst nach der Schlüsselprüfung bekannt ([`Recorder::set_key`], F30).
            payload: None,
            streamed_text: None,
            span: Span::none(),
            admission: None,
            cache_hit: false,
            guardrails: None,
            plugin_calls: None,
        }
    }

    /// Wurzel-Span, an dem Ergebnis-Attribute und das TTFT-Ereignis landen; dessen Trace-ID
    /// landet im Request-Log (F100).
    pub(crate) fn set_span(&mut self, span: Span) {
        if !span.is_disabled() {
            let trace_id = span.in_scope(aigw_core::observe::current_trace_id);
            if let Some(r) = self.rec() {
                r.trace_id = trace_id;
            }
        }
        self.span = span;
    }

    fn rec(&mut self) -> Option<&mut RequestRecord> {
        self.record.as_mut()
    }

    /// Wirksamer Payload-Modus dieser Anfrage (nach der Schlüsselprüfung).
    pub(crate) fn payload_mode(&self) -> aigw_core::observe::PayloadMode {
        self.payload
            .as_ref()
            .map_or(aigw_core::observe::PayloadMode::Off, |p| p.mode)
    }

    /// `true`, wenn Request/Antwort mitgeschnitten werden sollen.
    pub(crate) fn captures(&self) -> bool {
        self.payload.is_some()
    }

    /// Request-Header und -Body für den Payload-Mitschnitt (`Bytes` ohne Kopie).
    pub(crate) fn set_request_payload(&mut self, headers: &HeaderMap, body: &Bytes) {
        if let Some(p) = self.payload.as_mut() {
            p.request_headers = headers
                .iter()
                .map(|(k, v)| {
                    (
                        k.as_str().to_owned(),
                        String::from_utf8_lossy(v.as_bytes()).into_owned(),
                    )
                })
                .collect();
            p.request_body = body.clone();
        }
    }

    /// Antwort-Body (JSON) für den Payload-Mitschnitt.
    pub(crate) fn set_response_payload(&mut self, body: Bytes) {
        if let Some(p) = self.payload.as_mut() {
            p.response_body = body;
        }
    }

    /// Vom Client mitgegebene Request-ID (`x-client-request-id`, sonst `x-request-id`; F11) –
    /// im Request-Log suchbar und als Span-Attribut `aigw.client_request_id`. Der Writer kürzt
    /// und filtert sie.
    pub(crate) fn set_client_request_id(&mut self, headers: &HeaderMap) {
        let Some(id) = ["x-client-request-id", "x-request-id"]
            .iter()
            .find_map(|h| headers.get(*h))
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
        else {
            return;
        };
        let id: String = id.chars().take(256).collect();
        if !self.span.is_disabled() {
            self.span.record("aigw.client_request_id", id.as_str());
        }
        if let Some(r) = self.rec() {
            r.client_request_id = Some(id);
        }
    }

    /// Key, Team und Organisation aus der Schlüsselprüfung; entscheidet über den
    /// Payload-Mitschnitt (`payloadMode` an Key/Team/Organisation, sonst global; F30).
    pub(crate) fn set_key(&mut self, auth: &AuthContext) {
        let mode = self.observer.payload_mode(auth.key.as_deref());
        self.payload = mode.captures().then(|| {
            Box::new(RawPayload {
                mode,
                ..RawPayload::default()
            })
        });
        if let Some(r) = self.rec() {
            r.api_key_id = auth.api_key_id();
            r.team_id = auth.team_id();
            r.organization_id = auth.organization_id();
        }
    }

    /// Sammelstellen der Guardrail-Erkennungen und der Plugin-Modellaufrufe (nur bei
    /// Anfragen mit Plugins).
    pub(crate) fn set_plugin_sinks(&mut self, (hits, calls): (GuardrailHits, PluginCalls)) {
        self.guardrails = Some(hits);
        self.plugin_calls = Some(calls);
    }

    /// Zulassung durch die Limits; wird in [`Recorder::finish`] abgeschlossen.
    pub(crate) fn set_admission(&mut self, admission: crate::limits::Admission) {
        self.admission = Some(admission);
    }

    /// Zugelassene Anfrage (Nachreservierung nach den Pre-Plugins, S-38).
    pub(crate) fn admission_mut(&mut self) -> Option<&mut crate::limits::Admission> {
        self.admission.as_mut()
    }

    pub(crate) fn set_request(&mut self, req: &ChatRequest) {
        self.input_bytes = request_bytes(req);
        if req.is_stream() && self.payload.is_some() {
            self.streamed_text = Some(String::new());
        }
        if let Some(r) = self.rec() {
            r.model_alias = Some(req.model.clone());
            r.stream = req.is_stream();
        }
        if !self.span.is_disabled() {
            self.span
                .record("otel.name", format!("chat {}", req.model))
                .record("gen_ai.request.model", req.model.as_str())
                .record("aigw.stream", req.is_stream());
            if let Some(m) = req.max_tokens {
                self.span.record("gen_ai.request.max_tokens", m);
            }
        }
    }

    /// Anfrage eines anderen Endpunkts (Embeddings, Bilder, Audio; AIGW-21): Modell und
    /// Textumfang der Eingabe in Bytes (Grundlage einer Schätzung, falls der Anbieter keine
    /// Usage meldet).
    pub(crate) fn set_target(&mut self, model: &str, stream: bool, input_bytes: usize) {
        self.input_bytes = input_bytes;
        if let Some(r) = self.rec() {
            r.model_alias = Some(model.to_owned());
            r.stream = stream;
        }
        if !self.span.is_disabled() {
            self.span
                .record("gen_ai.request.model", model)
                .record("aigw.stream", stream);
        }
    }

    /// Usage einer Antwort anderer Endpunkte; `estimated` = Anbieter meldete keine Tokens
    /// (die Eingabe wird dann aus dem Textumfang geschätzt).
    pub(crate) fn set_usage(&mut self, usage: Usage, estimated: bool) {
        self.usage = Some(usage);
        self.usage_final = !estimated;
    }

    pub(crate) fn set_deployment(&mut self, d: Describe<'_>) {
        if let Some(r) = self.rec() {
            r.deployment_id = d.deployment_id;
            r.model_group = d.model_group.map(str::to_owned);
            r.provider = d.provider;
            r.provider_model = Some(d.provider_model.to_owned());
        }
        if !self.span.is_disabled() {
            self.span.record("aigw.deployment", d.deployment);
            if let Some(g) = d.model_group {
                self.span.record("aigw.model_group", g);
            }
            if let Some(p) = d.provider {
                self.span.record("gen_ai.system", gen_ai_system(p));
                self.span.record(
                    "gen_ai.provider.name",
                    aigw_core::observe::gen_ai_provider_name(p),
                );
            }
        }
    }

    /// Versuche (Retries/Fallbacks) übernehmen.
    pub(crate) fn set_attempts(&mut self, attempts: Vec<aigw_core::observe::Attempt>) {
        if !self.span.is_disabled() {
            self.span.record("aigw.attempts", attempts.len());
        }
        if let Some(r) = self.rec() {
            r.attempts = attempts;
        }
    }

    /// Nicht gestreamte Antwort: Usage übernehmen bzw. aus dem Antworttext schätzen.
    pub(crate) fn set_response(&mut self, resp: &ChatResponse) {
        self.span
            .record("gen_ai.response.model", resp.model.as_str());
        if self.cache_hit {
            return;
        }
        match &resp.usage {
            Some(u) => {
                self.usage = Some(u.clone());
                self.usage_final = true;
            }
            None => {
                self.output_bytes = response_bytes(resp);
                for c in &resp.choices {
                    push_content(&mut self.output_text, &c.message.content);
                }
            }
        }
    }

    /// Antwort eines Pre-Plugins (`Respond`, AIGW-36): kein Upstream-Verbrauch. Es zählt nur
    /// die Usage, die das Plugin selbst angibt (sonst 0) – die Token-Reservierung der Limits
    /// wird damit im Abschluss vollständig freigegeben, Kosten fallen keine an.
    pub(crate) fn set_plugin_response(&mut self, resp: &ChatResponse) {
        self.span
            .record("gen_ai.response.model", resp.model.as_str());
        self.usage = Some(resp.usage.clone().unwrap_or_default());
        self.usage_final = true;
    }

    /// Ergebnis des Antwort-Caches (AIGW-39/40).
    pub(crate) fn set_cache(&mut self, outcome: aigw_core::observe::CacheOutcome) {
        self.span.record("aigw.cache", outcome.as_str());
        if let Some(r) = self.rec() {
            r.cache = Some(outcome);
        }
    }

    /// Antwort aus dem Cache: kein Upstream-Verbrauch (Usage 0, keine Kosten); die
    /// Token-Reservierung der Limits wird im Abschluss vollständig freigegeben. Gezählt wird
    /// die Anfrage (Request-Limits, Log, Rollup `cache_hits`).
    pub(crate) fn set_cache_hit(&mut self, model_group: &str, resp: &ChatResponse) {
        self.span
            .record("gen_ai.response.model", resp.model.as_str());
        self.cache_hit = true;
        self.usage = Some(Usage::default());
        self.usage_final = true;
        if let Some(r) = self.rec() {
            r.model_group = Some(model_group.to_owned());
        }
    }

    /// Treffer im Embedding-Cache (F174): wie [`Recorder::set_cache_hit`] ohne Chat-Antwort.
    pub(crate) fn set_cache_hit_group(&mut self, model_group: &str) {
        self.cache_hit = true;
        self.usage = Some(Usage::default());
        self.usage_final = true;
        if let Some(r) = self.rec() {
            r.model_group = Some(model_group.to_owned());
        }
    }

    /// MCP-Tool-Aufruf (AIGW-41): Server, Tool, Ergebnis; `model_requested` im Request-Log
    /// ist der Name im Namensraum (`<server>__<tool>`).
    pub(crate) fn set_mcp(
        &mut self,
        server: &str,
        tool: &str,
        outcome: aigw_core::observe::McpOutcome,
    ) {
        if let Some(r) = self.rec() {
            r.model_alias = Some(format!("{server}{}{tool}", aigw_core::mcp::NAMESPACE_SEP));
            r.mcp = Some(Box::new(aigw_core::observe::McpCall {
                server: server.to_owned(),
                tool: tool.to_owned(),
                outcome,
            }));
        }
        if !self.span.is_disabled() {
            self.span.record(
                "otel.name",
                format!(
                    "mcp tools/call {server}{}{tool}",
                    aigw_core::mcp::NAMESPACE_SEP
                ),
            );
        }
    }

    /// Ergebnis eines MCP-Tool-Aufrufs: Ergebnisklasse, Statusäquivalent, Fehlerart.
    pub(crate) fn set_mcp_outcome(
        &mut self,
        outcome: aigw_core::observe::McpOutcome,
        status: u16,
        error: Option<aigw_core::ir::ErrorKind>,
    ) {
        if let Some(r) = self.rec() {
            r.status = status;
            r.error_kind = error;
            if let Some(m) = r.mcp.as_mut() {
                m.outcome = outcome;
            }
        }
    }

    /// Fehler vor dem ersten Byte.
    pub(crate) fn fail(&mut self, status: u16, err: &UpstreamError) {
        if let Some(r) = self.rec() {
            r.status = status;
            r.error_kind = Some(err.error.kind);
            r.error_class = Some(err.class);
        }
        // Ohne Antwort wurde nichts erzeugt; Eingabe-Usage wird nicht geschätzt.
        self.input_bytes = 0;
    }

    /// Ereignis eines Streams (vor dem Kodieren).
    fn observe(&mut self, ev: &StreamEvent) {
        match ev {
            StreamEvent::TextDelta { text, .. } => {
                self.first_token();
                self.output_bytes += text.len();
                push_capped(&mut self.output_text, text);
                if let Some(t) = self.streamed_text.as_mut() {
                    t.push_str(text);
                }
            }
            StreamEvent::ReasoningDelta { text, .. } => {
                self.first_token();
                self.output_bytes += text.len();
                push_capped(&mut self.output_text, text);
            }
            StreamEvent::ToolCallStart(t) => {
                self.first_token();
                self.output_bytes += t.name.len();
                push_capped(&mut self.output_text, &t.name);
            }
            StreamEvent::ToolCallDelta { arguments, .. } => {
                self.first_token();
                self.output_bytes += arguments.len();
                push_capped(&mut self.output_text, arguments);
            }
            StreamEvent::RedactedReasoning { .. } | StreamEvent::ReasoningSignature { .. } => {
                self.first_token();
            }
            StreamEvent::Stop { .. } => self.stopped = true,
            StreamEvent::Usage(_) if self.cache_hit => {}
            StreamEvent::Usage(u) => {
                self.usage = Some(u.clone());
                self.usage_final = self.stopped;
            }
            StreamEvent::Error(e) => {
                if let Some(r) = self.rec() {
                    r.error_kind = Some(e.kind);
                }
            }
            StreamEvent::Start { model, .. } => {
                self.span.record("gen_ai.response.model", model.as_str());
            }
            StreamEvent::Choice { .. } => {}
        }
    }

    fn first_token(&mut self) {
        let elapsed = self.start.elapsed();
        if let Some(r) = self.record.as_mut()
            && r.ttft.is_none()
        {
            r.ttft = Some(elapsed);
            let ttft_ms = elapsed.as_secs_f64() * 1000.0;
            self.span.record("aigw.ttft_ms", ttft_ms);
            tracing::event!(
                target: aigw_core::observe::TRACE_TARGET,
                parent: &self.span,
                tracing::Level::INFO,
                aigw.ttft_ms = ttft_ms,
                "first_token"
            );
        }
    }

    /// Schließt ab und übergibt den Datensatz (nur beim ersten Aufruf).
    pub(crate) fn finish(&mut self, client_aborted: bool) {
        let Some(mut r) = self.record.take() else {
            return;
        };
        r.client_aborted = client_aborted;
        r.duration = self.start.elapsed();
        let (usage, estimated) = match (self.usage.take(), self.usage_final) {
            (Some(u), true) => (u, false),
            (None, _) if self.input_bytes == 0 && self.output_bytes == 0 => {
                (Usage::default(), false)
            }
            (u, _) => {
                let mut u = u.unwrap_or_default();
                // Tokenizer der Modellgruppe aus der Zulassung (F46), sonst ≈ Bytes/4.
                let adm = self.admission.as_ref();
                if u.input_tokens == 0 && self.input_bytes > 0 {
                    u.input_tokens = adm.map_or_else(
                        || estimate_tokens(self.input_bytes),
                        crate::limits::Admission::input_estimate,
                    );
                }
                let out = match adm {
                    Some(a) if self.output_bytes > 0 => crate::limits::output_tokens(
                        a.tokenizer(),
                        &self.output_text,
                        self.output_bytes,
                    ),
                    _ => estimate_tokens(self.output_bytes),
                };
                u.output_tokens = u.output_tokens.max(out);
                (u, true)
            }
        };
        r.usage = usage;
        r.usage_estimated = estimated;
        if r.error_class.is_none() && r.is_error() {
            // F47: feine Klasse – letzter fehlgeschlagener Versuch, sonst aus der Fehlerart.
            r.error_class = r
                .attempts
                .last()
                .and_then(|a| a.error)
                .or_else(|| r.error_kind.map(aigw_core::ir::ErrorClass::from_kind));
        }
        let plugin_calls = self
            .plugin_calls
            .take()
            .map(|c| c.take())
            .unwrap_or_default();
        for c in &plugin_calls {
            r.plugin_usage.add(&c.usage);
        }
        if let Some(a) = self.admission.take() {
            a.complete(&mut r, &plugin_calls);
        }
        if let Some(h) = self.guardrails.take() {
            r.guardrails = h.take();
            if !r.guardrails.is_empty() && !self.span.is_disabled() {
                let n: u32 = r.guardrails.iter().map(|h| h.count).sum();
                self.span.record("aigw.guardrail_hits", n);
            }
        }
        if !self.span.is_disabled() {
            let s = &self.span;
            s.record("http.response.status_code", r.status)
                .record("gen_ai.usage.input_tokens", r.usage.input_tokens)
                .record("gen_ai.usage.output_tokens", r.usage.output_tokens)
                .record("aigw.usage_estimated", r.usage_estimated)
                .record("aigw.client_aborted", r.client_aborted);
            if let Some(k) = r.error_kind {
                s.record("error.type", error_type(k));
            }
            if r.is_error() {
                s.record("otel.status_code", "error");
            }
        }
        if let Some(mut p) = self.payload.take() {
            if let Some(t) = self.streamed_text.take() {
                p.response_body = Bytes::from(t);
            }
            r.payload = Some(p);
        }
        self.observer.record(r);
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        // Noch nicht abgeschlossen → der Handler bzw. Body-Stream wurde verworfen, weil der
        // Client die Verbindung geschlossen hat.
        self.finish(true);
    }
}

/// Byte-Strom (Sprachausgabe), der am Ende (oder beim Verwerfen) den Datensatz abgibt.
pub(crate) struct ObservedBytes {
    inner: ByteStream,
    rec: Recorder,
    first: bool,
}

impl ObservedBytes {
    pub(crate) fn new(inner: ByteStream, rec: Recorder) -> Self {
        Self {
            inner,
            rec,
            first: true,
        }
    }
}

impl Stream for ObservedBytes {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(b))) => {
                if std::mem::take(&mut this.first) {
                    this.rec.first_token();
                }
                Poll::Ready(Some(Ok(b)))
            }
            Poll::Ready(Some(Err(e))) => {
                // Mitten im Audio-Strom: Verbindung zum Client abbrechen (kein Fehlerformat
                // möglich), Fehlerklasse im Request-Log.
                if let Some(r) = this.rec.rec() {
                    r.error_kind = Some(e.kind);
                }
                this.rec.finish(false);
                Poll::Ready(Some(Err(std::io::Error::other(e.message))))
            }
            Poll::Ready(None) => {
                this.rec.finish(false);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Ereignisstrom, der unterwegs Kennzahlen sammelt und am Ende (oder beim Verwerfen) den
/// Datensatz abgibt.
pub(crate) struct ObservedStream {
    inner: EventStream,
    rec: Recorder,
}

impl ObservedStream {
    pub(crate) fn new(inner: EventStream, rec: Recorder) -> Self {
        Self { inner, rec }
    }
}

impl Stream for ObservedStream {
    type Item = StreamEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<StreamEvent>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(ev)) => {
                this.rec.observe(&ev);
                Poll::Ready(Some(ev))
            }
            Poll::Ready(None) => {
                this.rec.finish(false);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
