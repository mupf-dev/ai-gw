// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! [`OutboundClient`]: hyper-util-Client mit SSRF-Resolver, rustls und Limits.

use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use http::header::LOCATION;
use http::{HeaderMap, Request, Response, StatusCode};
use http_body_util::{BodyExt as _, Full, Limited};
use hyper::body::Incoming;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioTimer};

use crate::error::OutboundError;
use crate::resolve::{AddressGuard, GuardedResolver, Resolve, SystemResolver};
use crate::tls::{Fingerprint, TlsOptions, default_tls};

/// Einstellungen eines Clients (Verbindungsaufbau, Pool, HTTP/2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundOptions {
    /// Zeitlimit für den TCP-Verbindungsaufbau.
    pub connect_timeout: Duration,
    /// Unbenutzte Verbindungen werden danach geschlossen.
    pub pool_idle_timeout: Duration,
    /// Höchstzahl unbenutzter Verbindungen je Endpunkt.
    pub pool_max_idle_per_host: usize,
    /// TCP-Keep-Alive.
    pub tcp_keepalive: Option<Duration>,
    /// HTTP/2 per ALPN anbieten (sonst nur HTTP/1.1).
    pub http2: bool,
    /// Abstand der HTTP/2-Pings (hält Verbindungen durch NAT/Load-Balancer offen).
    pub http2_keep_alive_interval: Option<Duration>,
    /// Zeitlimit für die Ping-Antwort.
    pub http2_keep_alive_timeout: Duration,
    /// Adaptives HTTP/2-Flusskontrollfenster (große Antworten, Streams).
    pub http2_adaptive_window: bool,
    /// Vertrauensanker; `None` = prozessweite Standardwerte ([`crate::default_tls`]).
    pub tls: Option<TlsOptions>,
}

impl Default for OutboundOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            pool_idle_timeout: Duration::from_secs(90),
            pool_max_idle_per_host: 64,
            tcp_keepalive: Some(Duration::from_secs(60)),
            http2: true,
            http2_keep_alive_interval: None,
            http2_keep_alive_timeout: Duration::from_secs(10),
            http2_adaptive_window: false,
            tls: None,
        }
    }
}

/// Umgang mit 3xx-Antworten in [`OutboundClient::fetch`]. Weiterleitungen werden **nie**
/// automatisch verfolgt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Redirects {
    /// 3xx ist ein Fehler ([`OutboundError::Redirect`]). Standard.
    #[default]
    Deny,
    /// 3xx wird als normale Antwort zurückgegeben (der Aufrufer entscheidet).
    Return,
}

/// Grenzen eines vollständig gelesenen Aufrufs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FetchLimits {
    /// Gesamtzeit inklusive Verbindungsaufbau und Body.
    pub timeout: Duration,
    /// Größte Antwort (Body) in Byte.
    pub max_body: usize,
    /// Umgang mit Weiterleitungen.
    pub redirects: Redirects,
}

impl FetchLimits {
    /// Grenzen mit `timeout` und `max_body`, Weiterleitungen verboten.
    pub fn new(timeout: Duration, max_body: usize) -> Self {
        Self {
            timeout,
            max_body,
            redirects: Redirects::Deny,
        }
    }
}

/// Vollständig gelesene Antwort.
#[derive(Debug, Clone)]
pub struct Reply {
    /// Status.
    pub status: StatusCode,
    /// Header.
    pub headers: HeaderMap,
    /// Body.
    pub body: Bytes,
}

type Inner = Client<HttpsConnector<HttpConnector<GuardedResolver>>, Full<Bytes>>;

/// Schlüssel einer Pool-Variante: Vertrauensanker und Zeitlimit des Verbindungsaufbaus.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct VariantKey {
    tls: TlsOptions,
    connect: Duration,
}

/// Gemeinsamer Teil aller Varianten eines Clients.
struct Base {
    options: OutboundOptions,
    resolver: Arc<dyn Resolve>,
    /// Vertrauensanker der Basisvariante.
    tls: TlsOptions,
    /// Pool der Basisvariante.
    inner: Inner,
    /// Weitere Varianten mit anderen Vertrauensankern bzw. anderem Zeitlimit für den
    /// Verbindungsaufbau (eigener Pool je Variante).
    variants: Mutex<HashMap<VariantKey, (Fingerprint, Inner)>>,
}

/// Client für ausgehende Verbindungen (günstig klonbar, Klone teilen den Pool).
#[derive(Clone)]
pub struct OutboundClient {
    inner: Inner,
    guard: AddressGuard,
    tls: TlsOptions,
    /// Zeitlimit des Verbindungsaufbaus dieser Variante.
    connect: Duration,
    base: Arc<Base>,
}

impl fmt::Debug for OutboundClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutboundClient")
            .field("guard", &self.guard)
            .field("tls", &self.tls)
            .finish_non_exhaustive()
    }
}

/// Baut einen [`OutboundClient`].
#[derive(Debug)]
pub struct OutboundBuilder {
    guard: AddressGuard,
    options: OutboundOptions,
    resolver: Arc<dyn Resolve>,
    rustls: Option<rustls::ClientConfig>,
}

impl OutboundBuilder {
    /// Einstellungen.
    #[must_use]
    pub fn options(mut self, options: OutboundOptions) -> Self {
        self.options = options;
        self
    }

    /// Eigene Namensauflösung (Standard: [`SystemResolver`]).
    #[must_use]
    pub fn resolver(mut self, resolver: Arc<dyn Resolve>) -> Self {
        self.resolver = resolver;
        self
    }

    /// Fertige rustls-Konfiguration statt [`TlsOptions`] (z. B. mit Client-Zertifikat für
    /// mTLS). ALPN setzt der Client selbst.
    #[must_use]
    pub fn rustls_config(mut self, cfg: rustls::ClientConfig) -> Self {
        self.rustls = Some(cfg);
        self
    }

    /// Baut den Client (innerhalb einer Tokio-Laufzeit benutzen).
    pub fn build(self) -> Result<OutboundClient, OutboundError> {
        let tls = self.options.tls.clone().unwrap_or_else(default_tls);
        let rustls = match self.rustls {
            Some(mut cfg) => {
                cfg.alpn_protocols.clear();
                cfg
            }
            None => (*tls.client_config()?).clone(),
        };
        let inner = build_inner(&self.options, &self.guard, &self.resolver, rustls);
        Ok(OutboundClient {
            inner: inner.clone(),
            guard: self.guard,
            tls: tls.clone(),
            connect: self.options.connect_timeout,
            base: Arc::new(Base {
                options: self.options,
                resolver: self.resolver,
                tls,
                inner,
                variants: Mutex::new(HashMap::new()),
            }),
        })
    }
}

fn build_inner(
    opts: &OutboundOptions,
    guard: &AddressGuard,
    resolver: &Arc<dyn Resolve>,
    tls: rustls::ClientConfig,
) -> Inner {
    let mut http = HttpConnector::new_with_resolver(GuardedResolver {
        guard: guard.clone(),
        resolver: Arc::clone(resolver),
    });
    http.enforce_http(false);
    http.set_nodelay(true);
    http.set_connect_timeout(Some(opts.connect_timeout));
    http.set_keepalive(opts.tcp_keepalive);
    let builder = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls)
        .https_or_http()
        .enable_http1();
    let https = if opts.http2 {
        builder.enable_http2().wrap_connector(http)
    } else {
        builder.wrap_connector(http)
    };
    let mut b = Client::builder(TokioExecutor::new());
    b.timer(TokioTimer::new())
        .pool_timer(TokioTimer::new())
        .pool_idle_timeout(opts.pool_idle_timeout)
        .pool_max_idle_per_host(opts.pool_max_idle_per_host);
    if opts.http2 {
        b.http2_keep_alive_interval(opts.http2_keep_alive_interval)
            .http2_keep_alive_timeout(opts.http2_keep_alive_timeout)
            .http2_keep_alive_while_idle(opts.http2_keep_alive_interval.is_some())
            .http2_adaptive_window(opts.http2_adaptive_window);
    }
    b.build(https)
}

impl OutboundClient {
    /// Builder mit Adressfilter `guard`.
    pub fn builder(guard: AddressGuard) -> OutboundBuilder {
        OutboundBuilder {
            guard,
            options: OutboundOptions::default(),
            resolver: Arc::new(SystemResolver),
            rustls: None,
        }
    }

    /// Client mit Standardeinstellungen.
    pub fn new(guard: AddressGuard) -> Result<Self, OutboundError> {
        Self::builder(guard).build()
    }

    /// Adressfilter.
    pub fn guard(&self) -> &AddressGuard {
        &self.guard
    }

    /// Vertrauensanker dieses Clients.
    pub fn tls(&self) -> &TlsOptions {
        &self.tls
    }

    /// Einstellungen.
    pub fn options(&self) -> &OutboundOptions {
        &self.base.options
    }

    /// Zeitlimit des Verbindungsaufbaus dieser Variante.
    pub fn connect_timeout(&self) -> Duration {
        self.connect
    }

    /// Variante mit anderen Vertrauensankern (gleicher Adressfilter, gleiche Einstellungen,
    /// eigener Pool). Varianten werden wiederverwendet; ändert sich eine CA-Datei, entsteht
    /// beim nächsten Aufruf ein neuer Pool.
    pub fn with_tls(&self, tls: TlsOptions) -> Result<Self, OutboundError> {
        self.variant(tls, self.connect)
    }

    /// Variante mit anderem Zeitlimit für den Verbindungsaufbau (z. B. `settings.timeouts.
    /// connectMs` eines Deployments, F187; eigener Pool, gleiche Vertrauensanker und
    /// SSRF-Regeln). Varianten werden wiederverwendet.
    pub fn with_connect_timeout(&self, connect: Duration) -> Result<Self, OutboundError> {
        self.variant(self.tls.clone(), connect)
    }

    fn variant(&self, tls: TlsOptions, connect: Duration) -> Result<Self, OutboundError> {
        if tls == self.base.tls && connect == self.base.options.connect_timeout {
            return Ok(Self {
                inner: self.base.inner.clone(),
                guard: self.guard.clone(),
                tls,
                connect,
                base: Arc::clone(&self.base),
            });
        }
        let (fp, cfg) = tls.resolve()?;
        let key = VariantKey {
            tls: tls.clone(),
            connect,
        };
        let mut variants = self
            .base
            .variants
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let inner = match variants.get(&key) {
            Some((f, inner)) if *f == fp => inner.clone(),
            _ => {
                let options = OutboundOptions {
                    connect_timeout: connect,
                    ..self.base.options.clone()
                };
                let inner = build_inner(&options, &self.guard, &self.base.resolver, (*cfg).clone());
                variants.insert(key, (fp, inner.clone()));
                inner
            }
        };
        Ok(Self {
            inner,
            guard: self.guard.clone(),
            tls,
            connect,
            base: Arc::clone(&self.base),
        })
    }

    /// Variante mit einer zusätzlichen CA-Datei (z. B. `settings.caFile` eines Deployments).
    pub fn with_ca_file(&self, path: &std::path::Path) -> Result<Self, OutboundError> {
        self.variant(self.base.tls.clone().with_ca_file(path), self.connect)
    }

    /// Prüft Schema und – bei IP-Literalen, die die Namensauflösung umgehen – die Zieladresse.
    pub fn check_target(&self, uri: &http::Uri) -> Result<(), OutboundError> {
        match uri.scheme_str() {
            Some("http" | "https") => {}
            _ => {
                return Err(OutboundError::Request(format!(
                    "{uri}: nur http:// und https:// erlaubt"
                )));
            }
        }
        let host = uri
            .host()
            .ok_or_else(|| OutboundError::Request(format!("{uri}: Host fehlt")))?;
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        if let Ok(ip) = bare.parse::<IpAddr>()
            && !self.guard.allows(bare, ip)
        {
            return Err(OutboundError::Blocked(format!(
                "Zieladresse {ip} ist nicht erlaubt – SSRF-Schutz, Freischaltung über \
                 admin.allow_private_upstreams"
            )));
        }
        Ok(())
    }

    /// Sendet eine Anfrage; fertig, sobald die Antwort-Header da sind. Kein Zeitlimit außer
    /// dem Verbindungsaufbau (der Aufrufer begrenzt, z. B. mit `tokio::time::timeout`).
    /// Weiterleitungen werden nicht verfolgt.
    pub async fn send(&self, req: Request<Bytes>) -> Result<Response<Incoming>, OutboundError> {
        self.check_target(req.uri())?;
        self.inner
            .request(req.map(Full::new))
            .await
            .map_err(|e| OutboundError::from_client(&e))
    }

    /// Sendet eine Anfrage und liest die Antwort vollständig – innerhalb von `limits`.
    pub async fn fetch(
        &self,
        req: Request<Bytes>,
        limits: &FetchLimits,
    ) -> Result<Reply, OutboundError> {
        let max = limits.max_body;
        let call = async {
            let resp = self.send(req).await?;
            let (parts, body) = resp.into_parts();
            if parts.status.is_redirection() && limits.redirects == Redirects::Deny {
                return Err(OutboundError::Redirect {
                    status: parts.status.as_u16(),
                    location: parts
                        .headers
                        .get(LOCATION)
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.chars().take(256).collect()),
                });
            }
            let body = Limited::new(body, max)
                .collect()
                .await
                .map_err(|e| {
                    if e.is::<http_body_util::LengthLimitError>() {
                        OutboundError::TooLarge(max)
                    } else {
                        OutboundError::Connect(crate::error::error_chain(&*e))
                    }
                })?
                .to_bytes();
            Ok(Reply {
                status: parts.status,
                headers: parts.headers,
                body,
            })
        };
        tokio::time::timeout(limits.timeout, call)
            .await
            .unwrap_or(Err(OutboundError::Timeout(limits.timeout)))
    }
}
