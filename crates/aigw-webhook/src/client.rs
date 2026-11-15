// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! HTTP-Client für ausgehende Webhooks mit SSRF-Schutz.
//!
//! Dünne Hülle um den gemeinsamen Outbound-Baustein [`aigw_outbound::OutboundClient`]
//! (AIGW-72):
//! - URL-Prüfung beim Konfigurieren ([`WebhookClient::check_url`], Regeln wie Deployments).
//! - Eigene Namensauflösung: Jede aufgelöste Adresse wird gegen die Regeln geprüft
//!   ([`UpstreamPolicy::resolved_ip_allowed`]); bleibt keine übrig, scheitert der
//!   Verbindungsaufbau ([`WebhookError::Blocked`]). Damit fällt auch ein öffentlicher Name auf,
//!   der (per DNS-Rebinding) auf eine private Adresse zeigt.
//! - Keine Weiterleitungen (3xx wird nicht verfolgt, sondern als Antwort zurückgegeben und vom
//!   Aufrufer als Fehlschlag gewertet), Antwortgröße begrenzt, Zeitlimit für den ganzen Aufruf
//!   inklusive Body.
//! - Eigener Keep-Alive-Pool (getrennt von den Anbieter-Verbindungen), HTTP/2 und HTTP/1.1,
//!   Vertrauensanker aus `upstream.roots`/`upstream.ca_file`.
//! - mTLS-Erweiterungspunkt: [`ClientOptions::tls`] nimmt eine fertige rustls-Konfiguration
//!   (z. B. mit `with_client_auth_cert`); eine Konfigurationsoberfläche dafür folgt später.

use std::fmt;
use std::time::Duration;

use aigw_core::resource::SecretString;
use aigw_core::upstream::UpstreamPolicy;
use aigw_outbound::{
    AddressGuard, FetchLimits, OutboundClient, OutboundError, OutboundOptions, Redirects,
};
use bytes::Bytes;
use http::header::{CONTENT_TYPE, HeaderValue, USER_AGENT};
use http::{HeaderMap, Method, Request, StatusCode, Uri};

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
    /// Vertrauensanker aus `upstream.roots`/`upstream.ca_file`, aws-lc-rs.
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
    /// Verbindung/Übertragung fehlgeschlagen.
    #[error("Verbindung fehlgeschlagen: {0}")]
    Transport(String),
    /// Ziel durch die SSRF-Regeln gesperrt (auch nach der Namensauflösung); keine Wiederholung.
    #[error("Ziel gesperrt: {0}")]
    Blocked(String),
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

/// Webhook-Client (günstig klonbar, alle Klone teilen den Pool).
#[derive(Clone)]
pub struct WebhookClient {
    inner: OutboundClient,
}

impl fmt::Debug for WebhookClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebhookClient").finish_non_exhaustive()
    }
}

impl WebhookClient {
    /// Baut den Client mit den SSRF-Regeln `policy`.
    pub fn new(policy: UpstreamPolicy, opts: ClientOptions) -> Result<Self, OutboundError> {
        let mut builder =
            OutboundClient::builder(AddressGuard::policy(policy)).options(OutboundOptions {
                connect_timeout: opts.connect_timeout,
                pool_idle_timeout: opts.pool_idle_timeout,
                pool_max_idle_per_host: opts.pool_max_idle_per_host,
                ..OutboundOptions::default()
            });
        if let Some(tls) = opts.tls {
            builder = builder.rustls_config(tls);
        }
        Ok(Self {
            inner: builder.build()?,
        })
    }

    /// Variante nur für öffentliche Ziele (S-22): ohne `admin.allow_private_upstreams`,
    /// eigener Pool. Für Webhooks aus Bindungen auf Organisation/Team/Key.
    pub fn public_only(&self) -> Result<Self, OutboundError> {
        Ok(Self {
            inner: self.inner.public_only()?,
        })
    }

    /// Client mit strengen SSRF-Regeln und Standardeinstellungen.
    pub fn strict() -> Result<Self, OutboundError> {
        Self::new(UpstreamPolicy::strict(), ClientOptions::default())
    }

    /// SSRF-Regeln.
    pub fn policy(&self) -> &UpstreamPolicy {
        static STRICT: std::sync::OnceLock<UpstreamPolicy> = std::sync::OnceLock::new();
        self.inner
            .guard()
            .upstream_policy()
            .unwrap_or_else(|| STRICT.get_or_init(UpstreamPolicy::strict))
    }

    /// Prüft eine Ziel-URL gegen die SSRF-Regeln (Konfiguration; Fehlertext für den Nutzer).
    pub fn check_url(&self, url: &str) -> Result<Uri, String> {
        self.policy().check_endpoint(url)?;
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
            .body(body.clone())
            .map_err(|e| WebhookError::Request(e.to_string()))?;
        let h = req.headers_mut();
        *h = headers;
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        h.insert(
            USER_AGENT,
            HeaderValue::from_static(aigw_outbound::USER_AGENT),
        );
        if let Some(secret) = secret {
            let ts = time::OffsetDateTime::now_utc().unix_timestamp();
            let v = sign::sign(secret.expose().as_bytes(), ts, &body);
            h.insert(
                sign::SIGNATURE_HEADER,
                HeaderValue::from_str(&v).map_err(|e| WebhookError::Request(e.to_string()))?,
            );
        }
        let limits = FetchLimits {
            timeout,
            max_body: max_response_bytes,
            // 3xx wird nicht verfolgt; der Aufrufer wertet es (wie jeden Nicht-2xx-Status) als
            // Fehlschlag.
            redirects: Redirects::Return,
        };
        match self.inner.fetch(req, &limits).await {
            Ok(r) => Ok(WebhookResponse {
                status: r.status,
                headers: r.headers,
                body: r.body,
            }),
            Err(OutboundError::Timeout(t)) => Err(WebhookError::Timeout(t.as_millis())),
            Err(OutboundError::TooLarge(n)) => Err(WebhookError::ResponseTooLarge(n)),
            Err(OutboundError::Blocked(m)) => Err(WebhookError::Blocked(m)),
            Err(OutboundError::Request(m)) => Err(WebhookError::Request(m)),
            Err(other) => Err(WebhookError::Transport(other.to_string())),
        }
    }
}
