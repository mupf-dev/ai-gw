// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Gateway-Logik unabhängig vom HTTP-Transport: Aggregation, Namensraum, Rechteprüfung,
//! Weiterleitung an die Upstreams.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use aigw_core::config::McpConfig;
use aigw_core::mcp::NAMESPACE_SEP;
use aigw_core::snapshot::{McpServerCtx, Snapshot, SnapshotHandle};
use aigw_core::upstream::UpstreamPolicy;
use serde_json::{Map, Value, json};

use crate::client::{ClientEnv, ClientError, NoteSink, Pool, ServerEvent, ServerStatus, Upstream};
use crate::grants::Access;
use crate::httpc::HttpClient;
use crate::jsonrpc::{FORBIDDEN, INVALID_PARAMS, RESOURCE_NOT_FOUND, RpcError};
use crate::protocol::{ListKind, method};
use crate::session::{Scope, Session, Sessions};

/// Sicht eines Clients für eine Anfrage: Snapshot, Freigaben, Endpunkt.
#[derive(Debug, Clone)]
pub struct View {
    /// Snapshot dieser Anfrage.
    pub snap: Arc<Snapshot>,
    /// Freigaben des Keys.
    pub access: Access,
    /// Endpunkt.
    pub scope: Scope,
}

/// Ziel eines Tool-Aufrufs nach Auflösung und Rechteprüfung.
#[derive(Debug, Clone)]
pub struct ToolTarget {
    /// Server.
    pub server: String,
    /// Tool-Name beim Upstream.
    pub tool: String,
    ctx: Arc<McpServerCtx>,
}

/// Fehler der Auflösung eines Tools mit Ergebnisklasse (für Log und Metrik).
#[derive(Debug, Clone)]
pub struct ResolveError {
    /// JSON-RPC-Fehler an den Client.
    pub error: RpcError,
    /// Server und Tool, soweit bekannt.
    pub server: Option<String>,
    /// Tool-Name (beim Upstream, falls aufgelöst; sonst wie angefragt).
    pub tool: String,
}

fn not_found(kind: &str, msg: String) -> RpcError {
    RpcError::gateway(INVALID_PARAMS, 404, kind, msg)
}

fn forbidden(kind: &str, msg: String) -> RpcError {
    RpcError::gateway(FORBIDDEN, 403, kind, msg)
}

impl View {
    /// Prüft den Endpunkt `/mcp/{server}`: unbekannt → `404`, nicht freigegeben → `403`.
    pub fn check_scope(snap: &Snapshot, access: &Access, server: &str) -> Result<(), RpcError> {
        if !snap.mcp_servers.contains_key(server) {
            return Err(RpcError::gateway(
                INVALID_PARAMS,
                404,
                "server_not_found",
                format!("MCP-Server {server:?} existiert nicht"),
            ));
        }
        if !access.server_allowed(server) {
            return Err(forbidden(
                "server_not_allowed",
                format!("MCP-Server {server:?} ist für diesen API-Key nicht freigegeben"),
            ));
        }
        Ok(())
    }

    /// Freigegebene Server im Geltungsbereich (sortiert).
    fn servers(&self) -> Vec<Arc<McpServerCtx>> {
        match &self.scope {
            Scope::Server(s) => self
                .access
                .server_allowed(s)
                .then(|| self.snap.mcp_servers.get(s).cloned())
                .flatten()
                .into_iter()
                .collect(),
            Scope::Aggregate => self
                .access
                .servers()
                .filter_map(|s| self.snap.mcp_servers.get(s).cloned())
                .collect(),
        }
    }

    fn aggregate(&self) -> bool {
        self.scope == Scope::Aggregate
    }

    /// Zerlegt einen Namen aus dem Namensraum in (Server, Name) – nur auf existierende
    /// Server (Servernamen enthalten nie `__`, Tool-Namen dürfen es).
    fn split<'a>(&self, name: &'a str) -> Option<(&'a str, &'a str)> {
        name.match_indices(NAMESPACE_SEP).find_map(|(i, _)| {
            let (server, rest) = (&name[..i], &name[i + NAMESPACE_SEP.len()..]);
            (self.snap.mcp_servers.contains_key(server) && !rest.is_empty())
                .then_some((server, rest))
        })
    }

    /// (Server, Name beim Upstream) zu einem Namen des Clients; `None`, wenn kein Server passt.
    fn target(&self, name: &str) -> Option<(String, String)> {
        match &self.scope {
            Scope::Server(s) => Some((s.clone(), name.to_owned())),
            Scope::Aggregate => self.split(name).map(|(s, n)| (s.to_owned(), n.to_owned())),
        }
    }
}

/// Hintergrund-Task des Gateways; wird beim Fallenlassen beendet.
#[derive(Debug)]
pub struct GatewayTask(tokio::task::JoinHandle<()>);

impl Drop for GatewayTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// MCP-Gateway: Upstream-Pool, Sessions, Aggregation.
#[derive(Debug)]
pub struct McpGateway {
    pool: Pool,
    sessions: Sessions,
    progress_seq: AtomicU64,
}

/// Fingerabdruck der MCP-relevanten Konfiguration (Server, Freigaben).
fn mcp_fingerprint(snap: &Snapshot) -> String {
    let mut servers: Vec<String> = snap
        .mcp_servers
        .values()
        .map(|s| {
            format!(
                "{}={}",
                s.server.metadata.name,
                serde_json::to_string(&s.server.spec).unwrap_or_default()
            )
        })
        .collect();
    servers.sort();
    let mut grants: Vec<String> = snap
        .mcp_grants
        .iter()
        .map(|g| serde_json::to_string(&g.spec).unwrap_or_default())
        .collect();
    grants.sort();
    format!("{servers:?}{grants:?}")
}

impl McpGateway {
    /// Gateway aus der Konfiguration `mcp` und den Upstream-Regeln (`admin.*`).
    pub fn new(cfg: &McpConfig, policy: UpstreamPolicy) -> Result<Self, String> {
        let connect_timeout = Duration::from_secs(cfg.connect_timeout_secs.max(1));
        let env = ClientEnv {
            http: HttpClient::new(Arc::new(policy), connect_timeout)?,
            stdio_commands: cfg.allow_stdio_commands.clone().into(),
            call_timeout: Duration::from_secs(cfg.call_timeout_secs.max(1)),
            connect_timeout,
            max_message_bytes: cfg.max_message_bytes.max(1024),
            list_ttl: Duration::from_secs(cfg.list_cache_secs),
        };
        if !cfg.allow_stdio_commands.is_empty() {
            tracing::warn!(
                commands = ?cfg.allow_stdio_commands,
                "MCP: stdio-Server freigeschaltet – diese Programme laufen mit den Rechten des Gateways"
            );
        }
        Ok(Self {
            pool: Pool::new(env),
            sessions: Sessions::new(
                cfg.max_sessions,
                Duration::from_secs(cfg.session_idle_timeout_secs.max(1)),
            ),
            progress_seq: AtomicU64::new(1),
        })
    }

    /// Sessions.
    pub fn sessions(&self) -> &Sessions {
        &self.sessions
    }

    /// Ereignisse der Upstreams (`…/list_changed`).
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<ServerEvent> {
        self.pool.subscribe()
    }

    /// Zustand der Upstream-Verbindungen.
    pub fn status(&self) -> Vec<ServerStatus> {
        self.pool.status()
    }

    /// Hintergrund-Task: bei Konfigurationsänderungen Verbindungen zu entfernten Servern
    /// schließen und Clients `…/list_changed` schicken (Freigaben können Listen ändern);
    /// regelmäßig abgelaufene Sessions entfernen.
    pub fn spawn(self: &Arc<Self>, handle: SnapshotHandle) -> GatewayTask {
        let me = Arc::clone(self);
        GatewayTask(tokio::spawn(async move {
            let mut rx = handle.subscribe();
            let mut last = mcp_fingerprint(&handle.load());
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tokio::select! {
                    changed = rx.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        let snap = handle.load();
                        let fp = mcp_fingerprint(&snap);
                        if fp != last {
                            last = fp;
                            me.pool.retain(|s| snap.mcp_servers.contains_key(s));
                            for kind in ListKind::ALL {
                                me.sessions.broadcast(&crate::jsonrpc::Message::notification(kind.changed_method(), None).to_value());
                            }
                        }
                    }
                    _ = tick.tick() => {
                        let n = me.sessions.evict_idle();
                        if n > 0 {
                            tracing::debug!(sessions = n, "MCP: abgelaufene Sessions entfernt");
                        }
                    }
                }
            }
        }))
    }

    async fn upstream(&self, ctx: &McpServerCtx) -> Result<Arc<Upstream>, ClientError> {
        self.pool.get(ctx).await
    }

    /// Anfrage mit einmaliger Wiederholung, falls die Verbindung (Session) abgelaufen war –
    /// nur für Methoden ohne Nebenwirkung bzw. wenn der Upstream sie nicht verarbeitet hat.
    async fn request(
        &self,
        ctx: &McpServerCtx,
        method: &str,
        params: Option<Value>,
        sink: Option<&NoteSink>,
    ) -> Result<Value, ClientError> {
        let up = self.upstream(ctx).await?;
        match up.request(method, params.clone(), sink).await {
            Err(ClientError::SessionExpired | ClientError::Closed) => {
                let up = self.upstream(ctx).await?;
                up.request(method, params, sink).await
            }
            other => other,
        }
    }

    async fn list(
        &self,
        ctx: &McpServerCtx,
        kind: ListKind,
        method: &'static str,
    ) -> Result<Arc<Vec<Value>>, ClientError> {
        let mut up = self.upstream(ctx).await?;
        if !up.supports(kind) {
            return Ok(Arc::new(Vec::new()));
        }
        match up.list(method).await {
            Err(ClientError::SessionExpired | ClientError::Closed) => {
                up = self.upstream(ctx).await?;
                up.list(method).await
            }
            other => other,
        }
    }

    /// `tools/list`: freigegebene Tools (aggregiert mit Namensraum `<server>__<tool>`).
    /// Ein nicht erreichbarer Server fehlt im aggregierten Ergebnis (Warnung im Log); am
    /// Einzel-Endpunkt ist das ein Fehler.
    pub async fn list_tools(&self, view: &View) -> Result<Value, RpcError> {
        let mut out = Vec::new();
        for ctx in view.servers() {
            let server = ctx.server.metadata.name.clone();
            let tools = match self.list(&ctx, ListKind::Tools, method::TOOLS_LIST).await {
                Ok(t) => t,
                Err(e) if view.aggregate() => {
                    tracing::warn!(server = %server, error = %e, "MCP: tools/list fehlgeschlagen, Server ausgelassen");
                    continue;
                }
                Err(e) => return Err(e.to_rpc(&server)),
            };
            for t in tools.iter() {
                let Some(name) = t.get("name").and_then(Value::as_str) else {
                    continue;
                };
                if !view.access.tool_allowed(&server, name) {
                    continue;
                }
                let mut t = t.clone();
                if view.aggregate() {
                    t["name"] = Value::String(format!("{server}{NAMESPACE_SEP}{name}"));
                }
                out.push(t);
            }
        }
        Ok(json!({ "tools": out }))
    }

    /// Löst ein Tool auf und prüft die Rechte: unbekannter Server/Tool → `404`
    /// (`-32602`), nicht freigegeben → `403` (`-32003`), Upstream nicht erreichbar → `502`.
    pub async fn resolve_tool(&self, view: &View, name: &str) -> Result<ToolTarget, ResolveError> {
        let fail = |error: RpcError, server: Option<String>, tool: &str| ResolveError {
            error,
            server,
            tool: tool.to_owned(),
        };
        let Some((server, tool)) = view.target(name) else {
            return Err(fail(
                not_found("tool_not_found", format!("Unbekanntes Tool {name:?}")),
                None,
                name,
            ));
        };
        let Some(ctx) = view.snap.mcp_servers.get(&server).cloned() else {
            return Err(fail(
                not_found(
                    "server_not_found",
                    format!("MCP-Server {server:?} existiert nicht"),
                ),
                None,
                &tool,
            ));
        };
        if !view.access.server_allowed(&server) {
            return Err(fail(
                forbidden(
                    "server_not_allowed",
                    format!("MCP-Server {server:?} ist für diesen API-Key nicht freigegeben"),
                ),
                Some(server),
                &tool,
            ));
        }
        let tools = self
            .list(&ctx, ListKind::Tools, method::TOOLS_LIST)
            .await
            .map_err(|e| fail(e.to_rpc(&server), Some(server.clone()), &tool))?;
        if !tools
            .iter()
            .any(|t| t.get("name").and_then(Value::as_str) == Some(tool.as_str()))
        {
            return Err(fail(
                not_found("tool_not_found", format!("Unbekanntes Tool {name:?}")),
                Some(server),
                &tool,
            ));
        }
        if !view.access.tool_allowed(&server, &tool) {
            return Err(fail(
                forbidden(
                    "tool_not_allowed",
                    format!("Tool {name:?} ist für diesen API-Key nicht freigegeben"),
                ),
                Some(server),
                &tool,
            ));
        }
        Ok(ToolTarget { server, tool, ctx })
    }

    /// `tools/call` an den Upstream. `params` sind die Parameter des Clients; Name und
    /// Fortschritts-Token werden für den Upstream umgeschrieben, Benachrichtigungen der
    /// Anfrage gehen an `notes` (mit dem Token des Clients).
    pub async fn call_tool(
        &self,
        target: &ToolTarget,
        params: Value,
        notes: Option<tokio::sync::mpsc::UnboundedSender<Value>>,
    ) -> Result<Value, ClientError> {
        let mut params = match params {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        params.insert("name".into(), Value::String(target.tool.clone()));
        let mut token = None;
        if let Some(t) = params
            .get_mut("_meta")
            .and_then(Value::as_object_mut)
            .and_then(|m| m.get_mut("progressToken"))
        {
            // Eindeutig über alle Clients (geteilte Upstream-Verbindung).
            let up = Value::String(format!(
                "aigw-{}",
                self.progress_seq.fetch_add(1, Ordering::Relaxed)
            ));
            token = Some((up.clone(), std::mem::replace(t, up)));
        }
        let sink = notes.map(|tx| NoteSink::new(tx, token));
        let up = self.upstream(&target.ctx).await?;
        match up
            .request(
                method::TOOLS_CALL,
                Some(Value::Object(params.clone())),
                sink.as_ref(),
            )
            .await
        {
            // Session abgelaufen: der Upstream hat die Anfrage nicht verarbeitet.
            Err(ClientError::SessionExpired) => {
                let up = self.upstream(&target.ctx).await?;
                up.request(
                    method::TOOLS_CALL,
                    Some(Value::Object(params)),
                    sink.as_ref(),
                )
                .await
            }
            other => other,
        }
    }

    /// `resources/list` bzw. `resources/templates/list` (aggregiert: URIs unverändert, die
    /// Session merkt sich den liefernden Server für `resources/read`).
    pub async fn list_resources(
        &self,
        view: &View,
        session: &Session,
        templates: bool,
    ) -> Result<Value, RpcError> {
        let (m, field) = if templates {
            (method::RESOURCE_TEMPLATES_LIST, "resourceTemplates")
        } else {
            (method::RESOURCES_LIST, "resources")
        };
        let mut out = Vec::new();
        let mut routes = Vec::new();
        for ctx in view.servers() {
            let server = ctx.server.metadata.name.clone();
            let items = match self.list(&ctx, ListKind::Resources, m).await {
                Ok(i) => i,
                Err(e) if view.aggregate() => {
                    tracing::warn!(server = %server, error = %e, "MCP: {m} fehlgeschlagen, Server ausgelassen");
                    continue;
                }
                Err(e) => return Err(e.to_rpc(&server)),
            };
            for r in items.iter() {
                if !templates && let Some(uri) = r.get("uri").and_then(Value::as_str) {
                    routes.push((uri.to_owned(), server.clone()));
                }
                out.push(r.clone());
            }
        }
        if view.aggregate() {
            session.set_resource_routes(routes);
        }
        Ok(json!({ field: out }))
    }

    /// `resources/read`.
    pub async fn read_resource(
        &self,
        view: &View,
        session: &Session,
        params: Value,
    ) -> Result<Value, RpcError> {
        let uri = params
            .get("uri")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("uri fehlt"))?
            .to_owned();
        let server = match &view.scope {
            Scope::Server(s) => s.clone(),
            Scope::Aggregate => match session.resource_route(&uri) {
                Some(s) => s,
                None => {
                    // Einziger freigegebener Server mit Ressourcen?
                    let mut candidates = Vec::new();
                    for ctx in view.servers() {
                        if let Ok(up) = self.upstream(&ctx).await
                            && up.supports(ListKind::Resources)
                        {
                            candidates.push(ctx.server.metadata.name.clone());
                        }
                    }
                    match candidates.as_slice() {
                        [one] => one.clone(),
                        _ => {
                            return Err(RpcError::gateway(
                                RESOURCE_NOT_FOUND,
                                404,
                                "resource_not_found",
                                format!(
                                    "Ressource {uri:?} unbekannt – zuerst resources/list aufrufen \
                                     oder den Endpunkt /mcp/{{server}} verwenden"
                                ),
                            ));
                        }
                    }
                }
            },
        };
        let ctx = self.allowed_server(view, &server)?;
        self.request(&ctx, method::RESOURCES_READ, Some(params), None)
            .await
            .map_err(|e| e.to_rpc(&server))
    }

    fn allowed_server(&self, view: &View, server: &str) -> Result<Arc<McpServerCtx>, RpcError> {
        View::check_scope(&view.snap, &view.access, server)?;
        view.snap.mcp_servers.get(server).cloned().ok_or_else(|| {
            not_found(
                "server_not_found",
                format!("MCP-Server {server:?} existiert nicht"),
            )
        })
    }

    /// `prompts/list` (aggregiert mit Namensraum).
    pub async fn list_prompts(&self, view: &View) -> Result<Value, RpcError> {
        let mut out = Vec::new();
        for ctx in view.servers() {
            let server = ctx.server.metadata.name.clone();
            let items = match self
                .list(&ctx, ListKind::Prompts, method::PROMPTS_LIST)
                .await
            {
                Ok(i) => i,
                Err(e) if view.aggregate() => {
                    tracing::warn!(server = %server, error = %e, "MCP: prompts/list fehlgeschlagen, Server ausgelassen");
                    continue;
                }
                Err(e) => return Err(e.to_rpc(&server)),
            };
            for p in items.iter() {
                let mut p = p.clone();
                if view.aggregate()
                    && let Some(n) = p.get("name").and_then(Value::as_str)
                {
                    p["name"] = Value::String(format!("{server}{NAMESPACE_SEP}{n}"));
                }
                out.push(p);
            }
        }
        Ok(json!({ "prompts": out }))
    }

    /// `prompts/get`.
    pub async fn get_prompt(&self, view: &View, params: Value) -> Result<Value, RpcError> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("name fehlt"))?
            .to_owned();
        let Some((server, prompt)) = view.target(&name) else {
            return Err(not_found(
                "prompt_not_found",
                format!("Unbekannter Prompt {name:?}"),
            ));
        };
        let ctx = self.allowed_server(view, &server)?;
        let mut params = params;
        params["name"] = Value::String(prompt);
        self.request(&ctx, method::PROMPTS_GET, Some(params), None)
            .await
            .map_err(|e| e.to_rpc(&server))
    }

    /// Server-Fähigkeiten für `initialize` (Gateway bietet alle Listen mit `listChanged`).
    pub fn capabilities() -> Value {
        json!({
            "tools": {"listChanged": true},
            "resources": {"listChanged": true},
            "prompts": {"listChanged": true},
        })
    }

    /// Freigegebene Server einer Sicht (für die Filterung von Upstream-Ereignissen).
    pub fn granted_servers(view: &View) -> BTreeSet<String> {
        view.servers()
            .into_iter()
            .map(|c| c.server.metadata.name.clone())
            .collect()
    }
}
