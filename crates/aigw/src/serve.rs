// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Accept-Schleife und Verbindungsbetrieb mit Grenzen und Lese-Timeouts (S-32, AIGW-80).
//!
//! `axum::serve` kennt weder eine Verbindungsgrenze noch Lese-Timeouts; ein Client, der Kopf
//! oder Körper einer Anfrage Byte für Byte schickt (Slowloris), hielt eine Verbindung beliebig
//! lange. Hier deshalb eine eigene Schleife auf hyper-util (`auto`: HTTP/1 und HTTP/2):
//!
//! - **Verbindungsgrenze** je Listener (Semaphore, `try_acquire`): Über der Grenze wird die
//!   Verbindung angenommen, bekommt ohne zu warten `503` (nicht blockierendes `try_write`) und
//!   wird geschlossen. Die Accept-Schleife blockiert nie; die Kernel-Warteschlange (4096,
//!   AIGW-57) bleibt also leer.
//! - **Kopf-Timeout** (HTTP/1): ab dem ersten Byte einer Anfrage bis zum vollständigen Kopf.
//!   Gemessen von einer Wache je Verbindung über die gelesenen Bytes (nicht mit hypers
//!   `header_read_timeout`, der schon im Keep-Alive-Leerlauf läuft und damit Keep-Alive auf
//!   den Kopf-Timeout verkürzen würde; hypers Timer bleibt als Rückfallnetz mit
//!   Kopf- + Leerlauf-Timeout gesetzt).
//! - **Körper-Timeout**: höchste Pause zwischen zwei Teilen und Mindestrate ([`TimedBody`]);
//!   bei Verletzung wird die Verbindung sofort geschlossen.
//! - **Leerlauf** (HTTP/1 und HTTP/2): Verbindung ohne laufende Anfrage und ohne begonnenen
//!   Kopf wird nach `idle_timeout` geordnet geschlossen (HTTP/2: GOAWAY).
//! - **HTTP/2**: höchstens N gleichzeitige Streams, PING-Keepalive gegen tote Gegenstellen.
//!
//! Antworten laufen ohne Lese-Timeout: Solange eine Antwort (auch ein langer SSE-Stream) läuft,
//! zählt die Verbindung als beschäftigt ([`ResponseBody`] hält den Zähler bis zum Ende).
//! TLS terminiert ai-gw nicht (Reverse Proxy, docs/betrieb/haertung.md), daher gibt es keinen
//! TLS-Handshake-Timeout.

use std::convert::Infallible;
use std::fmt::Write as _;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use aigw_core::config::HttpServerConfig;
use axum::Router;
use bytes::Bytes;
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, Semaphore, watch};
use tokio::time::{Instant, Sleep};
use tower::ServiceExt as _;

/// Grenzen und Timeouts einer Verbindung (aus `server.http`).
#[derive(Debug, Clone)]
pub(crate) struct HttpLimits {
    pub(crate) header_read_timeout: Option<Duration>,
    pub(crate) body_read_timeout: Option<Duration>,
    pub(crate) body_min_bytes_per_sec: u64,
    pub(crate) idle_timeout: Option<Duration>,
    pub(crate) http2_max_concurrent_streams: u32,
    pub(crate) http2_keepalive_interval: Option<Duration>,
    pub(crate) http2_keepalive_timeout: Duration,
}

fn secs(v: u64) -> Option<Duration> {
    (v > 0).then(|| Duration::from_secs(v))
}

impl From<&HttpServerConfig> for HttpLimits {
    fn from(c: &HttpServerConfig) -> Self {
        Self {
            header_read_timeout: secs(c.header_read_timeout_secs),
            body_read_timeout: secs(c.body_read_timeout_secs),
            body_min_bytes_per_sec: c.body_min_bytes_per_sec,
            idle_timeout: secs(c.idle_timeout_secs),
            http2_max_concurrent_streams: c.http2_max_concurrent_streams,
            http2_keepalive_interval: secs(c.http2_keepalive_interval_secs),
            http2_keepalive_timeout: Duration::from_secs(c.http2_keepalive_timeout_secs.max(1)),
        }
    }
}

impl HttpLimits {
    /// Prüfabstand der Wache: halber kleinster Timeout, 100 ms … 5 s. Kopf- und
    /// Leerlauf-Timeouts greifen damit höchstens um diesen Abstand verspätet.
    fn check_interval(&self) -> Option<Duration> {
        let min = match (self.header_read_timeout, self.idle_timeout) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => return None,
        };
        Some((min / 2).clamp(Duration::from_millis(100), Duration::from_secs(5)))
    }

    fn builder(&self) -> auto::Builder<TokioExecutor> {
        let mut b = auto::Builder::new(TokioExecutor::new());
        // Rückfallnetz: hypers Kopf-Timer läuft schon im Leerlauf (siehe Moduldoku), daher
        // Kopf- plus Leerlauf-Timeout; ohne beide aus (hyper setzt sonst 30 s).
        let backstop = match (self.header_read_timeout, self.idle_timeout) {
            (Some(h), Some(i)) => Some(h + i),
            _ => None,
        };
        b.http1()
            .timer(TokioTimer::new())
            .header_read_timeout(backstop)
            .keep_alive(true);
        b.http2()
            .timer(TokioTimer::new())
            .max_concurrent_streams(self.http2_max_concurrent_streams)
            .keep_alive_interval(self.http2_keepalive_interval)
            .keep_alive_timeout(self.http2_keepalive_timeout);
        b
    }
}

/// Zähler eines Listeners für `/metrics`.
#[derive(Debug, Default)]
pub(crate) struct ListenerMetrics {
    accepted: AtomicU64,
    active: AtomicU64,
    rejected: AtomicU64,
    timeout_header: AtomicU64,
    timeout_body: AtomicU64,
    timeout_idle: AtomicU64,
    timeout_keepalive: AtomicU64,
}

impl ListenerMetrics {
    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> [u64; 7] {
        [
            &self.accepted,
            &self.active,
            &self.rejected,
            &self.timeout_header,
            &self.timeout_body,
            &self.timeout_idle,
            &self.timeout_keepalive,
        ]
        .map(|c| c.load(Relaxed))
    }
}

/// Verbindungsmetriken beider Listener (S-32).
#[derive(Debug, Default)]
pub(crate) struct ServerMetrics {
    pub(crate) data: Arc<ListenerMetrics>,
    pub(crate) control: Arc<ListenerMetrics>,
}

impl ServerMetrics {
    /// Prometheus-Textformat: `aigw_server_*`.
    pub(crate) fn render_prometheus(&self, out: &mut String, with_control: bool) {
        let listeners: &[(&str, &ListenerMetrics)] = if with_control {
            &[("data", &self.data), ("control", &self.control)]
        } else {
            &[("data", &self.data)]
        };
        type Pick = fn(&ListenerMetrics) -> &AtomicU64;
        let simple: [(&str, &str, &str, Pick); 3] = [
            (
                "aigw_server_connections_accepted_total",
                "counter",
                "Angenommene Client-Verbindungen",
                |m| &m.accepted,
            ),
            (
                "aigw_server_connections_active",
                "gauge",
                "Offene Client-Verbindungen",
                |m| &m.active,
            ),
            (
                "aigw_server_connections_rejected_total",
                "counter",
                "Wegen server.*.max_connections sofort geschlossene Verbindungen (503)",
                |m| &m.rejected,
            ),
        ];
        for (name, kind, help, get) in simple {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
            for (l, m) in listeners {
                let _ = writeln!(out, "{name}{{listener=\"{l}\"}} {}", get(m).load(Relaxed));
            }
        }
        let name = "aigw_server_timeouts_total";
        let _ = writeln!(
            out,
            "# HELP {name} Wegen Timeout geschlossene Verbindungen (header: Anfragekopf, body: \
             Anfragekörper, idle: Keep-Alive-Leerlauf, keepalive: HTTP/2-PING unbeantwortet)\n\
             # TYPE {name} counter"
        );
        for (l, m) in listeners {
            for (kind, c) in [
                ("header", &m.timeout_header),
                ("body", &m.timeout_body),
                ("idle", &m.timeout_idle),
                ("keepalive", &m.timeout_keepalive),
            ] {
                let _ = writeln!(
                    out,
                    "{name}{{listener=\"{l}\",kind=\"{kind}\"}} {}",
                    c.load(Relaxed)
                );
            }
        }
    }
}

/// Betreibt `listener`, bis `shutdown` auf `true` steht; wartet dann auf alle Verbindungen
/// (laufende Anfragen und Streams werden abgeschlossen, Leerlauf-Verbindungen geschlossen).
pub(crate) async fn serve(
    listener: TcpListener,
    router: Router,
    max_connections: usize,
    limits: Arc<HttpLimits>,
    metrics: Arc<ListenerMetrics>,
    mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    let slots = (max_connections > 0).then(|| Arc::new(Semaphore::new(max_connections)));
    let builder = Arc::new(limits.builder());
    // Jede Verbindung hält einen Empfänger; `closed()` meldet, wenn alle beendet sind.
    let (done_tx, done_rx) = watch::channel(());
    loop {
        let accepted = tokio::select! {
            biased;
            () = stopped(&mut shutdown) => break,
            r = listener.accept() => r,
        };
        let stream = match accepted {
            Ok((s, _)) => s,
            Err(e) => {
                accept_error(e).await;
                continue;
            }
        };
        let permit = match &slots {
            None => None,
            Some(s) => match Arc::clone(s).try_acquire_owned() {
                Ok(p) => Some(p),
                Err(_) => {
                    metrics.rejected.fetch_add(1, Relaxed);
                    reject(&stream);
                    continue;
                }
            },
        };
        // TCP_NODELAY: SSE-Chunks sofort senden statt auf ACKs zu warten (Nagle, Q4).
        let _ = stream.set_nodelay(true);
        metrics.accepted.fetch_add(1, Relaxed);
        let conn = Conn {
            router: router.clone(),
            builder: Arc::clone(&builder),
            limits: Arc::clone(&limits),
            metrics: Arc::clone(&metrics),
            shutdown: shutdown.clone(),
        };
        let done = done_rx.clone();
        tokio::spawn(async move {
            conn.run(stream).await;
            drop(permit);
            drop(done);
        });
    }
    drop(listener);
    drop(done_rx);
    done_tx.closed().await;
    Ok(())
}

/// Wartet, bis das Herunterfahren beginnt (ohne `watch::Ref` über ein `await` zu halten).
async fn stopped(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|s| *s).await;
}

/// Antwort über der Verbindungsgrenze: ohne zu warten schreiben, dann schließen.
fn reject(stream: &TcpStream) {
    const RESPONSE: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\
                              retry-after: 1\r\nconnection: close\r\n\r\n";
    let _ = stream.try_write(RESPONSE);
}

/// Wie `axum::serve`: Verbindungsfehler ignorieren, sonst (z. B. `EMFILE`) kurz warten.
async fn accept_error(e: io::Error) {
    if matches!(
        e.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    ) {
        return;
    }
    tracing::error!(error = %e, "accept fehlgeschlagen");
    tokio::time::sleep(Duration::from_secs(1)).await;
}

/// Zustand einer Verbindung, geteilt zwischen Lesen ([`TrackedIo`]), Anfragen und Wache.
#[derive(Debug)]
struct ConnState {
    start: Instant,
    /// Laufende Anfragen (bis zum Ende des Antwortkörpers).
    in_flight: AtomicU32,
    /// HTTP/1: Es läuft keine Anfrage, das nächste gelesene Byte beginnt einen Kopf.
    awaiting_head: AtomicBool,
    /// Beginn des aktuellen Kopfs (ms seit `start` + 1; 0 = keiner).
    head_started: AtomicU64,
    /// Seit wann keine Anfrage mehr läuft (ms seit `start`).
    idle_since: AtomicU64,
    /// HTTP/2 erkannt (Präambel).
    h2: AtomicBool,
    /// Körper-Timeout: Verbindung sofort schließen.
    killed: AtomicBool,
    kill: Notify,
}

impl ConnState {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            in_flight: AtomicU32::new(0),
            awaiting_head: AtomicBool::new(true),
            head_started: AtomicU64::new(0),
            idle_since: AtomicU64::new(0),
            h2: AtomicBool::new(false),
            killed: AtomicBool::new(false),
            kill: Notify::new(),
        }
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn request_started(&self) {
        self.in_flight.fetch_add(1, Relaxed);
        self.awaiting_head.store(false, Relaxed);
        self.head_started.store(0, Relaxed);
    }

    fn request_finished(&self) {
        if self.in_flight.fetch_sub(1, Relaxed) == 1 {
            self.idle_since.store(self.now_ms(), Relaxed);
            self.awaiting_head.store(!self.h2.load(Relaxed), Relaxed);
        }
    }

    /// HTTP/2-Präambel erkannt.
    fn set_h2(&self) {
        self.h2.store(true, Relaxed);
        self.awaiting_head.store(false, Relaxed);
    }

    /// Nach jedem Lesen von Bytes (HTTP/1): ggf. Beginn eines Kopfs vermerken.
    fn on_read(&self) {
        if self.awaiting_head.load(Relaxed) && self.head_started.load(Relaxed) == 0 {
            self.head_started.store(self.now_ms() + 1, Relaxed);
        }
    }

    fn kill(&self) {
        self.killed.store(true, Relaxed);
        self.kill.notify_one();
    }

    /// Prüfung der Wache.
    fn check(&self, limits: &HttpLimits) -> Verdict {
        if self.in_flight.load(Relaxed) > 0 {
            return Verdict::Ok;
        }
        let now = self.now_ms();
        let head = self.head_started.load(Relaxed);
        if head != 0 {
            if let Some(t) = limits.header_read_timeout
                && now.saturating_sub(head - 1) >= ms(t)
            {
                return Verdict::HeaderTimeout;
            }
            return Verdict::Ok;
        }
        if let Some(t) = limits.idle_timeout
            && now.saturating_sub(self.idle_since.load(Relaxed)) >= ms(t)
        {
            return Verdict::Idle;
        }
        Verdict::Ok
    }
}

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

enum Verdict {
    Ok,
    HeaderTimeout,
    Idle,
}

/// Eine Verbindung samt Wache.
struct Conn {
    router: Router,
    builder: Arc<auto::Builder<TokioExecutor>>,
    limits: Arc<HttpLimits>,
    metrics: Arc<ListenerMetrics>,
    shutdown: watch::Receiver<bool>,
}

impl Conn {
    async fn run(mut self, stream: TcpStream) {
        self.metrics.active.fetch_add(1, Relaxed);
        let state = Arc::new(ConnState::new());
        let io = TokioIo::new(TrackedIo {
            inner: stream,
            state: Arc::clone(&state),
            proto: Proto::Unknown(0),
        });
        let svc = ConnService {
            router: self.router.clone(),
            state: Arc::clone(&state),
            limits: Arc::clone(&self.limits),
        };
        let conn = self.builder.serve_connection(io, svc);
        tokio::pin!(conn);
        let interval = self.limits.check_interval();
        let mut tick = interval.map(|i| tokio::time::interval_at(Instant::now() + i, i));
        if let Some(t) = &mut tick {
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        }
        let mut closing = false;
        loop {
            let mut check = false;
            tokio::select! {
                res = conn.as_mut() => {
                    // Von der Körper-Wache getötet: zählt unten als `body` (die Verbindung kann
                    // vor `kill.notified()` fertig werden – beide Zweige sind dann bereit).
                    if !state.killed.load(Relaxed)
                        && let Err(e) = res
                        && e.downcast_ref::<hyper::Error>().is_some_and(hyper::Error::is_timeout)
                    {
                        // HTTP/1: Rückfallnetz des Kopf-Timers; HTTP/2: PING unbeantwortet.
                        if state.h2.load(Relaxed) {
                            self.metrics.timeout_keepalive.fetch_add(1, Relaxed);
                        } else {
                            self.metrics.timeout_header.fetch_add(1, Relaxed);
                        }
                    }
                    break;
                }
                () = state.kill.notified() => {
                    if state.killed.load(Relaxed) {
                        break;
                    }
                }
                () = stopped(&mut self.shutdown), if !closing => {
                    conn.as_mut().graceful_shutdown();
                    closing = true;
                }
                _ = async {
                    match tick.as_mut() {
                        Some(t) => {
                            t.tick().await;
                        }
                        None => std::future::pending().await,
                    }
                } => check = true,
            }
            if !check {
                continue;
            }
            // Auch beim Herunterfahren: langsame Köpfe würden es sonst aufhalten.
            match state.check(&self.limits) {
                Verdict::HeaderTimeout => {
                    // Langsamer Kopf: hart schließen.
                    self.metrics.timeout_header.fetch_add(1, Relaxed);
                    break;
                }
                Verdict::Idle if !closing => {
                    self.metrics.timeout_idle.fetch_add(1, Relaxed);
                    conn.as_mut().graceful_shutdown();
                    closing = true;
                }
                Verdict::Ok | Verdict::Idle => {}
            }
        }
        if state.killed.load(Relaxed) {
            self.metrics.timeout_body.fetch_add(1, Relaxed);
        }
        self.metrics.active.fetch_sub(1, Relaxed);
    }
}

/// TCP-Strom, der gelesene Bytes an [`ConnState`] meldet (Kopfbeginn, HTTP/2-Erkennung).
struct TrackedIo {
    inner: TcpStream,
    state: Arc<ConnState>,
    proto: Proto,
}

/// Protokollerkennung an den ersten drei Bytes (`PRI` = HTTP/2-Präambel).
#[derive(Clone, Copy)]
enum Proto {
    /// So viele Bytes von `PRI` wurden schon gesehen.
    Unknown(usize),
    Http1,
    Http2,
}

impl Proto {
    fn feed(self, read: &[u8]) -> Self {
        let Self::Unknown(seen) = self else {
            return self;
        };
        const PRI: &[u8] = b"PRI";
        let n = read.len().min(PRI.len() - seen);
        if read[..n] != PRI[seen..seen + n] {
            Self::Http1
        } else if seen + n == PRI.len() {
            Self::Http2
        } else {
            Self::Unknown(seen + n)
        }
    }
}

impl AsyncRead for TrackedIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let this = &mut *self;
        ready!(Pin::new(&mut this.inner).poll_read(cx, buf))?;
        let read = &buf.filled()[before..];
        if !read.is_empty() {
            match this.proto {
                Proto::Http1 => this.state.on_read(),
                Proto::Http2 => {}
                Proto::Unknown(_) => {
                    this.proto = this.proto.feed(read);
                    match this.proto {
                        Proto::Http1 => this.state.on_read(),
                        Proto::Http2 => this.state.set_h2(),
                        Proto::Unknown(_) => {}
                    }
                }
            }
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for TrackedIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// hyper-Service je Verbindung: Router mit Körper-Timeout und Anfragezählung.
#[derive(Clone)]
struct ConnService {
    router: Router,
    state: Arc<ConnState>,
    limits: Arc<HttpLimits>,
}

type BoxFuture = Pin<Box<dyn Future<Output = Result<Response<ResponseBody>, Infallible>> + Send>>;

impl hyper::service::Service<Request<Incoming>> for ConnService {
    type Response = Response<ResponseBody>;
    type Error = Infallible;
    type Future = BoxFuture;

    fn call(&self, req: Request<Incoming>) -> Self::Future {
        self.state.request_started();
        let guard = InFlight(Arc::clone(&self.state));
        let req = req.map(|b| TimedBody::new(b, Arc::clone(&self.state), &self.limits));
        let fut = self.router.clone().oneshot(req);
        Box::pin(async move {
            let res = fut.await?;
            Ok(res.map(|inner| ResponseBody {
                inner,
                _guard: guard,
            }))
        })
    }
}

/// Zählt eine laufende Anfrage bis zum Drop (Ende des Antwortkörpers bzw. Abbruch).
struct InFlight(Arc<ConnState>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.request_finished();
    }
}

/// Antwortkörper, der die Anfrage bis zu seinem Ende als laufend zählt (Streams sind damit
/// vor Leerlauf- und Kopf-Timeouts geschützt).
pub(crate) struct ResponseBody {
    inner: axum::body::Body,
    _guard: InFlight,
}

impl Body for ResponseBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// Fehler bei Überschreitung des Körper-Timeouts.
#[derive(Debug)]
struct BodyTimeout;

impl std::fmt::Display for BodyTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Zeitüberschreitung beim Lesen des Anfragekörpers")
    }
}

impl std::error::Error for BodyTimeout {}

/// Anfragekörper mit Pausen-Timeout und Mindestrate. Die Uhr läuft nur, wenn der Körper
/// gelesen wird und gerade keine Daten bereitliegen (schneller Pfad ohne Zeitabfrage).
struct TimedBody {
    inner: Incoming,
    state: Arc<ConnState>,
    gap: Option<Duration>,
    min_rate: u64,
    /// Erstes Warten auf Daten.
    started: Option<Instant>,
    received: u64,
    /// Nach neuen Daten muss die Frist neu berechnet werden.
    rearm: bool,
    sleep: Option<Pin<Box<Sleep>>>,
}

impl TimedBody {
    fn new(inner: Incoming, state: Arc<ConnState>, limits: &HttpLimits) -> Self {
        Self {
            inner,
            state,
            gap: limits.body_read_timeout,
            min_rate: limits.body_min_bytes_per_sec,
            started: None,
            received: 0,
            rearm: true,
            sleep: None,
        }
    }

    fn deadline(&mut self, now: Instant) -> Option<Instant> {
        let started = *self.started.get_or_insert(now);
        let by_gap = self.gap.map(|g| now + g);
        let by_rate = (self.min_rate > 0).then(|| {
            let secs = self.received as f64 / self.min_rate as f64;
            started + self.gap.unwrap_or_default() + Duration::from_secs_f64(secs.min(1e9))
        });
        match (by_gap, by_rate) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

impl Body for TimedBody {
    type Data = Bytes;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let this = &mut *self;
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(d) = frame.data_ref() {
                    this.received += d.len() as u64;
                }
                this.rearm = true;
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e.into()))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => {
                if this.rearm {
                    this.rearm = false;
                    match this.deadline(Instant::now()) {
                        None => this.sleep = None,
                        Some(d) => match &mut this.sleep {
                            Some(s) => s.as_mut().reset(d),
                            None => this.sleep = Some(Box::pin(tokio::time::sleep_until(d))),
                        },
                    }
                }
                if let Some(s) = &mut this.sleep
                    && s.as_mut().poll(cx).is_ready()
                {
                    this.state.kill();
                    return Poll::Ready(Some(Err(Box::new(BodyTimeout))));
                }
                Poll::Pending
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::{get, post};
    use http_body_util::{BodyExt as _, Empty};
    use std::time::Duration as D;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    /// SSE-ähnlicher Körper: `n` Ereignisse im Abstand von `every`.
    struct Ticks {
        interval: tokio::time::Interval,
        left: u32,
    }

    impl Body for Ticks {
        type Data = Bytes;
        type Error = Infallible;
        fn poll_frame(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
            if self.left == 0 {
                return Poll::Ready(None);
            }
            ready!(self.interval.poll_tick(cx));
            self.left -= 1;
            Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(b"data: x\n\n")))))
        }
    }

    fn ticks(n: u32, every: D) -> axum::body::Body {
        axum::body::Body::new(Ticks {
            interval: tokio::time::interval_at(Instant::now() + every, every),
            left: n,
        })
    }

    fn app() -> Router {
        Router::new()
            .route("/", get(|| async { "ok" }))
            .route("/echo", post(|b: Bytes| async move { b.len().to_string() }))
            .route("/stream", get(|| async { ticks(8, D::from_millis(400)) }))
    }

    fn limits(header: u64, body: u64, min_rate: u64, idle: u64) -> HttpLimits {
        let ms = |v: u64| (v > 0).then(|| D::from_millis(v));
        HttpLimits {
            header_read_timeout: ms(header),
            body_read_timeout: ms(body),
            body_min_bytes_per_sec: min_rate,
            idle_timeout: ms(idle),
            http2_max_concurrent_streams: 16,
            http2_keepalive_interval: Some(D::from_secs(1)),
            http2_keepalive_timeout: D::from_secs(1),
        }
    }

    struct Server {
        addr: std::net::SocketAddr,
        metrics: Arc<ListenerMetrics>,
        stop: watch::Sender<bool>,
        task: tokio::task::JoinHandle<io::Result<()>>,
    }

    impl Server {
        async fn start(l: HttpLimits, max: usize) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let metrics = Arc::new(ListenerMetrics::default());
            let (stop, rx) = watch::channel(false);
            let task = tokio::spawn(serve(
                listener,
                app(),
                max,
                Arc::new(l),
                Arc::clone(&metrics),
                rx,
            ));
            Self {
                addr,
                metrics,
                stop,
                task,
            }
        }

        async fn shutdown(self) {
            self.stop.send(true).unwrap();
            tokio::time::timeout(D::from_secs(5), self.task)
                .await
                .expect("Shutdown hängt")
                .unwrap()
                .unwrap();
        }
    }

    /// Liest, bis der Server schließt; liefert Gelesenes und Dauer.
    async fn read_until_closed(s: &mut TcpStream, max: D) -> (String, D) {
        let t = Instant::now();
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match tokio::time::timeout(max.saturating_sub(t.elapsed()), s.read(&mut buf)).await {
                Err(_) => panic!("Verbindung nicht geschlossen nach {max:?}"),
                Ok(Ok(0) | Err(_)) => break,
                Ok(Ok(n)) => out.extend_from_slice(&buf[..n]),
            }
        }
        (String::from_utf8_lossy(&out).into_owned(), t.elapsed())
    }

    /// Wartet bis zu 5 s, bis `cond` gilt (Zähler, die der Verbindungs-Task nach dem
    /// Schließen erhöht).
    async fn eventually(cond: impl Fn() -> bool) {
        let t = Instant::now();
        while !cond() {
            assert!(t.elapsed() < D::from_secs(5), "Bedingung nicht erfüllt");
            tokio::time::sleep(D::from_millis(10)).await;
        }
    }

    /// Eine HTTP/1-Antwort mit `content-length` lesen.
    async fn read_response(s: &mut TcpStream) -> String {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = tokio::time::timeout(D::from_secs(5), s.read(&mut buf))
                .await
                .unwrap()
                .unwrap();
            assert!(n > 0, "vorzeitig geschlossen");
            out.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&out).into_owned();
            if let Some(i) = text.find("\r\n\r\n") {
                let len: usize = text[..i]
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length: "))
                    .unwrap()
                    .parse()
                    .unwrap();
                if out.len() >= i + 4 + len {
                    return text;
                }
            }
        }
    }

    #[tokio::test]
    async fn slowloris_header_is_cut_but_keepalive_idle_is_not() {
        let srv = Server::start(limits(800, 0, 0, 10_000), 0).await;
        // Keep-Alive-Leerlauf länger als der Kopf-Timeout bricht nichts ab.
        let mut ka = TcpStream::connect(srv.addr).await.unwrap();
        ka.write_all(b"GET / HTTP/1.1\r\nhost: x\r\n\r\n")
            .await
            .unwrap();
        assert!(read_response(&mut ka).await.ends_with("ok"));
        tokio::time::sleep(D::from_millis(2000)).await;
        ka.write_all(b"GET / HTTP/1.1\r\nhost: x\r\n\r\n")
            .await
            .unwrap();
        assert!(read_response(&mut ka).await.ends_with("ok"));

        // Slowloris: Kopf Zeile für Zeile, alle 200 ms.
        let mut s = TcpStream::connect(srv.addr).await.unwrap();
        s.write_all(b"GET / HTTP/1.1\r\nhost: x\r\n").await.unwrap();
        let (mut r, mut w) = s.into_split();
        let trickle = tokio::spawn(async move {
            for i in 0..100 {
                tokio::time::sleep(D::from_millis(200)).await;
                if w.write_all(format!("x-{i}: y\r\n").as_bytes())
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
        let t = Instant::now();
        let mut buf = [0u8; 64];
        let n = tokio::time::timeout(D::from_secs(5), r.read(&mut buf))
            .await
            .expect("Slowloris nicht getrennt")
            .unwrap_or(0);
        assert_eq!(n, 0);
        let took = t.elapsed();
        assert!(
            took >= D::from_millis(700) && took < D::from_millis(2500),
            "{took:?}"
        );
        trickle.abort();
        assert_eq!(srv.metrics.timeout_header.load(Relaxed), 1);
        drop(ka);
        srv.shutdown().await;
    }

    #[tokio::test]
    async fn slow_body_is_cut_by_gap_and_by_min_rate() {
        let srv = Server::start(limits(5_000, 800, 1000, 10_000), 0).await;
        // Normaler Upload in Teilen.
        let mut ok = TcpStream::connect(srv.addr).await.unwrap();
        ok.write_all(b"POST /echo HTTP/1.1\r\nhost: x\r\ncontent-length: 20\r\n\r\n0123456789")
            .await
            .unwrap();
        tokio::time::sleep(D::from_millis(300)).await;
        ok.write_all(b"0123456789").await.unwrap();
        assert!(read_response(&mut ok).await.ends_with("20"));

        // Pause > Körper-Timeout.
        let mut s = TcpStream::connect(srv.addr).await.unwrap();
        s.write_all(b"POST /echo HTTP/1.1\r\nhost: x\r\ncontent-length: 100\r\n\r\n0123456789")
            .await
            .unwrap();
        let (_, took) = read_until_closed(&mut s, D::from_secs(10)).await;
        // Großzügige Obergrenze (Last durch `cargo test --workspace`); Hauptsache deutlich
        // vor dem Ende des Uploads.
        assert!(took < D::from_millis(5000), "{took:?}");
        // Der Zähler steigt ggf. erst kurz nach dem Schließen (eigener Task).
        eventually(|| srv.metrics.timeout_body.load(Relaxed) == 1).await;

        // Mindestrate: alle 300 ms 10 Bytes (keine Pause > 800 ms, aber ≈ 33 B/s).
        let s = TcpStream::connect(srv.addr).await.unwrap();
        let (mut r, mut w) = s.into_split();
        w.write_all(b"POST /echo HTTP/1.1\r\nhost: x\r\ncontent-length: 100000\r\n\r\n")
            .await
            .unwrap();
        let trickle = tokio::spawn(async move {
            for _ in 0..100 {
                if w.write_all(b"0123456789").await.is_err() {
                    return;
                }
                tokio::time::sleep(D::from_millis(300)).await;
            }
        });
        let t = Instant::now();
        let mut buf = [0u8; 4096];
        loop {
            match tokio::time::timeout(D::from_secs(15), r.read(&mut buf)).await {
                Err(_) => panic!("Mindestrate nicht durchgesetzt"),
                Ok(Ok(0) | Err(_)) => break,
                Ok(Ok(_)) => {}
            }
        }
        // Ohne Mindestrate liefe der Upload 30 s; Grenze großzügig gegen Last.
        assert!(t.elapsed() < D::from_secs(10), "{:?}", t.elapsed());
        trickle.abort();
        eventually(|| srv.metrics.timeout_body.load(Relaxed) == 2).await;
        srv.shutdown().await;
    }

    #[tokio::test]
    async fn connection_limit_rejects_without_blocking_accept() {
        let srv = Server::start(limits(5_000, 5_000, 0, 30_000), 2).await;
        let mut a = TcpStream::connect(srv.addr).await.unwrap();
        let b = TcpStream::connect(srv.addr).await.unwrap();
        a.write_all(b"GET / HTTP/1.1\r\nhost: x\r\n\r\n")
            .await
            .unwrap();
        assert!(read_response(&mut a).await.ends_with("ok"));
        // Weitere Verbindungen: sofort 503 und geschlossen.
        for _ in 0..3 {
            let mut c = TcpStream::connect(srv.addr).await.unwrap();
            let (text, took) = read_until_closed(&mut c, D::from_secs(2)).await;
            assert!(text.starts_with("HTTP/1.1 503"), "{text}");
            assert!(took < D::from_millis(500), "{took:?}");
        }
        assert_eq!(srv.metrics.rejected.load(Relaxed), 3);
        assert_eq!(srv.metrics.active.load(Relaxed), 2);
        // Platz frei → wieder angenommen.
        drop(b);
        tokio::time::sleep(D::from_millis(200)).await;
        let mut c = TcpStream::connect(srv.addr).await.unwrap();
        c.write_all(b"GET / HTTP/1.1\r\nhost: x\r\n\r\n")
            .await
            .unwrap();
        assert!(read_response(&mut c).await.ends_with("ok"));
        drop((a, c));
        srv.shutdown().await;
    }

    #[tokio::test]
    async fn idle_connection_is_closed() {
        let srv = Server::start(limits(5_000, 0, 0, 800), 0).await;
        let mut s = TcpStream::connect(srv.addr).await.unwrap();
        s.write_all(b"GET / HTTP/1.1\r\nhost: x\r\n\r\n")
            .await
            .unwrap();
        assert!(read_response(&mut s).await.ends_with("ok"));
        let (_, took) = read_until_closed(&mut s, D::from_secs(5)).await;
        assert!(
            took >= D::from_millis(700) && took < D::from_millis(2500),
            "{took:?}"
        );
        // Verbindung ohne ein Byte ebenso.
        let mut s = TcpStream::connect(srv.addr).await.unwrap();
        read_until_closed(&mut s, D::from_secs(5)).await;
        assert_eq!(srv.metrics.timeout_idle.load(Relaxed), 2);
        assert_eq!(srv.metrics.timeout_header.load(Relaxed), 0);
        srv.shutdown().await;
    }

    #[tokio::test]
    async fn long_streams_survive_all_read_timeouts() {
        // Alle Timeouts 600 ms, Stream ≈ 3,2 s mit 400 ms Abstand.
        let srv = Server::start(limits(600, 600, 1000, 600), 0).await;
        // HTTP/1.1 (chunked).
        let mut s = TcpStream::connect(srv.addr).await.unwrap();
        s.write_all(b"GET /stream HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
            .await
            .unwrap();
        let (text, took) = read_until_closed(&mut s, D::from_secs(10)).await;
        assert!(took >= D::from_secs(3), "{took:?}");
        assert_eq!(text.matches("data: x").count(), 8, "{text}");
        assert!(text.ends_with("0\r\n\r\n"), "{text}");

        // HTTP/2 (h2c, Vorwissen).
        let client = hyper_util::client::legacy::Client::builder(TokioExecutor::new())
            .http2_only(true)
            .build_http::<Empty<Bytes>>();
        let res = client
            .get(format!("http://{}/stream", srv.addr).parse().unwrap())
            .await
            .unwrap();
        let body = res.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body.len(), 8 * b"data: x\n\n".len());

        let m = srv.metrics.snapshot();
        assert_eq!(m[3..], [0, 0, 0, 0], "keine Timeouts: {m:?}");
        drop(client);
        srv.shutdown().await;
    }

    #[test]
    fn proto_detection() {
        assert!(matches!(
            Proto::Unknown(0).feed(b"PRI * HTTP/2.0"),
            Proto::Http2
        ));
        assert!(matches!(Proto::Unknown(0).feed(b"POST /"), Proto::Http1));
        let p = Proto::Unknown(0).feed(b"P");
        assert!(matches!(p, Proto::Unknown(1)));
        assert!(matches!(p.feed(b"RI"), Proto::Http2));
        assert!(matches!(p.feed(b"UT"), Proto::Http1));
    }

    #[test]
    fn metrics_render() {
        let m = ServerMetrics::default();
        m.data.rejected.fetch_add(2, Relaxed);
        let mut out = String::new();
        m.render_prometheus(&mut out, true);
        assert!(out.contains("aigw_server_connections_rejected_total{listener=\"data\"} 2"));
        assert!(out.contains("aigw_server_timeouts_total{listener=\"control\",kind=\"body\"} 0"));
        let mut out = String::new();
        m.render_prometheus(&mut out, false);
        assert!(!out.contains("control"));
    }
}
