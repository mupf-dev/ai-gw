// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! MCP-Client zu Upstream-Servern: Transporte, Initialisierung, Anfragen, Abbruch,
//! Benachrichtigungen, Listen-Cache.
//!
//! Transporte:
//! - **Streamable HTTP** (aktuell): jede Nachricht ein `POST`; Antwort als JSON oder als
//!   SSE-Strom (Fortschritt, Server-Anfragen, dann das Ergebnis). `Mcp-Session-Id` und
//!   `MCP-Protocol-Version` werden nach der Initialisierung mitgeschickt; `404` auf eine
//!   Session heißt „Session abgelaufen“ (neu verbinden). Ein `GET`-Strom empfängt
//!   Benachrichtigungen des Servers (`…/list_changed`), falls er ihn anbietet.
//! - **HTTP+SSE** (älter, `2024-11-05`): `GET` öffnet den Ereignisstrom, das erste Ereignis
//!   `endpoint` nennt die POST-Adresse (nur gleiche Herkunft erlaubt, SSRF); Antworten kommen
//!   über den Strom.
//! - **stdio**: Kindprozess, eine JSON-Nachricht je Zeile (nur nach Freischaltung).
//!
//! Abbruch: Wird eine laufende Anfrage verworfen (Client-Abbruch, Zeitlimit), schickt der
//! Client `notifications/cancelled` an den Upstream.

use std::collections::HashMap;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use aigw_core::mcp::McpServerSettings;
use aigw_core::resource::{CredentialData, McpTransport};
use aigw_core::snapshot::McpServerCtx;
use aigw_core::wire::sse::SseParser;
use bytes::Bytes;
use http::header::{ACCEPT, CONTENT_TYPE};
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use http_body_util::BodyExt as _;
use hyper::body::Incoming;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
use tokio::sync::{broadcast, oneshot};

use crate::httpc::HttpClient;
use crate::jsonrpc::{
    INVALID_REQUEST, Message, RequestId, RpcError, UPSTREAM_ERROR, UPSTREAM_TIMEOUT,
};
use crate::protocol::{self, ListKind, PROTOCOL_HEADER, SESSION_HEADER, method};

/// Fehler bei der Kommunikation mit einem Upstream.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientError {
    /// JSON-RPC-Fehler des Upstreams (unverändert weitergeben).
    Rpc(RpcError),
    /// Upstream kennt die Session nicht mehr (neu verbinden).
    SessionExpired,
    /// Unerwarteter HTTP-Status.
    Status(u16, String),
    /// Verbindung, Protokoll oder Format.
    Transport(String),
    /// Zeitlimit überschritten.
    Timeout,
    /// Durch Regeln gesperrt (SSRF, stdio nicht freigeschaltet).
    Blocked(String),
    /// Verbindung geschlossen.
    Closed,
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rpc(e) => write!(f, "JSON-RPC-Fehler: {e}"),
            Self::SessionExpired => f.write_str("Session abgelaufen"),
            Self::Status(s, m) => write!(f, "HTTP {s}: {m}"),
            Self::Transport(m) => f.write_str(m),
            Self::Timeout => f.write_str("Zeitlimit überschritten"),
            Self::Blocked(m) => write!(f, "gesperrt: {m}"),
            Self::Closed => f.write_str("Verbindung geschlossen"),
        }
    }
}

impl ClientError {
    /// Muss die Verbindung neu aufgebaut werden?
    pub fn is_broken(&self) -> bool {
        match self {
            Self::SessionExpired | Self::Transport(_) | Self::Closed => true,
            Self::Status(s, _) => *s >= 500 || *s == 401 || *s == 403,
            Self::Rpc(_) | Self::Timeout | Self::Blocked(_) => false,
        }
    }

    /// Als JSON-RPC-Fehler an den Client (Upstream-Fehler bleiben unverändert).
    pub fn to_rpc(&self, server: &str) -> RpcError {
        match self {
            Self::Rpc(e) => e.clone(),
            Self::Timeout => RpcError::gateway(
                UPSTREAM_TIMEOUT,
                504,
                "upstream_timeout",
                format!("MCP-Server {server:?} hat nicht rechtzeitig geantwortet"),
            ),
            other => RpcError::gateway(
                UPSTREAM_ERROR,
                502,
                "upstream_error",
                format!("MCP-Server {server:?} nicht verfügbar: {other}"),
            ),
        }
    }
}

/// Ereignis eines Upstreams für Clients (`…/list_changed`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerEvent {
    /// Server.
    pub server: String,
    /// Geänderte Liste.
    pub kind: ListKind,
}

/// Empfänger für Benachrichtigungen, die zu **einer** Anfrage gehören (Fortschritt,
/// Log-Meldungen). Ersetzt das Fortschritts-Token des Gateways wieder durch das des Clients.
#[derive(Debug, Clone)]
pub struct NoteSink {
    tx: tokio::sync::mpsc::UnboundedSender<Value>,
    token: Option<(Value, Value)>,
}

impl NoteSink {
    /// Neuer Empfänger; `token` = (Token beim Upstream, Token des Clients).
    pub fn new(
        tx: tokio::sync::mpsc::UnboundedSender<Value>,
        token: Option<(Value, Value)>,
    ) -> Self {
        Self { tx, token }
    }

    fn send(&self, mut msg: Value) {
        if let Some((up, down)) = &self.token
            && let Some(t) = msg.pointer_mut("/params/progressToken")
            && t == up
        {
            *t = down.clone();
        }
        let _ = self.tx.send(msg);
    }
}

/// Einstellungen und Ressourcen für Upstream-Verbindungen.
#[derive(Debug, Clone)]
pub struct ClientEnv {
    /// HTTP-Client (mit SSRF-Schutz).
    pub http: HttpClient,
    /// Freigeschaltete stdio-Programme (leer = stdio aus).
    pub stdio_commands: Arc<[String]>,
    /// Standard-Zeitlimit je Anfrage.
    pub call_timeout: Duration,
    /// Zeitlimit für den Verbindungsaufbau (inkl. `endpoint`-Ereignis bei SSE).
    pub connect_timeout: Duration,
    /// Höchstgröße einer Nachricht.
    pub max_message_bytes: usize,
    /// Gültigkeit zwischengespeicherter Listen.
    pub list_ttl: Duration,
}

/// Bricht den Task beim Fallenlassen ab.
#[derive(Debug)]
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Zwischengespeicherte Liste (Zeitpunkt, Einträge).
type CachedList = (Instant, Arc<Vec<Value>>);

/// Zustand, den Verbindung, Lese-Tasks und [`Upstream`] teilen.
#[derive(Debug)]
pub(crate) struct Shared {
    server: String,
    events: broadcast::Sender<ServerEvent>,
    cache: Mutex<HashMap<&'static str, CachedList>>,
    generation: AtomicU64,
    dead: AtomicBool,
}

impl Shared {
    fn on_notification(&self, method: &str) {
        if let Some(kind) = ListKind::from_changed(method) {
            self.invalidate(kind);
            let _ = self.events.send(ServerEvent {
                server: self.server.clone(),
                kind,
            });
            tracing::debug!(server = %self.server, method, "MCP-Upstream: Liste geändert");
        } else {
            tracing::trace!(server = %self.server, method, "MCP-Upstream: Benachrichtigung ignoriert");
        }
    }

    fn invalidate(&self, kind: ListKind) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut c) = self.cache.lock() {
            c.retain(|m, _| list_kind_of(m) != Some(kind));
        }
    }

    fn mark_dead(&self) {
        self.dead.store(true, Ordering::SeqCst);
    }
}

/// Antwort auf eine Anfrage des Upstreams an den Client (Gateway): nur `ping`; Sampling,
/// Elicitation und Roots bietet das Gateway nicht an (keine Fähigkeit angemeldet).
fn answer_server_request(id: RequestId, method: &str) -> Message {
    if method == method::PING {
        Message::result(id, json!({}))
    } else {
        Message::error(Some(id), RpcError::method_not_found(method))
    }
}

/// Liest einen Body bis `max` Bytes.
async fn read_body(body: Incoming, max: usize) -> Result<Bytes, ClientError> {
    let mut body = body;
    let mut out: Vec<u8> = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| ClientError::Transport(format!("Body: {e}")))?;
        if let Ok(data) = frame.into_data() {
            if out.len() + data.len() > max {
                return Err(ClientError::Transport(format!(
                    "Antwort größer als {max} Bytes"
                )));
            }
            out.extend_from_slice(&data);
        }
    }
    Ok(Bytes::from(out))
}

fn snippet(b: &[u8]) -> String {
    let s = String::from_utf8_lossy(&b[..b.len().min(300)]);
    s.chars().filter(|c| !c.is_control()).collect()
}

/// SSE-Strom eines Bodys: liefert die `data` jedes Ereignisses (mit Ereignisnamen).
struct SseBody {
    body: Incoming,
    parser: SseParser,
    queue: std::collections::VecDeque<(Option<String>, String)>,
    max: usize,
}

impl SseBody {
    fn new(body: Incoming, max: usize) -> Self {
        Self {
            body,
            parser: SseParser::new(),
            queue: Default::default(),
            max,
        }
    }

    async fn next(&mut self) -> Result<Option<(Option<String>, String)>, ClientError> {
        loop {
            if let Some(ev) = self.queue.pop_front() {
                return Ok(Some(ev));
            }
            let Some(frame) = self.body.frame().await else {
                return Ok(None);
            };
            let frame = frame.map_err(|e| ClientError::Transport(format!("SSE: {e}")))?;
            if let Ok(data) = frame.into_data() {
                for ev in self.parser.push(&data) {
                    self.queue.push_back((ev.event, ev.data));
                }
                if self.parser.buffered() > self.max {
                    return Err(ClientError::Transport(format!(
                        "SSE-Ereignis größer als {} Bytes",
                        self.max
                    )));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Streamable HTTP
// ---------------------------------------------------------------------------------------------

#[derive(Debug)]
pub(crate) struct HttpConn {
    http: HttpClient,
    url: Uri,
    headers: HeaderMap,
    max: usize,
    session: RwLock<Option<HeaderValue>>,
    version: RwLock<Option<HeaderValue>>,
}

impl HttpConn {
    fn request(
        &self,
        method: Method,
        accept: &'static str,
        body: Bytes,
    ) -> Result<http::Request<Bytes>, ClientError> {
        let mut b = http::Request::builder()
            .method(method)
            .uri(self.url.clone())
            .header(ACCEPT, accept);
        if !body.is_empty() {
            b = b.header(CONTENT_TYPE, "application/json");
        }
        let mut req = b
            .body(body)
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        let h = req.headers_mut();
        for (k, v) in &self.headers {
            h.insert(k.clone(), v.clone());
        }
        if let Some(s) = self.session.read().ok().and_then(|s| s.clone()) {
            h.insert(SESSION_HEADER, s);
        }
        if let Some(v) = self.version.read().ok().and_then(|v| v.clone()) {
            h.insert(PROTOCOL_HEADER, v);
        }
        if let Some(tp) =
            aigw_core::observe::current_traceparent().and_then(|t| HeaderValue::from_str(&t).ok())
        {
            h.insert("traceparent", tp);
        }
        Ok(req)
    }

    fn has_session(&self) -> bool {
        self.session.read().is_ok_and(|s| s.is_some())
    }

    async fn send(
        &self,
        method: Method,
        accept: &'static str,
        body: Bytes,
    ) -> Result<http::Response<Incoming>, ClientError> {
        let req = self.request(method, accept, body)?;
        let res = self.http.send(req).await.map_err(|e| {
            if e.contains("SSRF") {
                ClientError::Blocked(e)
            } else {
                ClientError::Transport(e)
            }
        })?;
        if res.status() == StatusCode::NOT_FOUND && self.has_session() {
            return Err(ClientError::SessionExpired);
        }
        if !self.has_session()
            && let Some(s) = res.headers().get(SESSION_HEADER)
            && let Ok(mut w) = self.session.write()
        {
            *w = Some(s.clone());
        }
        Ok(res)
    }

    async fn post(&self, msg: &Message) -> Result<http::Response<Incoming>, ClientError> {
        self.send(
            Method::POST,
            "application/json, text/event-stream",
            Bytes::from(msg.to_bytes()),
        )
        .await
    }

    async fn error_of(&self, res: http::Response<Incoming>) -> ClientError {
        let status = res.status().as_u16();
        let body = read_body(res.into_body(), 4096).await.unwrap_or_default();
        ClientError::Status(status, snippet(&body))
    }

    /// Antwortet auf eine Anfrage des Servers (best effort, im Hintergrund).
    fn reply(self: &Arc<Self>, id: RequestId, method: &str) {
        let msg = answer_server_request(id, method);
        let me = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(e) = me.post(&msg).await {
                tracing::debug!(error = %e, "MCP-Upstream: Antwort auf Server-Anfrage fehlgeschlagen");
            }
        });
    }

    async fn call(
        self: &Arc<Self>,
        shared: &Shared,
        id: &RequestId,
        msg: &Message,
        sink: Option<&NoteSink>,
    ) -> Result<Value, ClientError> {
        let res = self.post(msg).await?;
        if !res.status().is_success() {
            return Err(self.error_of(res).await);
        }
        let ct = res
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if ct.starts_with("text/event-stream") {
            let mut sse = SseBody::new(res.into_body(), self.max);
            while let Some((_, data)) = sse.next().await? {
                if data.trim().is_empty() {
                    continue;
                }
                match Message::parse(data.as_bytes()) {
                    Ok(Message::Response {
                        id: Some(rid),
                        result,
                    }) if rid == *id => {
                        return result.map_err(ClientError::Rpc);
                    }
                    Ok(Message::Notification { method, params }) => {
                        route_notification(shared, sink, &method, params);
                    }
                    Ok(Message::Request { id, method, .. }) => self.reply(id, &method),
                    Ok(Message::Response { .. }) => {}
                    Err(e) => {
                        return Err(ClientError::Transport(format!("Ungültige Nachricht: {e}")));
                    }
                }
            }
            Err(ClientError::Transport(
                "SSE-Antwort endete ohne Ergebnis".into(),
            ))
        } else {
            let body = read_body(res.into_body(), self.max).await?;
            match Message::parse(&body) {
                Ok(Message::Response { result, .. }) => result.map_err(ClientError::Rpc),
                Ok(_) => Err(ClientError::Transport(
                    "Antwort ist keine JSON-RPC-Antwort".into(),
                )),
                Err(e) => Err(ClientError::Transport(format!(
                    "Ungültige Antwort ({e}): {}",
                    snippet(&body)
                ))),
            }
        }
    }

    async fn notify(&self, msg: &Message) -> Result<(), ClientError> {
        let res = self.post(msg).await?;
        if res.status().is_success() {
            Ok(())
        } else {
            Err(self.error_of(res).await)
        }
    }

    /// Hält einen `GET`-Strom für Benachrichtigungen des Servers offen (falls angeboten).
    fn spawn_listener(self: &Arc<Self>, shared: &Arc<Shared>) -> AbortOnDrop {
        let me = Arc::clone(self);
        let shared = Arc::clone(shared);
        AbortOnDrop(tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            while !shared.dead.load(Ordering::SeqCst) {
                match me
                    .send(Method::GET, "text/event-stream", Bytes::new())
                    .await
                {
                    Ok(res) if res.status() == StatusCode::METHOD_NOT_ALLOWED => return,
                    Ok(res) if res.status().is_success() => {
                        backoff = Duration::from_secs(1);
                        let mut sse = SseBody::new(res.into_body(), me.max);
                        while let Ok(Some((_, data))) = sse.next().await {
                            match Message::parse(data.as_bytes()) {
                                Ok(Message::Notification { method, .. }) => {
                                    shared.on_notification(&method);
                                }
                                Ok(Message::Request { id, method, .. }) => me.reply(id, &method),
                                _ => {}
                            }
                        }
                    }
                    Err(ClientError::SessionExpired) => {
                        shared.mark_dead();
                        return;
                    }
                    Ok(res) => {
                        tracing::debug!(server = %shared.server, status = %res.status(), "MCP-Upstream: GET-Strom abgelehnt");
                    }
                    Err(e) => {
                        tracing::debug!(server = %shared.server, error = %e, "MCP-Upstream: GET-Strom fehlgeschlagen");
                    }
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }))
    }
}

fn route_notification(
    shared: &Shared,
    sink: Option<&NoteSink>,
    method: &str,
    params: Option<Value>,
) {
    if ListKind::from_changed(method).is_some() {
        shared.on_notification(method);
        return;
    }
    match sink {
        Some(s) => s.send(Message::notification(method, params).to_value()),
        None => shared.on_notification(method),
    }
}

// ---------------------------------------------------------------------------------------------
// Duplex-Transporte (HTTP+SSE, stdio)
// ---------------------------------------------------------------------------------------------

// Eine Instanz je Verbindung – die Größe spielt keine Rolle.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum Writer {
    Post {
        http: HttpClient,
        url: Uri,
        headers: HeaderMap,
    },
    Stdin(tokio::sync::Mutex<tokio::process::ChildStdin>),
}

type Pending = HashMap<RequestId, oneshot::Sender<Result<Value, RpcError>>>;

#[derive(Debug)]
pub(crate) struct Duplex {
    writer: Writer,
    pending: Mutex<Pending>,
    progress: Mutex<HashMap<String, NoteSink>>,
    closed: AtomicBool,
}

/// Entfernt Einträge einer Anfrage beim Verlassen (auch bei Abbruch).
struct PendingGuard<'a> {
    d: &'a Duplex,
    id: RequestId,
    token: Option<String>,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut p) = self.d.pending.lock() {
            p.remove(&self.id);
        }
        if let (Some(t), Ok(mut p)) = (&self.token, self.d.progress.lock()) {
            p.remove(t);
        }
    }
}

impl Duplex {
    fn new(writer: Writer) -> Self {
        Self {
            writer,
            pending: Mutex::new(HashMap::new()),
            progress: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
        }
    }

    async fn write(&self, msg: &Message) -> Result<(), ClientError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(ClientError::Closed);
        }
        match &self.writer {
            Writer::Post { http, url, headers } => {
                let mut req = http::Request::builder()
                    .method(Method::POST)
                    .uri(url.clone())
                    .header(CONTENT_TYPE, "application/json")
                    .body(Bytes::from(msg.to_bytes()))
                    .map_err(|e| ClientError::Transport(e.to_string()))?;
                for (k, v) in headers {
                    req.headers_mut().insert(k.clone(), v.clone());
                }
                let res = http.send(req).await.map_err(ClientError::Transport)?;
                if res.status().is_success() {
                    Ok(())
                } else {
                    let status = res.status().as_u16();
                    let body = read_body(res.into_body(), 4096).await.unwrap_or_default();
                    Err(ClientError::Status(status, snippet(&body)))
                }
            }
            Writer::Stdin(stdin) => {
                let mut line = msg.to_bytes();
                line.push(b'\n');
                let mut w = stdin.lock().await;
                w.write_all(&line)
                    .await
                    .map_err(|e| ClientError::Transport(format!("stdin: {e}")))?;
                w.flush()
                    .await
                    .map_err(|e| ClientError::Transport(format!("stdin: {e}")))
            }
        }
    }

    async fn call(
        &self,
        id: &RequestId,
        msg: &Message,
        sink: Option<&NoteSink>,
    ) -> Result<Value, ClientError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(ClientError::Closed);
        }
        let (tx, rx) = oneshot::channel();
        let token = match (msg, sink) {
            (
                Message::Request {
                    params: Some(p), ..
                },
                Some(s),
            ) => p.pointer("/_meta/progressToken").map(|t| {
                let key = t.to_string();
                if let Ok(mut m) = self.progress.lock() {
                    m.insert(key.clone(), s.clone());
                }
                key
            }),
            _ => None,
        };
        if let Ok(mut p) = self.pending.lock() {
            p.insert(id.clone(), tx);
        }
        let _guard = PendingGuard {
            d: self,
            id: id.clone(),
            token,
        };
        self.write(msg).await?;
        match rx.await {
            Ok(r) => r.map_err(ClientError::Rpc),
            Err(_) => Err(ClientError::Closed),
        }
    }

    /// Verarbeitet eine eingehende Nachricht.
    fn dispatch(self: &Arc<Self>, shared: &Shared, data: &[u8]) {
        match Message::parse(data) {
            Ok(Message::Response {
                id: Some(id),
                result,
            }) => {
                let tx = self.pending.lock().ok().and_then(|mut p| p.remove(&id));
                if let Some(tx) = tx {
                    let _ = tx.send(result);
                }
            }
            Ok(Message::Notification { method, params }) => {
                let sink = (method == method::PROGRESS)
                    .then(|| params.as_ref()?.get("progressToken").map(Value::to_string))
                    .flatten()
                    .and_then(|t| self.progress.lock().ok()?.get(&t).cloned());
                route_notification(shared, sink.as_ref(), &method, params);
            }
            Ok(Message::Request { id, method, .. }) => {
                let me = Arc::clone(self);
                let msg = answer_server_request(id, &method);
                tokio::spawn(async move {
                    let _ = me.write(&msg).await;
                });
            }
            Ok(Message::Response { id: None, .. }) => {}
            Err(e) => {
                tracing::debug!(server = %shared.server, error = %e, "MCP-Upstream: ungültige Nachricht")
            }
        }
    }

    fn close(&self, shared: &Shared, reason: &str) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            tracing::info!(server = %shared.server, reason, "MCP-Upstream: Verbindung beendet");
        }
        shared.mark_dead();
        if let Ok(mut p) = self.pending.lock() {
            p.clear();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Verbindung
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum ConnKind {
    Http(Arc<HttpConn>),
    Duplex(Arc<Duplex>),
}

impl ConnKind {
    async fn notify(&self, msg: &Message) -> Result<(), ClientError> {
        match self {
            Self::Http(h) => h.notify(msg).await,
            Self::Duplex(d) => d.write(msg).await,
        }
    }
}

/// Schickt `notifications/cancelled`, wenn eine Anfrage vor ihrem Ergebnis verworfen wird.
struct CancelGuard {
    kind: ConnKind,
    id: Option<RequestId>,
    reason: &'static str,
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        let Some(id) = self.id.take() else {
            return;
        };
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let kind = self.kind.clone();
        let msg = Message::notification(
            method::CANCELLED,
            Some(json!({"requestId": id.to_value(), "reason": self.reason})),
        );
        rt.spawn(async move {
            if let Err(e) = kind.notify(&msg).await {
                tracing::debug!(error = %e, "MCP-Upstream: Abbruch nicht zustellbar");
            }
        });
    }
}

/// Initialisierte Verbindung zu einem Upstream-MCP-Server.
#[derive(Debug)]
pub struct Upstream {
    kind: ConnKind,
    shared: Arc<Shared>,
    next_id: AtomicI64,
    timeout: Duration,
    list_ttl: Duration,
    /// Ergebnis von `initialize`.
    pub info: InitInfo,
    _tasks: Vec<AbortOnDrop>,
}

/// Ergebnis der Initialisierung.
#[derive(Debug, Clone, Default)]
pub struct InitInfo {
    /// Ausgehandelte Protokollversion.
    pub protocol_version: String,
    /// Fähigkeiten des Servers.
    pub capabilities: Value,
    /// `serverInfo`.
    pub server_info: Value,
    /// `instructions` (falls gesetzt).
    pub instructions: Option<String>,
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.shared.mark_dead();
        // Streamable HTTP: Session beim Server beenden (best effort).
        if let ConnKind::Http(h) = &self.kind
            && h.has_session()
            && let Ok(rt) = tokio::runtime::Handle::try_current()
        {
            let h = Arc::clone(h);
            rt.spawn(async move {
                let _ = h
                    .send(Method::DELETE, "application/json", Bytes::new())
                    .await;
            });
        }
    }
}

/// Listen-Methoden (Methode, Feld im Ergebnis).
fn list_spec(m: &str) -> Option<&'static str> {
    match m {
        method::TOOLS_LIST => Some("tools"),
        method::PROMPTS_LIST => Some("prompts"),
        method::RESOURCES_LIST => Some("resources"),
        method::RESOURCE_TEMPLATES_LIST => Some("resourceTemplates"),
        _ => None,
    }
}

fn list_kind_of(m: &str) -> Option<ListKind> {
    match m {
        method::TOOLS_LIST => Some(ListKind::Tools),
        method::PROMPTS_LIST => Some(ListKind::Prompts),
        method::RESOURCES_LIST | method::RESOURCE_TEMPLATES_LIST => Some(ListKind::Resources),
        _ => None,
    }
}

/// Höchstzahl Seiten beim vollständigen Lesen einer Liste.
const MAX_PAGES: usize = 50;

impl Upstream {
    /// Baut die Verbindung auf und initialisiert sie.
    pub async fn connect(
        ctx: &McpServerCtx,
        env: &ClientEnv,
        events: broadcast::Sender<ServerEvent>,
    ) -> Result<Self, ClientError> {
        let name = ctx.server.metadata.name.clone();
        let spec = &ctx.server.spec;
        let settings =
            McpServerSettings::from_json(&spec.settings).map_err(ClientError::Blocked)?;
        let shared = Arc::new(Shared {
            server: name.clone(),
            events,
            cache: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(0),
            dead: AtomicBool::new(false),
        });
        let timeout = settings
            .timeout_ms
            .map_or(env.call_timeout, Duration::from_millis);
        let mut tasks = Vec::new();
        let kind = match spec.transport {
            McpTransport::StreamableHttp | McpTransport::Sse => {
                env.http
                    .policy()
                    .check_endpoint(&spec.url)
                    .map_err(ClientError::Blocked)?;
                let url: Uri = spec
                    .url
                    .parse()
                    .map_err(|_| ClientError::Blocked("ungültige URL".into()))?;
                let headers = http_headers(&settings, ctx.credential.as_deref())?;
                if spec.transport == McpTransport::StreamableHttp {
                    ConnKind::Http(Arc::new(HttpConn {
                        http: env.http.clone(),
                        url,
                        headers,
                        max: env.max_message_bytes,
                        session: RwLock::new(None),
                        version: RwLock::new(None),
                    }))
                } else {
                    let (d, task) = tokio::time::timeout(
                        env.connect_timeout,
                        connect_sse(env, &spec.url, url, headers, &shared),
                    )
                    .await
                    .map_err(|_| ClientError::Timeout)??;
                    tasks.push(task);
                    ConnKind::Duplex(d)
                }
            }
            McpTransport::Stdio => {
                let (d, t) = spawn_stdio(
                    env,
                    &spec.url,
                    &settings,
                    ctx.credential.as_deref(),
                    &shared,
                )?;
                tasks.extend(t);
                ConnKind::Duplex(d)
            }
        };
        let mut up = Self {
            kind,
            shared,
            next_id: AtomicI64::new(1),
            timeout,
            list_ttl: env.list_ttl,
            info: InitInfo::default(),
            _tasks: tasks,
        };
        let init = up
            .request(
                method::INITIALIZE,
                Some(json!({
                    "protocolVersion": protocol::LATEST_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": aigw_core::NAME, "version": aigw_core::VERSION},
                })),
                None,
            )
            .await?;
        let version = init
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if !protocol::is_supported(&version) {
            return Err(ClientError::Rpc(RpcError::new(
                INVALID_REQUEST,
                format!("Upstream verlangt nicht unterstützte Protokollversion {version:?}"),
            )));
        }
        if let ConnKind::Http(h) = &up.kind
            && let (Ok(v), Ok(mut w)) = (HeaderValue::from_str(&version), h.version.write())
        {
            *w = Some(v);
        }
        up.info = InitInfo {
            protocol_version: version,
            capabilities: init
                .get("capabilities")
                .cloned()
                .unwrap_or_else(|| json!({})),
            server_info: init.get("serverInfo").cloned().unwrap_or(Value::Null),
            instructions: init
                .get("instructions")
                .and_then(Value::as_str)
                .map(str::to_owned),
        };
        up.kind
            .notify(&Message::notification(method::INITIALIZED, None))
            .await?;
        if let ConnKind::Http(h) = &up.kind {
            up._tasks.push(h.spawn_listener(&up.shared));
        }
        Ok(up)
    }

    /// Ist die Verbindung unbrauchbar (geschlossen, Session abgelaufen)?
    pub fn is_dead(&self) -> bool {
        self.shared.dead.load(Ordering::SeqCst)
    }

    /// Bietet der Server Tools/Ressourcen/Prompts an?
    pub fn supports(&self, kind: ListKind) -> bool {
        self.info
            .capabilities
            .get(kind.capability())
            .is_some_and(Value::is_object)
    }

    /// Sendet eine Anfrage und wartet auf das Ergebnis (mit Zeitlimit). Wird die Anfrage vor
    /// dem Ergebnis verworfen, erhält der Upstream `notifications/cancelled`.
    pub async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        sink: Option<&NoteSink>,
    ) -> Result<Value, ClientError> {
        let id = RequestId::Num(self.next_id.fetch_add(1, Ordering::Relaxed));
        let msg = Message::Request {
            id: id.clone(),
            method: method.to_owned(),
            params,
        };
        let mut guard = CancelGuard {
            kind: self.kind.clone(),
            id: (method != method::INITIALIZE).then(|| id.clone()),
            reason: "Client hat die Anfrage abgebrochen",
        };
        let fut = async {
            match &self.kind {
                ConnKind::Http(h) => h.call(&self.shared, &id, &msg, sink).await,
                ConnKind::Duplex(d) => d.call(&id, &msg, sink).await,
            }
        };
        let res = match tokio::time::timeout(self.timeout, fut).await {
            Ok(r) => {
                guard.id = None;
                r
            }
            Err(_) => {
                guard.reason = "Zeitlimit des Gateways überschritten";
                Err(ClientError::Timeout)
            }
        };
        if let Err(e) = &res
            && e.is_broken()
        {
            self.shared.mark_dead();
        }
        res
    }

    /// Vollständige Liste (alle Seiten), zwischengespeichert bis `list_changed` bzw. Ablauf.
    pub async fn list(&self, method: &'static str) -> Result<Arc<Vec<Value>>, ClientError> {
        let field = list_spec(method)
            .ok_or_else(|| ClientError::Transport(format!("keine Listenmethode: {method}")))?;
        if let Some((at, items)) = self
            .shared
            .cache
            .lock()
            .ok()
            .and_then(|c| c.get(method).cloned())
            && at.elapsed() < self.list_ttl
        {
            return Ok(items);
        }
        let generation = self.shared.generation.load(Ordering::SeqCst);
        let mut items = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let params = cursor.as_ref().map(|c| json!({"cursor": c}));
            let page = self.request(method, params, None).await?;
            if let Some(list) = page.get(field).and_then(Value::as_array) {
                items.extend(list.iter().cloned());
            }
            cursor = page
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        let items = Arc::new(items);
        if self.shared.generation.load(Ordering::SeqCst) == generation
            && let Ok(mut c) = self.shared.cache.lock()
        {
            c.insert(method, (Instant::now(), Arc::clone(&items)));
        }
        Ok(items)
    }
}

/// Header für HTTP-Transporte: statische Header plus Token aus den Zugangsdaten.
fn http_headers(
    settings: &McpServerSettings,
    cred: Option<&CredentialData>,
) -> Result<HeaderMap, ClientError> {
    let mut h = HeaderMap::new();
    for (k, v) in &settings.headers {
        let name = HeaderName::from_bytes(k.as_bytes())
            .map_err(|_| ClientError::Blocked(format!("ungültiger Header {k:?}")))?;
        let value = HeaderValue::from_str(v)
            .map_err(|_| ClientError::Blocked(format!("ungültiger Wert für Header {k:?}")))?;
        h.insert(name, value);
    }
    if let Some(c) = cred {
        let field = settings.credential_field();
        let secret = c.get(field).ok_or_else(|| {
            ClientError::Blocked(format!("Zugangsdaten enthalten kein Feld {field:?}"))
        })?;
        let header = settings
            .credential_header
            .as_deref()
            .unwrap_or("authorization")
            .to_ascii_lowercase();
        let raw = if header == "authorization" {
            format!("Bearer {}", secret.expose())
        } else {
            secret.expose().to_owned()
        };
        let mut value = HeaderValue::from_str(&raw)
            .map_err(|_| ClientError::Blocked("Token enthält ungültige Zeichen".into()))?;
        value.set_sensitive(true);
        let name = HeaderName::from_bytes(header.as_bytes())
            .map_err(|_| ClientError::Blocked("ungültiger credentialHeader".into()))?;
        h.insert(name, value);
    }
    Ok(h)
}

/// Älterer HTTP+SSE-Transport: Ereignisstrom öffnen, `endpoint` abwarten, Lese-Task starten.
async fn connect_sse(
    env: &ClientEnv,
    raw_url: &str,
    url: Uri,
    headers: HeaderMap,
    shared: &Arc<Shared>,
) -> Result<(Arc<Duplex>, AbortOnDrop), ClientError> {
    let mut req = http::Request::builder()
        .method(Method::GET)
        .uri(url)
        .header(ACCEPT, "text/event-stream")
        .body(Bytes::new())
        .map_err(|e| ClientError::Transport(e.to_string()))?;
    for (k, v) in &headers {
        req.headers_mut().insert(k.clone(), v.clone());
    }
    let res = env.http.send(req).await.map_err(|e| {
        if e.contains("SSRF") {
            ClientError::Blocked(e)
        } else {
            ClientError::Transport(e)
        }
    })?;
    if !res.status().is_success() {
        let status = res.status().as_u16();
        let body = read_body(res.into_body(), 4096).await.unwrap_or_default();
        return Err(ClientError::Status(status, snippet(&body)));
    }
    let mut sse = SseBody::new(res.into_body(), env.max_message_bytes);
    let endpoint = loop {
        match sse.next().await? {
            Some((Some(ev), data)) if ev == "endpoint" => break data,
            Some(_) => {}
            None => {
                return Err(ClientError::Transport(
                    "SSE-Strom ohne endpoint-Ereignis".into(),
                ));
            }
        }
    };
    let base = url::Url::parse(raw_url).map_err(|e| ClientError::Transport(e.to_string()))?;
    let post = base
        .join(endpoint.trim())
        .map_err(|e| ClientError::Transport(format!("ungültiger endpoint: {e}")))?;
    if post.origin() != base.origin() {
        return Err(ClientError::Blocked(format!(
            "endpoint {post} hat eine andere Herkunft als {base} (SSRF-Schutz)"
        )));
    }
    let post: Uri = post
        .as_str()
        .parse()
        .map_err(|_| ClientError::Transport("ungültiger endpoint".into()))?;
    let duplex = Arc::new(Duplex::new(Writer::Post {
        http: env.http.clone(),
        url: post,
        headers,
    }));
    let d = Arc::clone(&duplex);
    let sh = Arc::clone(shared);
    let task = AbortOnDrop(tokio::spawn(async move {
        loop {
            match sse.next().await {
                Ok(Some((ev, data))) => {
                    if ev.as_deref().is_none_or(|e| e == "message") && !data.trim().is_empty() {
                        d.dispatch(&sh, data.as_bytes());
                    }
                }
                Ok(None) => {
                    d.close(&sh, "SSE-Strom beendet");
                    return;
                }
                Err(e) => {
                    d.close(&sh, &e.to_string());
                    return;
                }
            }
        }
    }));
    Ok((duplex, task))
}

/// stdio-Transport: Kindprozess starten (nur freigeschaltete Programme).
fn spawn_stdio(
    env: &ClientEnv,
    command: &str,
    settings: &McpServerSettings,
    cred: Option<&CredentialData>,
    shared: &Arc<Shared>,
) -> Result<(Arc<Duplex>, Vec<AbortOnDrop>), ClientError> {
    if !env.stdio_commands.iter().any(|c| c == command) {
        return Err(ClientError::Blocked(format!(
            "stdio-Programm {command:?} ist nicht freigeschaltet (mcp.allow_stdio_commands)"
        )));
    }
    let mut cmd = tokio::process::Command::new(command);
    cmd.args(&settings.args)
        .env_clear()
        .env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into()),
        )
        .envs(&settings.env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if let (Some(var), Some(c)) = (&settings.credential_env, cred) {
        let field = settings.credential_field();
        let secret = c.get(field).ok_or_else(|| {
            ClientError::Blocked(format!("Zugangsdaten enthalten kein Feld {field:?}"))
        })?;
        cmd.env(var, secret.expose());
    }
    let mut child = cmd.spawn().map_err(|e| {
        ClientError::Transport(format!("Start von {command:?} fehlgeschlagen: {e}"))
    })?;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err(ClientError::Transport("stdio-Kanäle fehlen".into()));
    };
    let duplex = Arc::new(Duplex::new(Writer::Stdin(tokio::sync::Mutex::new(stdin))));
    let d = Arc::clone(&duplex);
    let sh = Arc::clone(shared);
    let max = env.max_message_bytes;
    let reader = AbortOnDrop(tokio::spawn(async move {
        let mut out = tokio::io::BufReader::new(stdout);
        let mut line = Vec::new();
        loop {
            line.clear();
            match out.read_until(b'\n', &mut line).await {
                Ok(0) => break,
                Ok(_) if line.len() > max => {
                    d.close(&sh, "Nachricht zu groß");
                    break;
                }
                Ok(_) => {
                    if !line.iter().all(u8::is_ascii_whitespace) {
                        d.dispatch(&sh, &line);
                    }
                }
                Err(e) => {
                    d.close(&sh, &e.to_string());
                    break;
                }
            }
        }
        d.close(&sh, "Prozess beendet");
        let _ = child.kill().await;
    }));
    let server = shared.server.clone();
    let logger = AbortOnDrop(tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(stderr).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            tracing::debug!(server = %server, line = %l.chars().take(500).collect::<String>(), "MCP-stdio stderr");
        }
    }));
    Ok((duplex, vec![reader, logger]))
}

// ---------------------------------------------------------------------------------------------
// Pool
// ---------------------------------------------------------------------------------------------

#[derive(Debug)]
enum SlotState {
    Empty,
    Up(Arc<Upstream>),
    Down {
        until: Instant,
        error: ClientError,
        failures: u32,
    },
}

#[derive(Debug)]
struct Slot {
    fingerprint: u64,
    state: tokio::sync::Mutex<SlotState>,
}

/// Zustand eines Upstreams (für Health-Auskunft).
#[derive(Debug, Clone, PartialEq)]
pub struct ServerStatus {
    /// Server.
    pub server: String,
    /// Verbunden und initialisiert.
    pub up: bool,
    /// Letzter Fehler (bei `up == false`).
    pub error: Option<String>,
    /// Aufeinanderfolgende Fehlversuche.
    pub failures: u32,
}

/// Verbindungs-Pool: eine initialisierte Verbindung je Server, geteilt von allen Clients;
/// Neuaufbau nach Fehlern mit exponentiellem Backoff (1 s … 60 s), Neuaufbau bei geänderter
/// Konfiguration (Fingerabdruck aus Spezifikation und Zugangsdaten).
#[derive(Debug)]
pub struct Pool {
    env: ClientEnv,
    slots: Mutex<HashMap<String, Arc<Slot>>>,
    events: broadcast::Sender<ServerEvent>,
}

fn fingerprint(ctx: &McpServerCtx) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(&ctx.server.spec)
        .unwrap_or_default()
        .hash(&mut h);
    if let Some(c) = &ctx.credential {
        for (k, v) in &c.0 {
            k.hash(&mut h);
            v.expose().hash(&mut h);
        }
    }
    h.finish()
}

impl Pool {
    /// Leerer Pool.
    pub fn new(env: ClientEnv) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            env,
            slots: Mutex::new(HashMap::new()),
            events,
        }
    }

    /// Einstellungen.
    pub fn env(&self) -> &ClientEnv {
        &self.env
    }

    /// Ereignisse aller Upstreams (`…/list_changed`).
    pub fn subscribe(&self) -> broadcast::Receiver<ServerEvent> {
        self.events.subscribe()
    }

    /// Verbindung zum Server (aufbauen, falls nötig).
    pub async fn get(&self, ctx: &McpServerCtx) -> Result<Arc<Upstream>, ClientError> {
        let fp = fingerprint(ctx);
        let name = &ctx.server.metadata.name;
        let slot = {
            let mut slots = self
                .slots
                .lock()
                .map_err(|_| ClientError::Transport("Pool gesperrt".into()))?;
            let slot = slots.entry(name.clone()).or_insert_with(|| {
                Arc::new(Slot {
                    fingerprint: fp,
                    state: tokio::sync::Mutex::new(SlotState::Empty),
                })
            });
            if slot.fingerprint != fp {
                *slot = Arc::new(Slot {
                    fingerprint: fp,
                    state: tokio::sync::Mutex::new(SlotState::Empty),
                });
            }
            Arc::clone(slot)
        };
        let mut st = slot.state.lock().await;
        let failures = match &*st {
            SlotState::Up(u) if !u.is_dead() => return Ok(Arc::clone(u)),
            SlotState::Down { until, error, .. } if Instant::now() < *until => {
                return Err(error.clone());
            }
            SlotState::Down { failures, .. } => *failures,
            _ => 0,
        };
        match Upstream::connect(ctx, &self.env, self.events.clone()).await {
            Ok(u) => {
                tracing::info!(server = %name, version = %u.info.protocol_version, "MCP-Upstream verbunden");
                let u = Arc::new(u);
                *st = SlotState::Up(Arc::clone(&u));
                Ok(u)
            }
            Err(e) => {
                let backoff =
                    Duration::from_secs(1u64 << failures.min(6)).min(Duration::from_secs(60));
                tracing::warn!(server = %name, error = %e, backoff_s = backoff.as_secs(), "MCP-Upstream nicht erreichbar");
                *st = SlotState::Down {
                    until: Instant::now() + backoff,
                    error: e.clone(),
                    failures: failures.saturating_add(1),
                };
                Err(e)
            }
        }
    }

    /// Entfernt Verbindungen zu Servern, für die `keep` `false` liefert (gelöscht/deaktiviert).
    pub fn retain(&self, keep: impl Fn(&str) -> bool) {
        if let Ok(mut s) = self.slots.lock() {
            s.retain(|name, _| keep(name));
        }
    }

    /// Zustand aller bekannten Upstreams (Server mit gerade laufendem Verbindungsaufbau fehlen).
    pub fn status(&self) -> Vec<ServerStatus> {
        let slots: Vec<(String, Arc<Slot>)> = self
            .slots
            .lock()
            .map(|s| s.iter().map(|(k, v)| (k.clone(), Arc::clone(v))).collect())
            .unwrap_or_default();
        let mut out: Vec<ServerStatus> = slots
            .into_iter()
            .filter_map(|(server, slot)| {
                let st = slot.state.try_lock().ok()?;
                Some(match &*st {
                    SlotState::Up(u) => ServerStatus {
                        server,
                        up: !u.is_dead(),
                        error: None,
                        failures: 0,
                    },
                    SlotState::Down {
                        error, failures, ..
                    } => ServerStatus {
                        server,
                        up: false,
                        error: Some(error.to_string()),
                        failures: *failures,
                    },
                    SlotState::Empty => ServerStatus {
                        server,
                        up: false,
                        error: None,
                        failures: 0,
                    },
                })
            })
            .collect();
        out.sort_by(|a, b| a.server.cmp(&b.server));
        out
    }
}
