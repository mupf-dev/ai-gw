// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! [`PluginHost`] des Gateways (AIGW-38): Zugangsdaten aus dem aktuellen Snapshot,
//! HTTP-Aufrufe über den gemeinsamen Outbound-Baustein (AIGW-72: SSRF-Regeln auch nach der
//! Namensauflösung, keine Weiterleitungen, Größen- und Zeitlimit) und Modellaufrufe über das
//! eigene Routing – ohne Plugin-Kette (keine Rekursion) und ohne Limits des Clients.

use std::fmt;
use std::sync::Arc;

use aigw_core::ir::{ChatRequest, ChatResponse};
use aigw_core::resource::CredentialData;
use aigw_core::snapshot::SnapshotHandle;
use aigw_core::upstream::UpstreamPolicy;
use aigw_counters::CounterBackend;
use aigw_outbound::{AddressGuard, FetchLimits, OutboundClient, OutboundOptions};
use aigw_plugins::{BoxFuture, HttpCall, HttpReply, PluginError, PluginHost};
use bytes::Bytes;

use crate::routes::ModelRoutes;
use crate::routing::exec::{self, CapacityCtx, Outcome};

/// Höchstgröße einer Antwort externer Prüfdienste.
const MAX_REPLY_BYTES: usize = 256 * 1024;

struct Inner {
    routes: Arc<dyn ModelRoutes>,
    counters: Arc<dyn CounterBackend>,
    snapshot: Option<SnapshotHandle>,
    policy: UpstreamPolicy,
    client: OutboundClient,
}

/// Dienste des Gateways für Plugins (günstig klonbar).
#[derive(Clone)]
pub struct GatewayHost(Arc<Inner>);

impl fmt::Debug for GatewayHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GatewayHost")
            .field("policy", &self.0.policy)
            .finish_non_exhaustive()
    }
}

impl GatewayHost {
    /// Host aus Routing, Zähler-Backend (Deployment-Kapazität), Snapshot (Zugangsdaten),
    /// SSRF-Regeln (Prüfung der URL) und HTTP-Client (Prüfung nach der Namensauflösung; sollte
    /// dieselben Regeln tragen).
    pub fn new(
        routes: Arc<dyn ModelRoutes>,
        counters: Arc<dyn CounterBackend>,
        snapshot: Option<SnapshotHandle>,
        policy: UpstreamPolicy,
        client: OutboundClient,
    ) -> Self {
        Self(Arc::new(Inner {
            routes,
            counters,
            snapshot,
            policy,
            client,
        }))
    }
}

impl GatewayHost {
    /// Wie [`GatewayHost::new`] mit eigenem HTTP-Client (SSRF-Regeln `policy`, eigener Pool).
    pub fn with_defaults(
        routes: Arc<dyn ModelRoutes>,
        counters: Arc<dyn CounterBackend>,
        snapshot: Option<SnapshotHandle>,
        policy: UpstreamPolicy,
    ) -> Result<Self, String> {
        let client = OutboundClient::builder(AddressGuard::policy(policy.clone()))
            .options(OutboundOptions {
                connect_timeout: std::time::Duration::from_secs(5),
                ..OutboundOptions::default()
            })
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self::new(routes, counters, snapshot, policy, client))
    }
}

impl PluginHost for GatewayHost {
    fn credential(&self, name: &str) -> Option<Arc<CredentialData>> {
        self.0
            .snapshot
            .as_ref()?
            .load()
            .credential_data
            .get(name)
            .cloned()
    }

    fn check_endpoint(&self, url: &str) -> Result<(), String> {
        self.0.policy.check_endpoint(url)
    }

    fn post_json(&self, call: HttpCall) -> BoxFuture<'static, Result<HttpReply, PluginError>> {
        let inner = Arc::clone(&self.0);
        Box::pin(async move {
            inner
                .policy
                .check_endpoint(&call.url)
                .map_err(PluginError::new)?;
            let mut req = axum::http::Request::post(&call.url)
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .header(axum::http::header::ACCEPT, "application/json")
                .header(axum::http::header::USER_AGENT, aigw_outbound::USER_AGENT);
            for (k, v) in &call.headers {
                req = req.header(*k, v.as_str());
            }
            let req = req
                .body(Bytes::from(call.body))
                .map_err(|e| PluginError::new(format!("Anfrage ungültig: {e}")))?;
            let reply = inner
                .client
                .fetch(req, &FetchLimits::new(call.timeout, MAX_REPLY_BYTES))
                .await
                .map_err(|e| match e {
                    aigw_outbound::OutboundError::Timeout(_) => {
                        PluginError::new("Zeitlimit des Dienstes überschritten")
                    }
                    aigw_outbound::OutboundError::Blocked(m) => {
                        PluginError::new(format!("Ziel nicht erlaubt: {m}"))
                    }
                    other => PluginError::new(format!("Dienst nicht erreichbar: {other}")),
                })?;
            Ok(HttpReply {
                status: reply.status.as_u16(),
                body: reply.body.to_vec(),
            })
        })
    }

    fn chat(
        &self,
        model_group: &str,
        req: ChatRequest,
    ) -> BoxFuture<'static, Result<ChatResponse, PluginError>> {
        let inner = Arc::clone(&self.0);
        let group = model_group.to_owned();
        Box::pin(async move {
            let route = inner
                .routes
                .route(&group)
                .ok_or_else(|| PluginError::new(format!("Modellgruppe {group:?} unbekannt")))?;
            let tokens = crate::observe::estimate_tokens(crate::observe::request_bytes(&req))
                + req.max_tokens.unwrap_or(256);
            let routed = exec::execute(
                inner.routes.as_ref(),
                route,
                req,
                CapacityCtx {
                    counters: &inner.counters,
                    tokens,
                },
            )
            .await;
            match routed.result {
                Ok(Outcome::Json(resp)) => Ok(resp),
                Ok(Outcome::Stream(_)) => Err(PluginError::new("Prüfmodell hat gestreamt")),
                Err(e) => Err(PluginError::new(format!(
                    "Prüfmodell {group:?}: {}",
                    e.error.message
                ))),
            }
        })
    }
}
