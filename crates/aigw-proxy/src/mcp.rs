// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! MCP-Endpunkte der Datenebene (AIGW-41): ai-gw als MCP-Server über Streamable HTTP.
//!
//! - `/mcp` – alle für den API-Key freigegebenen Upstream-Server, Tools und Prompts im
//!   Namensraum `<server>__<name>`.
//! - `/mcp/{server}` – ein Server, Namen wie beim Upstream.
//!
//! Je Endpunkt: `POST` (eine JSON-RPC-Nachricht; Antwort JSON bzw. SSE bei `tools/call`),
//! `GET` (SSE-Strom für Benachrichtigungen der Session, z. B. `…/list_changed`), `DELETE`
//! (Session beenden). Anmeldung wie auf `/v1/*` (API-Key über [`Authenticator`]); Sessions
//! sind an den Key gebunden. Jeder `tools/call` durchläuft die Limits des Keys (eine Anfrage)
//! und erzeugt einen Eintrag im Request-Log ([`WireFormat::Mcp`], `aigw_mcp_calls_total`).
//! Auch `initialize`, Listen, Ressourcen und Prompts durchlaufen die Limits (S-28: Anfragen,
//! `concurrency`), Sessions sind je Key/Team begrenzt, Benachrichtigungskanäle sind begrenzt.
//!
//! [`Authenticator`]: crate::Authenticator

// Ablehnungen sind fertige HTTP-Antworten; nur auf Fehlerpfaden, Boxen brächte nichts.
#![allow(clippy::result_large_err)]

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use aigw_core::ir::ErrorKind;
use aigw_core::observe::McpOutcome;
use aigw_core::snapshot::Snapshot;
use aigw_core::wire::WireFormat;
use aigw_core::wire::sse::write_event;
use aigw_mcp::jsonrpc::{INVALID_REQUEST, RATE_LIMITED, UPSTREAM_TIMEOUT};
use aigw_mcp::protocol::{self, PROTOCOL_HEADER, SESSION_HEADER, method};
use aigw_mcp::{
    Access, ClientError, McpGateway, Message, RequestId, RpcError, Scope, Session, View,
};
use axum::Router;
use axum::body::Body;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, Extension, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use futures_util::{FutureExt as _, Stream};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::Instrument as _;

use crate::ProxyState;
use crate::auth::{AuthContext, extract_api_key};
use crate::limits::{Admission, TokenEstimate, retry_after_secs};
use crate::observe::Recorder;
use crate::request_id::{RequestId as HttpRequestId, RequestStart};

/// `-32001`: nicht angemeldet.
const UNAUTHORIZED: i64 = -32001;
/// Abstand der Keep-Alive-Kommentare in SSE-Strömen.
const KEEPALIVE: Duration = Duration::from_secs(15);

/// Zustand der MCP-Routen.
#[derive(Clone)]
struct McpState {
    proxy: ProxyState,
    gateway: Arc<McpGateway>,
}

type St = State<Arc<McpState>>;

/// Router für `/mcp` und `/mcp/{server}` (Anmeldung, Limits und Beobachtung aus `state`).
pub fn router(state: ProxyState, gateway: Arc<McpGateway>) -> Router {
    let max_body = state.config.max_body_bytes;
    let auth = Arc::clone(&state.auth);
    let st = Arc::new(McpState {
        proxy: state,
        gateway,
    });
    Router::new()
        .route(
            "/mcp",
            axum::routing::post(post_all)
                .get(get_all)
                .delete(delete_all),
        )
        .route(
            "/mcp/{server}",
            axum::routing::post(post_one)
                .get(get_one)
                .delete(delete_one),
        )
        .layer(DefaultBodyLimit::max(max_body))
        .layer(axum::middleware::from_fn_with_state(auth, crate::auth_gate))
        .layer(axum::middleware::from_fn(crate::request_id::middleware))
        .with_state(st)
}

// ---------------------------------------------------------------------------------------------
// Antworten
// ---------------------------------------------------------------------------------------------

fn json_body(status: StatusCode, msg: &Message) -> Response {
    let mut res = Response::new(Body::from(msg.to_bytes()));
    *res.status_mut() = status;
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    res
}

/// Fehler auf HTTP-Ebene (vor bzw. außerhalb einer JSON-RPC-Anfrage).
fn http_error(status: StatusCode, id: Option<RequestId>, err: RpcError) -> Response {
    let mut res = json_body(status, &Message::error(id, err));
    if status == StatusCode::UNAUTHORIZED {
        res.headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    }
    res
}

fn status_of(err: &RpcError) -> StatusCode {
    err.status()
        .and_then(|s| StatusCode::from_u16(s).ok())
        .unwrap_or(StatusCode::BAD_REQUEST)
}

fn accepted() -> Response {
    let mut res = Response::new(Body::empty());
    *res.status_mut() = StatusCode::ACCEPTED;
    res
}

fn sse_frame(msg: &Value) -> Bytes {
    let mut out = Vec::new();
    write_event(
        &mut out,
        "message",
        &serde_json::to_vec(msg).unwrap_or_default(),
    );
    Bytes::from(out)
}

fn sse_response(body: Body) -> Response {
    let mut res = Response::new(body);
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    res
}

fn accepts_sse(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| v.contains("text/event-stream"))
}

// ---------------------------------------------------------------------------------------------
// Anmeldung, Geltungsbereich, Session
// ---------------------------------------------------------------------------------------------

struct Caller {
    auth: AuthContext,
    view: View,
}

fn authenticate(
    st: &McpState,
    headers: &HeaderMap,
    server: Option<&str>,
) -> Result<Caller, Response> {
    let auth = st
        .proxy
        .auth
        .authenticate(extract_api_key(headers).as_ref())
        .map_err(|e| {
            http_error(
                StatusCode::UNAUTHORIZED,
                None,
                RpcError::gateway(UNAUTHORIZED, 401, "unauthorized", e.message),
            )
        })?;
    let snap = auth
        .snapshot
        .clone()
        .unwrap_or_else(|| Arc::new(Snapshot::default()));
    let access = match &auth.key {
        Some(k) => Access::for_key(&snap, k),
        // Ohne Schlüsselprüfung (AllowAll, Tests): alle Server des Snapshots.
        None => Access::all(&snap),
    };
    let scope = match server {
        Some(s) => {
            View::check_scope(&snap, &access, s).map_err(|e| http_error(status_of(&e), None, e))?;
            Scope::Server(s.to_owned())
        }
        None => Scope::Aggregate,
    };
    Ok(Caller {
        auth,
        view: View {
            snap,
            access,
            scope,
        },
    })
}

/// Session aus dem Header: fehlt → `400`, unbekannt/abgelaufen/fremd → `404` (Client muss
/// neu initialisieren).
fn session_of(
    st: &McpState,
    headers: &HeaderMap,
    caller: &Caller,
) -> Result<Arc<Session>, Response> {
    let Some(id) = headers.get(SESSION_HEADER).and_then(|v| v.to_str().ok()) else {
        return Err(http_error(
            StatusCode::BAD_REQUEST,
            None,
            RpcError::new(
                INVALID_REQUEST,
                "Header Mcp-Session-Id fehlt (zuerst initialize)",
            ),
        ));
    };
    match st.gateway.sessions().get(id) {
        Some(s) if s.key == caller.auth.api_key_id() && s.scope == caller.view.scope => Ok(s),
        _ => Err(http_error(
            StatusCode::NOT_FOUND,
            None,
            RpcError::gateway(
                INVALID_REQUEST,
                404,
                "session_not_found",
                "Session unbekannt oder abgelaufen – bitte neu initialisieren",
            ),
        )),
    }
}

fn check_version_header(headers: &HeaderMap) -> Result<(), Response> {
    match headers.get(PROTOCOL_HEADER).map(|v| v.to_str()) {
        None => Ok(()),
        Some(Ok(v)) if protocol::is_supported(v) => Ok(()),
        _ => Err(http_error(
            StatusCode::BAD_REQUEST,
            None,
            RpcError::new(
                INVALID_REQUEST,
                format!(
                    "Nicht unterstützte MCP-Protokollversion (unterstützt: {})",
                    protocol::SUPPORTED_VERSIONS.join(", ")
                ),
            ),
        )),
    }
}

/// Ablehnung durch Limits: JSON-RPC-Fehler und `Retry-After` (Sekunden).
struct Limited {
    error: RpcError,
    retry_after: Option<u64>,
}

impl Limited {
    /// `status`: HTTP-Status (`initialize`: `429`, da keine Session entsteht; sonst `200` mit
    /// JSON-RPC-Fehler).
    fn response(self, status: StatusCode, id: RequestId) -> Response {
        let mut res = json_body(status, &Message::error(Some(id), self.error));
        if let Some(v) = self
            .retry_after
            .and_then(|s| HeaderValue::from_str(&s.to_string()).ok())
        {
            res.headers_mut().insert(header::RETRY_AFTER, v);
        }
        res
    }
}

/// Limits des Keys (eine Anfrage, keine Tokens; `concurrency` bis zum Fallenlassen der
/// [`Admission`]).
async fn admit(st: &McpState, caller: &Caller) -> Result<Admission, Limited> {
    st.proxy
        .limits
        .admit(
            &caller.auth,
            None,
            TokenEstimate::default(),
            std::time::SystemTime::now(),
        )
        .await
        .map_err(|denied| {
            let secs = denied.error.retry_after.map(retry_after_secs);
            let code = denied
                .error
                .error
                .code
                .clone()
                .unwrap_or_else(|| "rate_limited".to_owned());
            Limited {
                error: RpcError {
                    code: RATE_LIMITED,
                    message: denied.error.error.message.clone(),
                    data: Some(json!({
                        "status": 429, "code": "rate_limited", "reason": code, "retryAfter": secs
                    })),
                },
                retry_after: secs,
            }
        })
}

// ---------------------------------------------------------------------------------------------
// POST
// ---------------------------------------------------------------------------------------------

async fn post_all(
    State(st): St,
    Extension(rid): Extension<HttpRequestId>,
    Extension(started): Extension<RequestStart>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    post(&st, None, rid, started, headers, body).await
}

async fn post_one(
    State(st): St,
    Path(server): Path<String>,
    Extension(rid): Extension<HttpRequestId>,
    Extension(started): Extension<RequestStart>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    post(&st, Some(&server), rid, started, headers, body).await
}

async fn post(
    st: &Arc<McpState>,
    server: Option<&str>,
    rid: HttpRequestId,
    started: RequestStart,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let caller = match authenticate(st, &headers, server) {
        Ok(c) => c,
        Err(res) => return res,
    };
    let body = match body {
        Ok(b) => b,
        Err(r) => {
            let status = r.status();
            return http_error(
                status,
                None,
                RpcError::new(
                    INVALID_REQUEST,
                    format!("Body nicht lesbar: {}", r.body_text()),
                ),
            );
        }
    };
    if let Err(res) = check_version_header(&headers) {
        return res;
    }
    let msg = match Message::parse(&body) {
        Ok(m) => m,
        Err(e) => return http_error(StatusCode::BAD_REQUEST, None, e),
    };
    let (id, method, params) = match msg {
        Message::Request { id, method, params } => (id, method, params),
        Message::Notification { method, params } => {
            return match session_of(st, &headers, &caller) {
                Ok(s) => {
                    if method == method::CANCELLED
                        && let Some(id) = params
                            .as_ref()
                            .and_then(|p| p.get("requestId"))
                            .and_then(RequestId::from_value)
                    {
                        let hit = s.cancel(&id);
                        tracing::debug!(request = %id, hit, "MCP: Abbruch durch Client");
                    }
                    accepted()
                }
                Err(res) => res,
            };
        }
        // Antworten auf Server-Anfragen: das Gateway stellt keine.
        Message::Response { .. } => {
            return match session_of(st, &headers, &caller) {
                Ok(_) => accepted(),
                Err(res) => res,
            };
        }
    };

    if method == method::INITIALIZE {
        // S-28: auch `initialize` zählt gegen die Limits des Keys.
        let _admission = match admit(st, &caller).await {
            Ok(a) => a,
            Err(l) => return l.response(StatusCode::TOO_MANY_REQUESTS, id),
        };
        return initialize(st, &caller, id, params.as_ref());
    }
    let session = match session_of(st, &headers, &caller) {
        Ok(s) => s,
        Err(res) => return res,
    };
    let params = params.unwrap_or_else(|| json!({}));
    // S-28: Listen, Ressourcen und Prompts durchlaufen die Limits (ping nicht; `tools/call`
    // mit eigenem Request-Log-Eintrag unten). Die Admission lebt bis zur Antwort.
    let _admission = if method == method::PING || method == method::TOOLS_CALL {
        None
    } else {
        match admit(st, &caller).await {
            Ok(a) => Some(a),
            Err(l) => return l.response(StatusCode::OK, id),
        }
    };
    let gw = &st.gateway;
    let view = &caller.view;
    let result = match method.as_str() {
        method::PING => Ok(json!({})),
        method::TOOLS_CALL => {
            return tool_call(
                st, rid, started, &headers, body, caller, session, id, params,
            )
            .await;
        }
        method::TOOLS_LIST => gw.list_tools(view).await,
        method::RESOURCES_LIST => gw.list_resources(view, &session, false).await,
        method::RESOURCE_TEMPLATES_LIST => gw.list_resources(view, &session, true).await,
        method::RESOURCES_READ => gw.read_resource(view, &session, params).await,
        method::PROMPTS_LIST => gw.list_prompts(view).await,
        method::PROMPTS_GET => gw.get_prompt(view, params).await,
        other => Err(RpcError::method_not_found(other)),
    };
    let msg = match result {
        Ok(r) => Message::result(id, r),
        Err(e) => Message::error(Some(id), e),
    };
    json_body(StatusCode::OK, &msg)
}

fn initialize(st: &McpState, caller: &Caller, id: RequestId, params: Option<&Value>) -> Response {
    let requested = params
        .and_then(|p| p.get("protocolVersion"))
        .and_then(Value::as_str)
        .unwrap_or(protocol::LATEST_VERSION);
    let version = protocol::negotiate(requested);
    let session = match st.gateway.sessions().create(
        caller.auth.api_key_id(),
        caller.auth.team_id(),
        caller.view.scope.clone(),
        version,
    ) {
        Ok(s) => s,
        Err(e) => return http_error(status_of(&e), Some(id), e),
    };
    let instructions = match &caller.view.scope {
        Scope::Aggregate => "ai-gw MCP-Gateway: Tools und Prompts aller freigegebenen Server, \
                             benannt als <server>__<name>."
            .to_owned(),
        Scope::Server(s) => format!("ai-gw MCP-Gateway für den Server {s:?}."),
    };
    let result = json!({
        "protocolVersion": version,
        "capabilities": McpGateway::capabilities(),
        "serverInfo": {"name": aigw_core::NAME, "version": aigw_core::VERSION},
        "instructions": instructions,
    });
    let mut res = json_body(StatusCode::OK, &Message::result(id, result));
    if let Ok(v) = HeaderValue::from_str(&session.id) {
        res.headers_mut().insert(SESSION_HEADER, v);
    }
    res
}

// ---------------------------------------------------------------------------------------------
// tools/call
// ---------------------------------------------------------------------------------------------

fn outcome_of(err: &RpcError) -> (McpOutcome, u16, ErrorKind) {
    let status = err.status().unwrap_or(502);
    match (err.code, status) {
        (_, 404) => (McpOutcome::NotFound, 404, ErrorKind::NotFound),
        (_, 403) => (McpOutcome::Denied, 403, ErrorKind::PermissionDenied),
        (RATE_LIMITED, _) => (McpOutcome::RateLimited, 429, ErrorKind::RateLimit),
        (UPSTREAM_TIMEOUT, _) => (McpOutcome::Timeout, 504, ErrorKind::Timeout),
        _ => (McpOutcome::UpstreamError, 502, ErrorKind::BadGateway),
    }
}

#[allow(clippy::too_many_arguments)]
async fn tool_call(
    st: &Arc<McpState>,
    rid: HttpRequestId,
    started: RequestStart,
    headers: &HeaderMap,
    body: Bytes,
    caller: Caller,
    session: Arc<Session>,
    id: RequestId,
    params: Value,
) -> Response {
    let mut rec = Recorder::new(
        Arc::clone(&st.proxy.observer),
        rid.0.clone(),
        started,
        WireFormat::Mcp,
    );
    let span = crate::trace::request_span(&rid.0, WireFormat::Mcp, headers);
    rec.set_span(span.clone());
    rec.set_key(&caller.auth);
    rec.set_request_payload(headers, &body);
    drop(body);

    let Some(name) = params
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        let e = RpcError::invalid_params("name fehlt");
        rec.set_mcp("", "", McpOutcome::NotFound);
        rec.set_mcp_outcome(McpOutcome::NotFound, 400, Some(ErrorKind::InvalidRequest));
        rec.finish(false);
        return json_body(StatusCode::OK, &Message::error(Some(id), e));
    };
    // Auflösung und Rechte: unbekannt 404, nicht freigegeben 403.
    let target = match st
        .gateway
        .resolve_tool(&caller.view, &name)
        .instrument(span.clone())
        .await
    {
        Ok(t) => t,
        Err(e) => {
            let (outcome, status, kind) = outcome_of(&e.error);
            rec.set_mcp(e.server.as_deref().unwrap_or(""), &e.tool, outcome);
            rec.set_mcp_outcome(outcome, status, Some(kind));
            rec.finish(false);
            return json_body(StatusCode::OK, &Message::error(Some(id), e.error));
        }
    };
    rec.set_mcp(&target.server, &target.tool, McpOutcome::Cancelled);

    // Limits des Keys (eine Anfrage, keine Tokens).
    match admit(st, &caller).await {
        Ok(a) => rec.set_admission(a),
        Err(l) => {
            rec.set_mcp_outcome(McpOutcome::RateLimited, 429, Some(ErrorKind::RateLimit));
            rec.finish(false);
            return l.response(StatusCode::OK, id);
        }
    }

    // Bis zum Ergebnis gilt der Aufruf als abgebrochen (Verbindungsabbruch verwirft ihn).
    rec.set_mcp_outcome(McpOutcome::Cancelled, 499, None);
    let want_sse = accepts_sse(headers);
    // Begrenzt (S-28): voll → weitere Meldungen verwerfen (NoteSink loggt).
    let (notes_tx, notes_rx) = mpsc::channel(st.gateway.notification_buffer());
    let inflight = session.begin(id.clone());
    let gw = Arc::clone(&st.gateway);
    let call = async move {
        let mut inflight = inflight;
        let mut rec = rec;
        let outcome = tokio::select! {
            r = gw.call_tool(&target, params, want_sse.then_some(notes_tx)) => Some(r),
            _ = &mut inflight.cancelled => None,
        };
        match outcome {
            None => {
                rec.set_mcp_outcome(McpOutcome::Cancelled, 499, None);
                rec.finish(true);
                None
            }
            Some(Ok(result)) => {
                let is_error = result.get("isError").and_then(Value::as_bool) == Some(true);
                let o = if is_error {
                    McpOutcome::ToolError
                } else {
                    McpOutcome::Ok
                };
                rec.set_mcp_outcome(o, 200, None);
                if rec.captures() {
                    rec.set_response_payload(Bytes::from(
                        serde_json::to_vec(&result).unwrap_or_default(),
                    ));
                }
                rec.finish(false);
                Some(Message::result(id, result))
            }
            Some(Err(e)) => {
                let err = e.to_rpc(&target.server);
                let (o, status, kind) = match &e {
                    ClientError::Timeout => (McpOutcome::Timeout, 504, ErrorKind::Timeout),
                    _ => (McpOutcome::UpstreamError, 502, ErrorKind::BadGateway),
                };
                rec.set_mcp_outcome(o, status, Some(kind));
                rec.finish(false);
                Some(Message::error(Some(id), err))
            }
        }
    }
    .instrument(span);

    if want_sse {
        let stream = CallStream {
            call: Some(Box::pin(call)),
            notes: notes_rx,
            keepalive: tokio::time::interval_at(tokio::time::Instant::now() + KEEPALIVE, KEEPALIVE),
        };
        sse_response(Body::from_stream(stream))
    } else {
        match call.await {
            Some(msg) => json_body(StatusCode::OK, &msg),
            // Abgebrochen: keine Antwort (Spezifikation).
            None => accepted(),
        }
    }
}

type CallFuture = Pin<Box<dyn Future<Output = Option<Message>> + Send>>;

/// SSE-Antwort eines `tools/call`: Fortschritt/Log-Meldungen des Upstreams, dann das Ergebnis.
/// Bricht der Client die Verbindung ab, wird der Aufruf verworfen (Upstream erhält
/// `notifications/cancelled`, Request-Log: `client_aborted`).
struct CallStream {
    call: Option<CallFuture>,
    notes: mpsc::Receiver<Value>,
    keepalive: tokio::time::Interval,
}

impl Stream for CallStream {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let Some(call) = this.call.as_mut() else {
            return Poll::Ready(None);
        };
        if let Poll::Ready(Some(n)) = this.notes.poll_recv(cx) {
            return Poll::Ready(Some(Ok(sse_frame(&n))));
        }
        if let Poll::Ready(out) = call.poll_unpin(cx) {
            this.call = None;
            return Poll::Ready(out.map(|m| Ok(sse_frame(&m.to_value()))));
        }
        if this.keepalive.poll_tick(cx).is_ready() {
            return Poll::Ready(Some(Ok(Bytes::from_static(b": keep-alive\n\n"))));
        }
        Poll::Pending
    }
}

// ---------------------------------------------------------------------------------------------
// GET (Benachrichtigungen) und DELETE
// ---------------------------------------------------------------------------------------------

async fn get_all(State(st): St, headers: HeaderMap) -> Response {
    get(&st, None, &headers)
}

async fn get_one(State(st): St, Path(server): Path<String>, headers: HeaderMap) -> Response {
    get(&st, Some(&server), &headers)
}

fn get(st: &Arc<McpState>, server: Option<&str>, headers: &HeaderMap) -> Response {
    let caller = match authenticate(st, headers, server) {
        Ok(c) => c,
        Err(res) => return res,
    };
    if !accepts_sse(headers) {
        return http_error(
            StatusCode::NOT_ACCEPTABLE,
            None,
            RpcError::new(INVALID_REQUEST, "GET braucht Accept: text/event-stream"),
        );
    }
    let session = match session_of(st, headers, &caller) {
        Ok(s) => s,
        Err(res) => return res,
    };
    let servers = McpGateway::granted_servers(&caller.view);
    let mut session_rx = session.subscribe();
    let mut upstream_rx = st.gateway.subscribe();
    let (tx, rx) = mpsc::channel::<Bytes>(16);
    // Der Task endet, sobald der Client trennt (Senden schlägt fehl) oder die Session endet.
    let sessions = Arc::clone(&st.gateway);
    let sid = session.id.clone();
    drop(session);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + KEEPALIVE, KEEPALIVE);
        loop {
            let frame = tokio::select! {
                m = session_rx.recv() => match m {
                    Ok(v) => sse_frame(&v),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        // S-28: begrenzter Kanal – ein langsamer Client verliert Meldungen.
                        tracing::warn!(
                            dropped = n,
                            "MCP: Benachrichtigungen der Session verworfen – Client liest zu \
                             langsam (mcp.notification_buffer)"
                        );
                        continue;
                    }
                    Err(_) => break,
                },
                e = upstream_rx.recv() => match e {
                    Ok(ev) if servers.contains(&ev.server) => {
                        sse_frame(&Message::notification(ev.kind.changed_method(), None).to_value())
                    }
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(dropped = n, "MCP: Server-Ereignisse für GET-Strom verworfen");
                        continue;
                    }
                    Err(_) => break,
                },
                _ = tick.tick() => {
                    if sessions.sessions().get(&sid).is_none() {
                        break;
                    }
                    Bytes::from_static(b": keep-alive\n\n")
                }
            };
            if tx.send(frame).await.is_err() {
                break;
            }
        }
    });
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|b| (Ok::<_, Infallible>(b), rx))
    });
    sse_response(Body::from_stream(stream))
}

async fn delete_all(State(st): St, headers: HeaderMap) -> Response {
    delete(&st, None, &headers)
}

async fn delete_one(State(st): St, Path(server): Path<String>, headers: HeaderMap) -> Response {
    delete(&st, Some(&server), &headers)
}

fn delete(st: &Arc<McpState>, server: Option<&str>, headers: &HeaderMap) -> Response {
    let caller = match authenticate(st, headers, server) {
        Ok(c) => c,
        Err(res) => return res,
    };
    match session_of(st, headers, &caller) {
        Ok(s) => {
            st.gateway.sessions().remove(&s.id);
            let mut res = Response::new(Body::empty());
            *res.status_mut() = StatusCode::NO_CONTENT;
            res
        }
        Err(res) => res,
    }
}
