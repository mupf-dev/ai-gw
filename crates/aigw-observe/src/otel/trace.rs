// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! `tracing`-Layer, der Spans mit `target: "aigw::otel"` als OTLP-Spans aufzeichnet.
//!
//! - **Wurzel-Span** (kein übergeordneter Otel-Span): übernimmt einen gültigen eingehenden
//!   W3C-`traceparent` aus dem Feld `otel.traceparent` (Trace-ID, Eltern-Span, ggf.
//!   Sampling-Flag) oder beginnt einen neuen Trace. Die Sampling-Entscheidung fällt einmal
//!   hier (Kopf-Sampling) und gilt für alle Kind-Spans.
//! - **Nicht gesampelte** Spans tragen nur ihren Kontext (für die Weitergabe von
//!   `traceparent` mit Flag `00`), sammeln keine Attribute und werden nicht exportiert.
//! - **Fertige Spans** gehen per `try_send` in einen begrenzten Kanal; voll → verwerfen und
//!   `aigw_otlp_spans_dropped_total` erhöhen. Ein Hintergrund-Task exportiert in Batches.
//!
//! Ist Tracing nicht aktiviert, sind die Callsites für diesen Layer abgeschaltet: Der Proxy
//! bekommt `Span::none()` und zahlt nur die Prüfung des Callsite-Interesses.

use std::cell::Cell;
use std::fmt;
use std::hash::{BuildHasher, RandomState};
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aigw_core::config::OtlpConfig;
use aigw_core::observe::TRACE_TARGET;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::Interest;
use tracing::{Event, Metadata, Subscriber};
use tracing_subscriber::layer::{Context, Filter, Layer, SubscriberExt as _};
use tracing_subscriber::registry::{LookupSpan, Registry};

use super::http::{OtlpClient, OtlpConfigError, Signal};
use super::proto::{
    AnyValue, EventData, KeyValue, Origin, SpanData, SpanKind, StatusCode, encode_traces,
};
use super::{OtlpMetrics, origin};

/// Unix-Nanosekunden jetzt.
pub(crate) fn now_unix_nano() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Schneller, nicht kryptografischer Zufall (splitmix64, Startwert je Thread aus `RandomState`).
fn random_u64() -> u64 {
    thread_local! {
        static STATE: Cell<u64> = Cell::new(RandomState::new().hash_one(std::thread::current().id()));
    }
    STATE.with(|s| {
        let mut z = s.get().wrapping_add(0x9E37_79B9_7F4A_7C15);
        s.set(z);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    })
}

fn nonzero_u64() -> u64 {
    loop {
        let v = random_u64();
        if v != 0 {
            return v;
        }
    }
}

fn new_trace_id() -> [u8; 16] {
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(&random_u64().to_be_bytes());
    id[8..].copy_from_slice(&nonzero_u64().to_be_bytes());
    id
}

fn new_span_id() -> [u8; 8] {
    nonzero_u64().to_be_bytes()
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(DIGITS[usize::from(b >> 4)]));
        s.push(char::from(DIGITS[usize::from(b & 0xF)]));
    }
    s
}

fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    let b = s.as_bytes();
    if b.len() != N * 2 {
        return None;
    }
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let mut out = [0u8; N];
    for (i, o) in out.iter_mut().enumerate() {
        *o = (digit(b[2 * i])? << 4) | digit(b[2 * i + 1])?;
    }
    Some(out)
}

/// Eingehender W3C-`traceparent` (Version 00; höhere Versionen werden nach Spezifikation
/// anhand der ersten vier Felder gelesen).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TraceParent {
    pub(crate) trace_id: [u8; 16],
    pub(crate) parent_id: [u8; 8],
    pub(crate) sampled: bool,
}

pub(crate) fn parse_traceparent(s: &str) -> Option<TraceParent> {
    let s = s.trim();
    let mut parts = s.split('-');
    let version = parts.next()?;
    let trace = parts.next()?;
    let parent = parts.next()?;
    let flags = parts.next()?;
    let version = unhex::<1>(version)?[0];
    if version == 0xff || (version == 0 && parts.next().is_some()) {
        return None;
    }
    let trace_id = unhex::<16>(trace)?;
    let parent_id = unhex::<8>(parent)?;
    let flags = unhex::<1>(flags)?[0];
    if trace_id == [0; 16] || parent_id == [0; 8] {
        return None;
    }
    Some(TraceParent {
        trace_id,
        parent_id,
        sampled: flags & 1 == 1,
    })
}

/// Kontext eines Spans (auch nicht gesampelter) in den Extensions der Registry.
#[derive(Debug, Clone, Copy)]
struct SpanCtx {
    trace_id: [u8; 16],
    span_id: [u8; 8],
    sampled: bool,
    propagate: bool,
}

impl SpanCtx {
    fn traceparent(&self) -> String {
        format!(
            "00-{}-{}-{:02x}",
            hex(&self.trace_id),
            hex(&self.span_id),
            u8::from(self.sampled)
        )
    }
}

/// Gesammelte Felder (Span oder Event).
#[derive(Debug, Default)]
struct Fields {
    name: Option<String>,
    kind: Option<SpanKind>,
    status: Option<StatusCode>,
    status_message: Option<String>,
    traceparent: Option<String>,
    message: Option<String>,
    attrs: Vec<KeyValue>,
}

impl Fields {
    fn set(&mut self, key: &str, value: AnyValue) {
        match self.attrs.iter_mut().find(|kv| kv.key == key) {
            Some(kv) => kv.value = value,
            None => self.attrs.push(KeyValue::new(key, value)),
        }
    }
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "otel.name" => self.name = Some(value.to_owned()),
            "otel.kind" => self.kind = Some(SpanKind::parse(value)),
            "otel.status_code" => {
                self.status = Some(if value.eq_ignore_ascii_case("error") {
                    StatusCode::Error
                } else if value.eq_ignore_ascii_case("ok") {
                    StatusCode::Ok
                } else {
                    StatusCode::Unset
                });
            }
            "otel.status_message" => self.status_message = Some(value.to_owned()),
            "otel.traceparent" => self.traceparent = Some(value.to_owned()),
            "message" => self.message = Some(value.to_owned()),
            name => self.set(name, AnyValue::Str(value.to_owned())),
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.record_str(field, &format!("{value:?}"));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.set(field.name(), AnyValue::Int(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.set(
            field.name(),
            AnyValue::Int(i64::try_from(value).unwrap_or(i64::MAX)),
        );
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.set(field.name(), AnyValue::Bool(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.set(field.name(), AnyValue::Double(value));
    }
}

/// Aufzeichnung eines gesampelten Spans bis zum Schließen.
#[derive(Debug)]
struct Building {
    parent_span_id: Option<[u8; 8]>,
    start: u64,
    fields: Fields,
    events: Vec<EventData>,
}

/// Aktiver Tracer (nach [`TraceActivator::activate`]).
#[derive(Debug)]
struct Tracer {
    ratio: f64,
    parent_based: bool,
    accept_traceparent: bool,
    propagate: bool,
    tx: mpsc::Sender<SpanData>,
    metrics: Arc<OtlpMetrics>,
}

impl Tracer {
    /// Deterministisch nach Trace-ID (untere 63 Bit), wie `TraceIdRatioBased`.
    fn sample(&self, trace_id: &[u8; 16]) -> bool {
        if self.ratio >= 1.0 {
            return true;
        }
        if self.ratio <= 0.0 {
            return false;
        }
        let mut low = [0u8; 8];
        low.copy_from_slice(&trace_id[8..]);
        let x = u64::from_be_bytes(low) >> 1;
        (x as f64) < self.ratio * (1u64 << 63) as f64
    }
}

type Shared = Arc<OnceLock<Arc<Tracer>>>;

/// Der Layer (zusammen mit [`OtelFilter`] über [`layer`] zu erzeugen).
#[derive(Clone)]
pub struct OtelLayer {
    shared: Shared,
}

impl fmt::Debug for OtelLayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OtelLayer")
            .field("active", &self.shared.get().is_some())
            .finish()
    }
}

/// Filter je Layer: nur `target: "aigw::otel"`, nur wenn aktiviert.
#[derive(Clone)]
pub struct OtelFilter {
    shared: Shared,
}

impl fmt::Debug for OtelFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OtelFilter").finish_non_exhaustive()
    }
}

fn is_otel(meta: &Metadata<'_>) -> bool {
    meta.target() == TRACE_TARGET
}

impl<S> Filter<S> for OtelFilter {
    fn enabled(&self, meta: &Metadata<'_>, _cx: &Context<'_, S>) -> bool {
        is_otel(meta) && self.shared.get().is_some()
    }

    fn callsite_enabled(&self, meta: &'static Metadata<'static>) -> Interest {
        // Inaktiv: `never` – zusammen mit dem Log-Filter (`aigw::otel=off`) ist die Callsite
        // dann ganz abgeschaltet (`Span::none()`, keine Registry-Einträge). `activate()` baut
        // den Interest-Cache neu auf.
        if is_otel(meta) && self.shared.get().is_some() {
            Interest::always()
        } else {
            Interest::never()
        }
    }
}

/// Schaltet den Layer scharf (einmal je Layer).
#[derive(Clone)]
pub struct TraceActivator {
    shared: Shared,
}

impl fmt::Debug for TraceActivator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TraceActivator")
            .field("active", &self.shared.get().is_some())
            .finish()
    }
}

/// Layer (mit Filter) und zugehöriger Aktivierer. Der Layer wird beim Start des Prozesses in
/// den `tracing`-Subscriber eingehängt (vor dem Laden der Konfiguration); exportiert wird erst
/// nach [`TraceActivator::activate`].
pub fn layer<S>() -> (impl Layer<S> + Send + Sync + 'static, TraceActivator)
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let shared: Shared = Arc::new(OnceLock::new());
    let layer = OtelLayer {
        shared: Arc::clone(&shared),
    };
    let filter = OtelFilter {
        shared: Arc::clone(&shared),
    };
    (
        <OtelLayer as Layer<S>>::with_filter(layer, filter),
        TraceActivator { shared },
    )
}

/// Registry mit dem Otel-Layer (für Tests ohne weitere Layer).
pub fn test_subscriber() -> (impl Subscriber + Send + Sync + 'static, TraceActivator) {
    let (layer, activator) = layer::<Registry>();
    (Registry::default().with(layer), activator)
}

/// Laufender Export-Task der Traces.
#[derive(Debug)]
pub struct TraceExporter {
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl TraceExporter {
    /// Exportiert die noch gepufferten Spans und beendet den Task (höchstens `timeout`).
    pub async fn shutdown(self, timeout: Duration) -> bool {
        let _ = self.shutdown.send(true);
        let mut task = self.task;
        match tokio::time::timeout(timeout, &mut task).await {
            Ok(_) => true,
            Err(_) => {
                task.abort();
                false
            }
        }
    }
}

impl TraceActivator {
    /// `true`, wenn Traces aufgezeichnet werden.
    pub fn is_active(&self) -> bool {
        self.shared.get().is_some()
    }

    /// Aktiviert Aufzeichnung und Export nach `cfg` (Abschnitt `observe.otlp`). Benötigt eine
    /// laufende Tokio-Runtime. Ein zweiter Aufruf ist ein Fehler.
    pub fn activate(
        &self,
        cfg: &OtlpConfig,
        metrics: Arc<OtlpMetrics>,
    ) -> Result<TraceExporter, OtlpConfigError> {
        let client = OtlpClient::new(cfg, Arc::clone(&metrics))?;
        let t = &cfg.traces;
        let (tx, rx) = mpsc::channel(t.queue_capacity.max(1));
        let tracer = Arc::new(Tracer {
            ratio: t.sampling_ratio.get(),
            parent_based: t.parent_based,
            accept_traceparent: t.accept_traceparent,
            propagate: t.propagate,
            tx,
            metrics: Arc::clone(&metrics),
        });
        self.shared
            .set(tracer)
            .map_err(|_| OtlpConfigError::Endpoint("Tracing ist bereits aktiviert".into()))?;
        // Für `aigw_core::observe::current_traceparent` (Upstream-Header).
        aigw_core::observe::set_traceparent_source(current_traceparent);
        tracing::callsite::rebuild_interest_cache();
        let (shutdown, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(run_exporter(
            rx,
            client,
            origin(cfg),
            t.batch_size.max(1),
            Duration::from_millis(t.flush_interval_ms.max(1)),
            metrics,
            shutdown_rx,
        ));
        Ok(TraceExporter { shutdown, task })
    }
}

/// `traceparent` des aktuellen Spans (Quelle für `aigw_core::observe::current_traceparent`).
fn current_traceparent() -> Option<String> {
    tracing::Span::current()
        .with_subscriber(|(id, dispatch)| {
            let registry = dispatch.downcast_ref::<Registry>()?;
            let span = registry.span(id)?;
            let ext = span.extensions();
            let ctx = ext.get::<SpanCtx>()?;
            ctx.propagate.then(|| ctx.traceparent())
        })
        .flatten()
}

impl<S> Layer<S> for OtelLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(tracer) = self.shared.get() else {
            return;
        };
        let Some(span) = ctx.span(id) else {
            return;
        };
        let parent = span
            .parent()
            .and_then(|p| p.extensions().get::<SpanCtx>().copied());
        let mut fields = Fields::default();
        let (trace_id, parent_span_id, sampled) = match parent {
            Some(p) => {
                if p.sampled {
                    attrs.record(&mut fields);
                }
                (p.trace_id, Some(p.span_id), p.sampled)
            }
            None => {
                attrs.record(&mut fields);
                let incoming = if tracer.accept_traceparent {
                    fields.traceparent.as_deref().and_then(parse_traceparent)
                } else {
                    None
                };
                match incoming {
                    Some(tp) => {
                        let sampled = if tracer.parent_based {
                            tp.sampled
                        } else {
                            tracer.sample(&tp.trace_id)
                        };
                        (tp.trace_id, Some(tp.parent_id), sampled)
                    }
                    None => {
                        let trace_id = new_trace_id();
                        (trace_id, None, tracer.sample(&trace_id))
                    }
                }
            }
        };
        let mut ext = span.extensions_mut();
        ext.insert(SpanCtx {
            trace_id,
            span_id: new_span_id(),
            sampled,
            propagate: tracer.propagate,
        });
        if sampled {
            ext.insert(Building {
                parent_span_id,
                start: now_unix_nano(),
                fields,
                events: Vec::new(),
            });
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id)
            && let Some(b) = span.extensions_mut().get_mut::<Building>()
        {
            values.record(&mut b.fields);
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.event_span(event) else {
            return;
        };
        let mut ext = span.extensions_mut();
        let Some(b) = ext.get_mut::<Building>() else {
            return;
        };
        let mut fields = Fields::default();
        event.record(&mut fields);
        b.events.push(EventData {
            time_unix_nano: now_unix_nano(),
            name: fields
                .message
                .unwrap_or_else(|| event.metadata().name().to_owned()),
            attributes: fields.attrs,
        });
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(tracer) = self.shared.get() else {
            return;
        };
        let Some(span) = ctx.span(&id) else {
            return;
        };
        let mut ext = span.extensions_mut();
        let Some(b) = ext.remove::<Building>() else {
            return;
        };
        let Some(sc) = ext.get_mut::<SpanCtx>().map(|c| *c) else {
            return;
        };
        let f = b.fields;
        let data = SpanData {
            trace_id: sc.trace_id,
            span_id: sc.span_id,
            parent_span_id: b.parent_span_id,
            name: f.name.unwrap_or_else(|| span.name().to_owned()),
            kind: f.kind.unwrap_or_default(),
            start_unix_nano: b.start,
            end_unix_nano: now_unix_nano(),
            attributes: f.attrs,
            events: b.events,
            status: f.status.unwrap_or_default(),
            status_message: f.status_message.unwrap_or_default(),
        };
        if tracer.tx.try_send(data).is_err() {
            tracer.metrics.spans_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

async fn export(client: &OtlpClient, origin: &Origin, buf: &mut Vec<SpanData>, m: &OtlpMetrics) {
    if buf.is_empty() {
        return;
    }
    let body = encode_traces(origin, buf);
    let n = buf.len() as u64;
    buf.clear();
    match client.post(Signal::Traces, body).await {
        Ok(()) => {
            m.spans_exported.fetch_add(n, Ordering::Relaxed);
        }
        Err(e) => {
            m.trace_export_errors.fetch_add(1, Ordering::Relaxed);
            m.spans_dropped.fetch_add(n, Ordering::Relaxed);
            tracing::warn!(error = %e, spans = n, "OTLP-Export der Traces fehlgeschlagen");
        }
    }
}

async fn run_exporter(
    mut rx: mpsc::Receiver<SpanData>,
    client: OtlpClient,
    origin: Origin,
    batch: usize,
    interval: Duration,
    metrics: Arc<OtlpMetrics>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut buf: Vec<SpanData> = Vec::with_capacity(batch);
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let room = batch.saturating_sub(buf.len()).max(1);
        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            n = rx.recv_many(&mut buf, room) => {
                if n == 0 {
                    break;
                }
                if buf.len() >= batch {
                    export(&client, &origin, &mut buf, &metrics).await;
                }
            }
            _ = tick.tick() => export(&client, &origin, &mut buf, &metrics).await,
        }
    }
    rx.close();
    while let Ok(s) = rx.try_recv() {
        buf.push(s);
        if buf.len() >= batch {
            export(&client, &origin, &mut buf, &metrics).await;
        }
    }
    export(&client, &origin, &mut buf, &metrics).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traceparent_roundtrip_and_validation() {
        let tp =
            parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01").unwrap();
        assert!(tp.sampled);
        assert_eq!(hex(&tp.trace_id), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(hex(&tp.parent_id), "00f067aa0ba902b7");
        assert!(
            !parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00")
                .unwrap()
                .sampled
        );
        for bad in [
            "",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7",
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
        ] {
            assert!(parse_traceparent(bad).is_none(), "{bad}");
        }
        // Zukünftige Version mit zusätzlichem Feld ist erlaubt.
        assert!(
            parse_traceparent("01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-x")
                .is_some()
        );
        let ctx = SpanCtx {
            trace_id: tp.trace_id,
            span_id: [1, 2, 3, 4, 5, 6, 7, 8],
            sampled: false,
            propagate: true,
        };
        assert_eq!(
            ctx.traceparent(),
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0102030405060708-00"
        );
    }

    #[test]
    fn ratio_sampling_is_deterministic_and_roughly_proportional() {
        let (tx, _rx) = mpsc::channel(1);
        let t = Tracer {
            ratio: 0.25,
            parent_based: true,
            accept_traceparent: true,
            propagate: true,
            tx,
            metrics: Arc::default(),
        };
        let ids: Vec<[u8; 16]> = (0..20_000).map(|_| new_trace_id()).collect();
        let hits = ids.iter().filter(|id| t.sample(id)).count();
        assert!((4000..6000).contains(&hits), "{hits}");
        assert!(ids.iter().all(|id| t.sample(id) == t.sample(id)));
    }
}
