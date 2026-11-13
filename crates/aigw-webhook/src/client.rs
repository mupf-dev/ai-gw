// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! HTTP-Client für ausgehende Webhooks mit SSRF-Schutz.
//!
//! - URL-Prüfung beim Konfigurieren ([`WebhookClient::check_url`], Regeln wie Deployments).
//! - Eigene Namensauflösung: Jede aufgelöste Adresse wird gegen die Regeln geprüft
//!   ([`UpstreamPolicy::resolved_ip_allowed`]); bleibt keine übrig, scheitert der
//!   Verbindungsaufbau. Damit fällt auch ein öffentlicher Name auf, der (per DNS-Rebinding)
//!   auf eine private Adresse zeigt.
//! - Keine Weiterleitungen (3xx ist ein Fehler), Antwortgröße begrenzt, Zeitlimit für den
//!   ganzen Aufruf inklusive Body.
//! - Eigener Keep-Alive-Pool (getrennt von den Anbieter-Verbindungen), HTTP/2 und HTTP/1.1.
//! - mTLS-Erweiterungspunkt: [`ClientOptions::tls`] nimmt eine fertige rustls-Konfiguration
//!   (z. B. mit `with_client_auth_cert`); eine Konfigurationsoberfläche dafür folgt später.

use std::fmt;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use aigw_core::resource::SecretString;
use aigw_core::upstream::UpstreamPolicy;
use bytes::Bytes;
use http::header::{CONTENT_TYPE, HeaderValue, USER_AGENT};
use http::{HeaderMap, Method, Request, StatusCode, Uri};
use http_body_util::{BodyExt, Full, Limited};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::connect::dns::Name;
use hyper_util::rt::{TokioExecutor, TokioTimer};

use crate::sign;

/// Header mit der Request-ID der auslösenden Anfrage (Plugin-Webhooks).
pub const REQUEST_ID_HEADER: &str = "x-aigw-request-id";
/// Header mit einer eindeutigen ID je Zustellung (für Idempotenz/Replay-Schutz beim
/// Empfänger; bei Ereignissen über Wiederholungen hinweg gleich = Ereignis-ID).
pub const DELIVERY_HEADER: &str = "x-aigw-delivery";

/// Einstellungen des Webhook-Clients.
#[derive(Clone)]
pub struct ClientOptions {
    /// Zeitlimit für den TCP-Verbindungsaufbau.
    pub connect_timeout: Duration,
    /// Unbenutzte Verbindungen werden danach geschlossen.
    pub pool_idle_timeout: Duration,
    /// Höchstzahl unbenutzter Verbindungen je Ziel.
    pub pool_max_idle_per_host: usize,
    /// Eigene TLS-Konfiguration (mTLS-Client-Zertifikat, eigene CAs). `None` =
    /// Mozilla-Wurzelzertifikate (webpki-roots), aws-lc-rs.
    pub tls: Option<rustls::ClientConfig>,
}

impl fmt::Debug for ClientOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientOptions")
            .field("connect_timeout", &self.connect_timeout)
            .field("pool_idle_timeout", &self.pool_idle_timeout)
            .field("pool_max_idle_per_host", &self.pool_max_idle_per_host)
            .field("tls", &self.tls.is_some())
            .finish()
    }
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(5),
            pool_idle_timeout: Duration::from_secs(90),
            pool_max_idle_per_host: 64,
            tls: None,
        }
    }
}

/// Fehler eines Webhook-Aufrufs.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WebhookError {
    /// Zeitlimit überschritten.
    #[error("Zeitlimit von {0} ms überschritten")]
    Timeout(u128),
    /// Verbindung/Übertragung fehlgeschlagen (inkl. SSRF-Ablehnung beim Auflösen).
    #[error("Verbindung fehlgeschlagen: {0}")]
    Transport(String),
    /// Antwort größer als erlaubt.
    #[error("Antwort größer als {0} Byte")]
    ResponseTooLarge(usize),
    /// Anfrage ließ sich nicht bauen (ungültiger Header o. ä.).
    #[error("ungültige Anfrage: {0}")]
    Request(String),
}

impl WebhookError {
    /// Lohnt eine Wiederholung?
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Timeout(_) | Self::Transport(_))
    }
}

/// Antwort eines Webhooks.
#[derive(Debug, Clone)]
pub struct WebhookResponse {
    /// Status.
    pub status: StatusCode,
    /// Header.
    pub headers: HeaderMap,
    /// Body (höchstens `max_response_bytes`).
    pub body: Bytes,
}

/// DNS-Auflösung mit SSRF-Filter.
#[derive(Clone)]
struct GuardedResolver {
    policy: Arc<UpstreamPolicy>,
}

type ResolveFuture =
    Pin<Box<dyn Future<Output = Result<std::vec::IntoIter<SocketAddr>, io::Error>> + Send>>;

impl tower_service::Service<Name> for GuardedResolver {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = io::Error;
    type Future = ResolveFuture;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        let policy = Arc::clone(&self.policy);
        Box::pin(async move {
            let host = name.as_str().to_owned();
            let all: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let allowed: Vec<SocketAddr> = all
                .into_iter()
                .filter(|a| policy.resolved_ip_allowed(&host, a.ip()))
                .collect();
            if allowed.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "{host}: keine erlaubte Zieladresse (SSRF-Schutz, \
                         admin.allow_private_upstreams)"
                    ),
                ));
            }
            Ok(allowed.into_iter())
        })
    }
}

type Inner = Client<HttpsConnector<HttpConnector<GuardedResolver>>, Full<Bytes>>;

/// Webhook-Client (günstig klonbar, alle Klone teilen den Pool).
#[derive(Clone)]
pub struct WebhookClient {
    inner: Inner,
    policy: Arc<UpstreamPolicy>,
}

impl fmt::Debug for WebhookClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebhookClient").finish_non_exhaustive()
    }
}

impl WebhookClient {
    /// Baut den Client mit den SSRF-Regeln `policy`.
    pub fn new(policy: UpstreamPolicy, opts: ClientOptions) -> Result<Self, rustls::Error> {
        let policy = Arc::new(policy);
        let mut http = HttpConnector::new_with_resolver(GuardedResolver {
            policy: Arc::clone(&policy),
        });
        http.enforce_http(false);
        http.set_nodelay(true);
        http.set_connect_timeout(Some(opts.connect_timeout));
        http.set_keepalive(Some(Duration::from_secs(60)));
        let builder = hyper_rustls::HttpsConnectorBuilder::new();
        let builder = match opts.tls {
            Some(cfg) => builder.with_tls_config(cfg),
            None => builder
                .with_provider_and_webpki_roots(rustls::crypto::aws_lc_rs::default_provider())?,
        };
        let https = builder
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .wrap_connector(http);
        let inner = Client::builder(TokioExecutor::new())
            .timer(TokioTimer::new())
            .pool_timer(TokioTimer::new())
            .pool_idle_timeout(opts.pool_idle_timeout)
            .pool_max_idle_per_host(opts.pool_max_idle_per_host)
            .build(https);
        Ok(Self { inner, policy })
    }

    /// Client mit strengen SSRF-Regeln und Standardeinstellungen.
    pub fn strict() -> Result<Self, rustls::Error> {
        Self::new(UpstreamPolicy::strict(), ClientOptions::default())
    }

    /// SSRF-Regeln.
    pub fn policy(&self) -> &UpstreamPolicy {
        &self.policy
    }

    /// Prüft eine Ziel-URL gegen die SSRF-Regeln (Konfiguration; Fehlertext für den Nutzer).
    pub fn check_url(&self, url: &str) -> Result<Uri, String> {
        self.policy.check_endpoint(url)?;
        url.parse::<Uri>()
            .map_err(|_| format!("URL {url:?} ist ungültig"))
    }

    /// Sendet `body` (JSON) per `POST`; mit `secret` signiert (`x-aigw-signature`).
    pub async fn post_json(
        &self,
        uri: &Uri,
        headers: HeaderMap,
        body: Bytes,
        secret: Option<&SecretString>,
        timeout: Duration,
        max_response_bytes: usize,
    ) -> Result<WebhookResponse, WebhookError> {
        let mut req = Request::builder()
            .method(Method::POST)
            .uri(uri.clone())
            .body(Full::new(body.clone()))
            .map_err(|e| WebhookError::Request(e.to_string()))?;
        let h = req.headers_mut();
        *h = headers;
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        h.insert(
            USER_AGENT,
            HeaderValue::from_static(concat!("ai-gw/", env!("CARGO_PKG_VERSION"))),
        );
        if let Some(secret) = secret {
            let ts = time::OffsetDateTime::now_utc().unix_timestamp();
            let v = sign::sign(secret.expose().as_bytes(), ts, &body);
            h.insert(
                sign::SIGNATURE_HEADER,
                HeaderValue::from_str(&v).map_err(|e| WebhookError::Request(e.to_string()))?,
            );
        }
        let call = async {
            let res = self
                .inner
                .request(req)
                .await
                .map_err(|e| WebhookError::Transport(error_chain(&e)))?;
            let (parts, body) = res.into_parts();
            let body = Limited::new(body, max_response_bytes)
                .collect()
                .await
                .map_err(|e| {
                    if e.is::<http_body_util::LengthLimitError>() {
                        WebhookError::ResponseTooLarge(max_response_bytes)
                    } else {
                        WebhookError::Transport(e.to_string())
                    }
                })?
                .to_bytes();
            Ok(WebhookResponse {
                status: parts.status,
                headers: parts.headers,
                body,
            })
        };
        match tokio::time::timeout(timeout, call).await {
            Ok(r) => r,
            Err(_) => Err(WebhookError::Timeout(timeout.as_millis())),
        }
    }
}

/// Fehlerkette als Text (hyper-Fehler nennen die Ursache erst in `source()`).
fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(s) = cur {
        out.push_str(": ");
        out.push_str(&s.to_string());
        cur = s.source();
    }
    out
}
