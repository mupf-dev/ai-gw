// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! HTTP-Client zu Upstream-MCP-Servern mit SSRF-Schutz beim Verbindungsaufbau.
//!
//! Dünne Hülle um den gemeinsamen Outbound-Baustein [`aigw_outbound::OutboundClient`]
//! (AIGW-72). Die URL eines Servers wird beim Schreiben (Admin-API/GitOps) und vor jedem
//! Verbindungsaufbau mit [`UpstreamPolicy::check_endpoint`] geprüft. Zusätzlich filtert die
//! eigene Namensauflösung die aufgelösten Adressen mit [`UpstreamPolicy::resolved_ip_allowed`]
//! – ein öffentlicher Name, der (auch per DNS-Rebinding) auf eine private Adresse zeigt, wird
//! so nicht verbunden. IP-Literale prüft der Baustein direkt. Weiterleitungen (3xx) folgt der
//! Client nicht; Vertrauensanker aus `upstream.roots`/`upstream.ca_file`.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use aigw_core::upstream::UpstreamPolicy;
use aigw_outbound::{AddressGuard, OutboundClient, OutboundOptions};
use bytes::Bytes;
use hyper::body::Incoming;

/// HTTP-Client (günstig klonbar, Klone teilen den Verbindungspool).
#[derive(Clone)]
pub struct HttpClient {
    inner: OutboundClient,
    policy: Arc<UpstreamPolicy>,
}

impl fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpClient").finish_non_exhaustive()
    }
}

impl HttpClient {
    /// Baut den Client (innerhalb einer Tokio-Laufzeit benutzen).
    pub fn new(policy: Arc<UpstreamPolicy>, connect_timeout: Duration) -> Result<Self, String> {
        let inner = OutboundClient::builder(AddressGuard::Policy(Arc::clone(&policy)))
            .options(OutboundOptions {
                connect_timeout,
                ..OutboundOptions::default()
            })
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self { inner, policy })
    }

    /// Regeln für Upstream-Endpunkte.
    pub fn policy(&self) -> &UpstreamPolicy {
        &self.policy
    }

    /// Sendet eine Anfrage; fertig, sobald die Antwort-Header da sind. Der Aufrufer prüft die
    /// URL vorher mit [`UpstreamPolicy::check_endpoint`] (Schema, Zugangsdaten, Namen).
    pub(crate) async fn send(
        &self,
        req: http::Request<Bytes>,
    ) -> Result<http::Response<Incoming>, String> {
        self.inner.send(req).await.map_err(|e| e.to_string())
    }
}
