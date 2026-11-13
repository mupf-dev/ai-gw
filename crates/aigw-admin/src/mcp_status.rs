// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Zustand der Upstream-MCP-Server (AIGW-41-Folge): `GET /admin/api/v1/mcp-servers:status`.
//!
//! Verbindet die konfigurierten Server (Datenbank) mit dem Verbindungs-Pool des
//! MCP-Gateways ([`McpGateway::status`]): `up` (verbunden und initialisiert), `down` (letzter
//! Versuch fehlgeschlagen, mit Fehler und Zahl der Fehlversuche), `idle` (noch keine
//! Verbindung – der Pool verbindet bei Bedarf) bzw. `disabled`. Ohne aktives Gateway
//! (`mcp.enabled: false`) ist `gatewayEnabled` falsch und alle Server sind `idle`/`disabled`.
//! Lesen darf jede Rolle mit Leserecht auf den Katalog.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use aigw_core::resource::{McpServerSpec, McpTransport};
use aigw_proxy::McpGateway;
use aigw_store::Store;
use axum::extract::{Extension, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse as _, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use utoipa::ToSchema;

use crate::auth::{AdminAuthenticator, Permission, Principal, Target, require_principal};
use crate::problem::Problem;

/// Pfad des Endpunkts.
pub const STATUS_PATH: &str = "/admin/api/v1/mcp-servers:status";

/// Zustand des Routers.
#[derive(Clone)]
pub struct McpStatusState {
    /// Datenbank (konfigurierte Server).
    pub store: Store,
    /// Anmeldung (dieselbe wie für die übrige Admin-API).
    pub auth: Arc<dyn AdminAuthenticator>,
    /// MCP-Gateway (fehlt bei `mcp.enabled: false`).
    pub gateway: Option<Arc<McpGateway>>,
}

impl fmt::Debug for McpStatusState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpStatusState")
            .field("gateway", &self.gateway.is_some())
            .finish_non_exhaustive()
    }
}

/// Verbindungszustand eines Servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum McpConnectionState {
    /// Verbunden und initialisiert.
    Up,
    /// Letzter Verbindungsversuch fehlgeschlagen (Neuaufbau mit Backoff).
    Down,
    /// Noch keine Verbindung (wird bei Bedarf aufgebaut).
    Idle,
    /// Server abgeschaltet (`spec.enabled: false`).
    Disabled,
}

/// Zustand eines Upstream-MCP-Servers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpServerState {
    /// Name des Servers.
    pub name: String,
    /// Transport.
    pub transport: McpTransport,
    /// Zustand.
    pub state: McpConnectionState,
    /// Letzter Fehler (bei `down`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Aufeinanderfolgende Fehlversuche.
    pub failures: u32,
}

/// Antwort von `GET /mcp-servers:status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpStatusList {
    /// MCP-Gateway eingeschaltet (`mcp.enabled`).
    pub gateway_enabled: bool,
    /// Server (nach Name sortiert).
    pub items: Vec<McpServerState>,
}

/// Zustand der Upstream-MCP-Server.
#[utoipa::path(
    get, path = "/admin/api/v1/mcp-servers:status", tag = "mcp-servers",
    responses(
        (status = 200, description = "Zustand je Server", body = McpStatusList),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub(crate) async fn mcp_status(
    State(st): State<McpStatusState>,
    Extension(p): Extension<Principal>,
) -> Result<Response, Problem> {
    p.require_on(Permission::Read, &Target::Catalog)?;
    let servers = st.store.list::<McpServerSpec>().await?;
    let mut pool: HashMap<String, _> = st
        .gateway
        .as_ref()
        .map(|g| {
            g.status()
                .into_iter()
                .map(|s| (s.server.clone(), s))
                .collect()
        })
        .unwrap_or_default();
    let mut items: Vec<McpServerState> = servers
        .into_iter()
        .map(|s| {
            let name = s.metadata.name;
            let live = pool.remove(&name);
            let (state, error, failures) = match (s.spec.enabled, live) {
                (false, _) => (McpConnectionState::Disabled, None, 0),
                (true, Some(l)) if l.up => (McpConnectionState::Up, None, l.failures),
                (true, Some(l)) if l.failures > 0 || l.error.is_some() => {
                    (McpConnectionState::Down, l.error, l.failures)
                }
                (true, _) => (McpConnectionState::Idle, None, 0),
            };
            McpServerState {
                name,
                transport: s.spec.transport,
                state,
                error,
                failures,
            }
        })
        .collect();
    items.sort_by(|a, b| a.name.cmp(&b.name));
    let mut res = Json(McpStatusList {
        gateway_enabled: st.gateway.is_some(),
        items,
    })
    .into_response();
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(res)
}

/// Router (mit Anmeldung).
pub fn router(state: McpStatusState) -> Router {
    Router::new()
        .route(STATUS_PATH, get(mcp_status))
        .route_layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state.auth),
            require_principal,
        ))
        .with_state(state)
}
