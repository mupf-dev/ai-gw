// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! HTTP-Client zu Upstream-MCP-Servern mit SSRF-Schutz beim Verbindungsaufbau.
//!
//! Die URL eines Servers wird beim Schreiben (Admin-API/GitOps) und vor jedem Verbindungsaufbau
//! mit [`UpstreamPolicy::check_endpoint`] geprüft. Zusätzlich filtert der eigene DNS-Resolver
//! die aufgelösten Adressen mit [`UpstreamPolicy::resolved_ip_allowed`] – ein öffentlicher
//! Name, der (auch per DNS-Rebinding) auf eine private Adresse zeigt, wird so nicht
//! verbunden. Weiterleitungen (3xx) folgt der Client nicht.

use std::fmt;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use aigw_core::upstream::UpstreamPolicy;
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::connect::dns::Name;
use hyper_util::rt::{TokioExecutor, TokioTimer};

/// DNS-Resolver, der nur nach [`UpstreamPolicy`] erlaubte Adressen liefert.
#[derive(Clone)]
pub(crate) struct GuardedResolver {
    policy: Arc<UpstreamPolicy>,
}

impl tower::Service<Name> for GuardedResolver {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, io::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        let policy = Arc::clone(&self.policy);
        Box::pin(async move {
            let host = name.as_str().to_owned();
            let all: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let allowed: Vec<SocketAddr> = all
                .iter()
                .copied()
                .filter(|a| policy.resolved_ip_allowed(&host, a.ip()))
                .collect();
            if allowed.is_empty() {
                tracing::warn!(host = %host, addrs = ?all, "MCP-Upstream: Ziel durch SSRF-Regeln gesperrt");
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{host} löst nur auf gesperrte Adressen auf (SSRF-Schutz)"),
                ));
            }
            Ok(allowed.into_iter())
        })
    }
}

type Inner = Client<HttpsConnector<HttpConnector<GuardedResolver>>, Full<Bytes>>;

/// HTTP-Client (günstig klonbar, Klone teilen den Verbindungspool).
#[derive(Clone)]
pub struct HttpClient {
    inner: Inner,
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
        let mut http = HttpConnector::new_with_resolver(GuardedResolver {
            policy: Arc::clone(&policy),
        });
        http.enforce_http(false);
        http.set_nodelay(true);
        http.set_connect_timeout(Some(connect_timeout));
        http.set_keepalive(Some(Duration::from_secs(60)));
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_provider_and_webpki_roots(rustls::crypto::aws_lc_rs::default_provider())
            .map_err(|e| e.to_string())?
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .wrap_connector(http);
        let inner = Client::builder(TokioExecutor::new())
            .timer(TokioTimer::new())
            .pool_timer(TokioTimer::new())
            .pool_idle_timeout(Duration::from_secs(90))
            .build(https);
        Ok(Self { inner, policy })
    }

    /// Regeln für Upstream-Endpunkte.
    pub fn policy(&self) -> &UpstreamPolicy {
        &self.policy
    }

    /// Sendet eine Anfrage; fertig, sobald die Antwort-Header da sind. IP-Literale in der URL
    /// laufen nicht über den Resolver – daher prüft der Aufrufer die URL vorher mit
    /// [`UpstreamPolicy::check_endpoint`].
    pub(crate) async fn send(
        &self,
        req: http::Request<Bytes>,
    ) -> Result<http::Response<Incoming>, String> {
        self.inner.request(req.map(Full::new)).await.map_err(|e| {
            use std::error::Error as _;
            let mut msg = e.to_string();
            let mut src = e.source();
            while let Some(s) = src {
                msg.push_str(": ");
                msg.push_str(&s.to_string());
                src = s.source();
            }
            msg
        })
    }
}
