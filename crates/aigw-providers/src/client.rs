// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Gemeinsamer Upstream-HTTP-Client der Anbieter-Adapter (Architektur §2, §6).
//!
//! Dünne Hülle um [`aigw_outbound::OutboundClient`] (AIGW-72): eigene Namensauflösung mit den
//! SSRF-Regeln aus `admin.allow_*_upstreams` (auch nach der Auflösung – schützt vor Namen, die
//! auf private Adressen zeigen, und DNS-Rebinding), keine Weiterleitungen, rustls mit
//! aws-lc-rs und den Vertrauensankern aus `upstream.roots`/`upstream.ca_file`, ALPN `h2` +
//! `http/1.1`, TCP-Keep-Alive und `TCP_NODELAY`. Der Verbindungspool ist je Endpunkt
//! (Schema + Host + Port) getrennt; HTTP/2-Verbindungen werden für parallele Anfragen
//! gemultiplext und mit Pings offen gehalten. Klartext-HTTP ist für freigeschaltete lokale
//! Anbieter (vLLM, Ollama) erlaubt. Deployments mit `settings.caFile` erhalten eine Variante
//! mit eigenem Pool ([`UpstreamClient::for_ca_file`]).

use std::fmt;
use std::path::Path;
use std::time::Duration;

use aigw_core::upstream::UpstreamPolicy;
use aigw_outbound::{AddressGuard, OutboundClient, OutboundError, OutboundOptions};
use hyper::body::Incoming;

use crate::UpstreamRequest;

/// Einstellungen des Upstream-Clients (gelten für alle Anbieter).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientConfig {
    /// Zeitlimit für den TCP-Verbindungsaufbau.
    pub connect_timeout: Duration,
    /// Unbenutzte Verbindungen werden danach geschlossen.
    pub pool_idle_timeout: Duration,
    /// Höchstzahl unbenutzter Verbindungen je Endpunkt.
    pub pool_max_idle_per_host: usize,
    /// TCP-Keep-Alive.
    pub tcp_keepalive: Option<Duration>,
    /// Abstand der HTTP/2-Pings (hält Verbindungen durch NAT/Load-Balancer offen).
    pub http2_keep_alive_interval: Option<Duration>,
    /// Zeitlimit für die Ping-Antwort.
    pub http2_keep_alive_timeout: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            pool_idle_timeout: Duration::from_secs(90),
            pool_max_idle_per_host: 1024,
            tcp_keepalive: Some(Duration::from_secs(60)),
            http2_keep_alive_interval: Some(Duration::from_secs(30)),
            http2_keep_alive_timeout: Duration::from_secs(10),
        }
    }
}

impl ClientConfig {
    fn outbound(&self) -> OutboundOptions {
        OutboundOptions {
            connect_timeout: self.connect_timeout,
            pool_idle_timeout: self.pool_idle_timeout,
            pool_max_idle_per_host: self.pool_max_idle_per_host,
            tcp_keepalive: self.tcp_keepalive,
            http2: true,
            http2_keep_alive_interval: self.http2_keep_alive_interval,
            http2_keep_alive_timeout: self.http2_keep_alive_timeout,
            http2_adaptive_window: true,
            tls: None,
        }
    }
}

/// Upstream-HTTP-Client (günstig klonbar, alle Klone teilen den Pool).
#[derive(Clone)]
pub struct UpstreamClient {
    inner: OutboundClient,
    /// `upstream.ca_dir` (F177): einziges Verzeichnis für `settings.caFile`.
    ca_dir: Option<std::sync::Arc<std::path::PathBuf>>,
}

impl fmt::Debug for UpstreamClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UpstreamClient")
            .field("guard", self.inner.guard())
            .finish_non_exhaustive()
    }
}

impl UpstreamClient {
    /// Baut den Client mit den SSRF-Regeln `policy` (Betrieb: aus `admin.*`). Muss innerhalb
    /// einer Tokio-Laufzeit benutzt werden.
    pub fn new(cfg: &ClientConfig, policy: UpstreamPolicy) -> Result<Self, OutboundError> {
        let ca_dir = policy
            .ca_dir()
            .map(|d| std::sync::Arc::new(d.to_path_buf()));
        let mut c = Self::with_guard(cfg, AddressGuard::policy(policy))?;
        c.ca_dir = ca_dir;
        Ok(c)
    }

    /// Baut den Client mit beliebigem Adressfilter.
    pub fn with_guard(cfg: &ClientConfig, guard: AddressGuard) -> Result<Self, OutboundError> {
        OutboundClient::builder(guard)
            .options(cfg.outbound())
            .build()
            .map(Self::from_outbound)
    }

    /// Hülle um einen fertigen [`OutboundClient`] (z. B. mit eigenem Resolver in Tests).
    pub fn from_outbound(inner: OutboundClient) -> Self {
        Self {
            inner,
            ca_dir: None,
        }
    }

    /// Client mit Standardeinstellungen und den SSRF-Regeln `policy`.
    pub fn with_policy(policy: UpstreamPolicy) -> Result<Self, OutboundError> {
        Self::new(&ClientConfig::default(), policy)
    }

    /// Client mit Standardeinstellungen und **strengen** SSRF-Regeln (keine privaten oder
    /// lokalen Ziele, auch nicht nach der Namensauflösung).
    pub fn with_defaults() -> Result<Self, OutboundError> {
        Self::with_policy(UpstreamPolicy::strict())
    }

    /// Client **ohne** Adressfilter – nur für Tests und Benchmarks gegen lokale Server.
    pub fn unguarded() -> Result<Self, OutboundError> {
        Self::with_guard(&ClientConfig::default(), AddressGuard::Trusted)
    }

    /// Setzt `upstream.ca_dir` (F177) für `settings.caFile` (bei [`UpstreamClient::new`] aus
    /// der Policy übernommen).
    #[must_use]
    pub fn with_ca_dir(mut self, dir: Option<std::path::PathBuf>) -> Self {
        self.ca_dir = dir.map(std::sync::Arc::new);
        self
    }

    /// Der zugrunde liegende Outbound-Client.
    pub fn outbound(&self) -> &OutboundClient {
        &self.inner
    }

    /// Variante mit zusätzlicher CA-Datei (`settings.caFile` eines Deployments; eigener Pool,
    /// gleiche SSRF-Regeln). Die Datei muss in `upstream.ca_dir` liegen (F177: relativ =
    /// darin; Symlinks, die hinausführen, werden abgelehnt).
    pub fn for_ca_file(&self, path: &Path) -> Result<Self, OutboundError> {
        let Some(dir) = &self.ca_dir else {
            return Err(OutboundError::Tls(
                "nur mit upstream.ca_dir in der Startkonfiguration erlaubt (F177)".into(),
            ));
        };
        let real = aigw_core::paths::resolve_within(dir, &path.to_string_lossy())
            .map_err(OutboundError::Tls)?;
        self.inner.with_ca_file(&real).map(|inner| Self {
            inner,
            ca_dir: self.ca_dir.clone(),
        })
    }

    /// Variante mit anderem Zeitlimit für den Verbindungsaufbau (`settings.timeouts.
    /// connectMs` eines Deployments, F187; eigener Pool, gleiche SSRF-Regeln).
    pub fn for_connect_timeout(&self, connect: Duration) -> Result<Self, OutboundError> {
        self.inner
            .with_connect_timeout(connect)
            .map(Self::from_outbound)
    }

    /// Sendet eine Anfrage; das Ergebnis liegt vor, sobald die Antwort-Header da sind.
    pub(crate) async fn send(
        &self,
        req: UpstreamRequest,
    ) -> Result<http::Response<Incoming>, OutboundError> {
        self.inner.send(req).await
    }
}
