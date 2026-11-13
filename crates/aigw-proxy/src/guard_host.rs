// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! [`PluginHost`] des Gateways (AIGW-38): Zugangsdaten aus dem aktuellen Snapshot,
//! HTTP-Aufrufe mit SSRF-Regeln (`aigw_core::upstream`) und Modellaufrufe über das eigene
//! Routing – ohne Plugin-Kette (keine Rekursion) und ohne Limits des Clients.

use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;

use aigw_core::ir::{ChatRequest, ChatResponse};
use aigw_core::resource::CredentialData;
use aigw_core::snapshot::SnapshotHandle;
use aigw_core::upstream::UpstreamPolicy;
use aigw_counters::CounterBackend;
use aigw_plugins::{BoxFuture, HttpCall, HttpReply, PluginError, PluginHost};
use aigw_providers::UpstreamClient;
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
    client: UpstreamClient,
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
    /// SSRF-Regeln und HTTP-Client.
    pub fn new(
        routes: Arc<dyn ModelRoutes>,
        counters: Arc<dyn CounterBackend>,
        snapshot: Option<SnapshotHandle>,
        policy: UpstreamPolicy,
        client: UpstreamClient,
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
    /// Wie [`GatewayHost::new`] mit eigenem HTTP-Client (Standardeinstellungen).
    pub fn with_defaults(
        routes: Arc<dyn ModelRoutes>,
        counters: Arc<dyn CounterBackend>,
        snapshot: Option<SnapshotHandle>,
        policy: UpstreamPolicy,
    ) -> Result<Self, String> {
        let client = UpstreamClient::with_defaults().map_err(|e| e.to_string())?;
        Ok(Self::new(routes, counters, snapshot, policy, client))
    }
}

/// Prüft die aufgelösten Adressen des Ziels (Namen, die auf private Adressen zeigen).
async fn check_resolved(policy: &UpstreamPolicy, url: &str) -> Result<(), PluginError> {
    let uri: axum::http::Uri = url.parse().map_err(|_| PluginError::new("ungültige URL"))?;
    let host = uri
        .host()
        .ok_or_else(|| PluginError::new("URL ohne Host"))?
        .trim_start_matches('[')
        .trim_end_matches(']');
    let port = uri.port_u16().unwrap_or(match uri.scheme_str() {
        Some("http") => 80,
        _ => 443,
    });
    let addrs: Vec<IpAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![ip],
        Err(_) => tokio::net::lookup_host((host, port))
            .await
            .map_err(|e| PluginError::new(format!("Namensauflösung: {e}")))?
            .map(|a| a.ip())
            .collect(),
    };
    if addrs.is_empty() || addrs.iter().any(|ip| !policy.ip_allowed(*ip)) {
        return Err(PluginError::new(format!(
            "Ziel {host} löst auf eine nicht erlaubte Adresse auf"
        )));
    }
    Ok(())
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
            let fut = async {
                check_resolved(&inner.policy, &call.url).await?;
                let mut req = axum::http::Request::post(&call.url)
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .header(axum::http::header::ACCEPT, "application/json");
                for (k, v) in &call.headers {
                    req = req.header(*k, v.as_str());
                }
                let req = req
                    .body(Bytes::from(call.body))
                    .map_err(|e| PluginError::new(format!("Anfrage ungültig: {e}")))?;
                let (status, body) = inner
                    .client
                    .fetch(req, MAX_REPLY_BYTES)
                    .await
                    .map_err(|e| PluginError::new(format!("Dienst nicht erreichbar: {e}")))?;
                Ok(HttpReply {
                    status: status.as_u16(),
                    body: body.to_vec(),
                })
            };
            tokio::time::timeout(call.timeout, fut)
                .await
                .map_err(|_| PluginError::new("Zeitlimit des Dienstes überschritten"))?
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
