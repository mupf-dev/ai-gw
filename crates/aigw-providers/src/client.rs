// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Gemeinsamer Upstream-HTTP-Client (Architektur §2, §6).
//!
//! hyper-util-Client mit rustls (aws-lc-rs, Mozilla-Wurzelzertifikate), ALPN `h2` +
//! `http/1.1`, TCP-Keep-Alive und `TCP_NODELAY`. Der Verbindungspool ist je Endpunkt
//! (Schema + Host + Port) getrennt; HTTP/2-Verbindungen werden für parallele Anfragen
//! gemultiplext und mit Pings offen gehalten. Klartext-HTTP ist für lokale Anbieter (vLLM,
//! Ollama, Testserver) erlaubt.

use std::fmt;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioTimer};

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

type Inner = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

/// Upstream-HTTP-Client (günstig klonbar, alle Klone teilen den Pool).
#[derive(Clone)]
pub struct UpstreamClient {
    inner: Inner,
}

impl fmt::Debug for UpstreamClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UpstreamClient").finish_non_exhaustive()
    }
}

impl UpstreamClient {
    /// Baut den Client. Muss innerhalb einer Tokio-Laufzeit benutzt werden.
    pub fn new(cfg: &ClientConfig) -> Result<Self, rustls::Error> {
        let mut http = HttpConnector::new();
        http.enforce_http(false);
        http.set_nodelay(true);
        http.set_connect_timeout(Some(cfg.connect_timeout));
        http.set_keepalive(cfg.tcp_keepalive);

        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_provider_and_webpki_roots(rustls::crypto::aws_lc_rs::default_provider())?
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .wrap_connector(http);

        let inner = Client::builder(TokioExecutor::new())
            .timer(TokioTimer::new())
            .pool_timer(TokioTimer::new())
            .pool_idle_timeout(cfg.pool_idle_timeout)
            .pool_max_idle_per_host(cfg.pool_max_idle_per_host)
            .http2_keep_alive_interval(cfg.http2_keep_alive_interval)
            .http2_keep_alive_timeout(cfg.http2_keep_alive_timeout)
            .http2_keep_alive_while_idle(true)
            .http2_adaptive_window(true)
            .build(https);
        Ok(Self { inner })
    }

    /// Client mit Standardeinstellungen.
    pub fn with_defaults() -> Result<Self, rustls::Error> {
        Self::new(&ClientConfig::default())
    }

    /// Sendet eine Anfrage; das Ergebnis liegt vor, sobald die Antwort-Header da sind.
    pub(crate) async fn send(
        &self,
        req: UpstreamRequest,
    ) -> Result<http::Response<Incoming>, hyper_util::client::legacy::Error> {
        self.inner.request(req.map(Full::new)).await
    }
}
