// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! MCP-Gateway von ai-gw (AIGW-41): Eigenentwicklung des Model Context Protocol
//! (JSON-RPC 2.0) für Server- und Client-Seite.
//!
//! - [`jsonrpc`]: Nachrichten, IDs, Fehlercodes.
//! - [`protocol`]: Versionen (Aushandlung), Header, Methoden.
//! - [`client`]: Client zu Upstream-MCP-Servern über Streamable HTTP, das ältere HTTP+SSE
//!   und – nur nach Freischaltung – stdio; Verbindungs-Pool mit Neuaufbau und Backoff,
//!   Zeitlimits, Abbruch (`notifications/cancelled`), Listen-Cache mit `list_changed`.
//! - [`grants`]: Auswertung der `McpGrant`-Freigaben eines API-Keys (bis auf Tool-Ebene).
//! - [`session`]: Client-Sessions (`Mcp-Session-Id`).
//! - [`gateway`]: Aggregation (`<server>__<tool>`), Rechteprüfung, Weiterleitung.
//!
//! Den HTTP-Endpunkt (`/mcp`, `/mcp/{server}`) stellt `aigw-proxy` bereit (Auth, Limits,
//! Request-Log). Konzept: `docs/konzept/15-mcp-gateway.md`.

pub mod client;
pub mod gateway;
pub mod grants;
mod httpc;
pub mod jsonrpc;
pub mod protocol;
pub mod session;

pub use client::{ClientError, ServerEvent, ServerStatus};
pub use gateway::{GatewayTask, McpGateway, ResolveError, ToolTarget, View};
pub use grants::Access;
pub use httpc::HttpClient;
pub use jsonrpc::{Message, RequestId, RpcError};
pub use session::{Scope, Session, SessionLimits, Sessions};
