// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! HTTP-Client für OTLP/HTTP (Protobuf): `POST {endpoint}/v1/traces` bzw. `/v1/logs`.
//!
//! hyper-util-Client mit rustls (aws-lc-rs, Mozilla-Wurzelzertifikate) wie der Upstream-Client;
//! Klartext-HTTP ist für einen Collector im selben Netz (Sidecar, `localhost:4318`) erlaubt.

use std::fmt;
use std::time::Duration;

use aigw_core::config::OtlpConfig;
use bytes::Bytes;
use http::header::{CONTENT_TYPE, HeaderName, HeaderValue};
use http::{Method, Request, Uri};
use http_body_util::{BodyExt as _, Full, Limited};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

use crate::export::ExportError;

/// Fehler in der OTLP-Konfiguration.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OtlpConfigError {
    /// `observe.otlp.endpoint` ist keine gültige `http(s)://`-URL.
    #[error("observe.otlp.endpoint ist keine gültige http(s)-URL: {0:?}")]
    Endpoint(String),
    /// Header-Name oder -Wert ungültig.
    #[error("observe.otlp.headers: ungültiger Header {0:?}")]
    Header(String),
    /// TLS-Initialisierung.
    #[error("TLS: {0}")]
    Tls(#[from] rustls::Error),
}

/// Signal-Pfad.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Signal {
    Traces,
    Logs,
}

impl Signal {
    pub(crate) fn path(self) -> &'static str {
        match self {
            Self::Traces => "/v1/traces",
            Self::Logs => "/v1/logs",
        }
    }
}

/// Größte gelesene Antwort des Collectors (Rest wird verworfen).
const MAX_RESPONSE: usize = 64 * 1024;

/// OTLP-HTTP-Client (günstig klonbar).
#[derive(Clone)]
pub(crate) struct OtlpClient {
    client: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
    traces_uri: Uri,
    logs_uri: Uri,
    headers: Vec<(HeaderName, HeaderValue)>,
    timeout: Duration,
}

impl fmt::Debug for OtlpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OtlpClient")
            .field("traces_uri", &self.traces_uri)
            .field("logs_uri", &self.logs_uri)
            .finish_non_exhaustive()
    }
}

impl OtlpClient {
    pub(crate) fn new(cfg: &OtlpConfig) -> Result<Self, OtlpConfigError> {
        let base = cfg.endpoint.trim().trim_end_matches('/');
        let uri = |path: &str| -> Result<Uri, OtlpConfigError> {
            let u: Uri = format!("{base}{path}")
                .parse()
                .map_err(|_| OtlpConfigError::Endpoint(cfg.endpoint.clone()))?;
            match u.scheme_str() {
                Some("http" | "https") if u.host().is_some() => Ok(u),
                _ => Err(OtlpConfigError::Endpoint(cfg.endpoint.clone())),
            }
        };
        let traces_uri = uri(Signal::Traces.path())?;
        let logs_uri = uri(Signal::Logs.path())?;
        let headers = cfg
            .headers
            .iter()
            .map(|(k, v)| {
                Ok((
                    HeaderName::from_bytes(k.as_bytes())
                        .map_err(|_| OtlpConfigError::Header(k.clone()))?,
                    HeaderValue::from_str(v).map_err(|_| OtlpConfigError::Header(k.clone()))?,
                ))
            })
            .collect::<Result<_, OtlpConfigError>>()?;

        let mut http = HttpConnector::new();
        http.enforce_http(false);
        http.set_nodelay(true);
        http.set_connect_timeout(Some(Duration::from_secs(5)));
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_provider_and_webpki_roots(rustls::crypto::aws_lc_rs::default_provider())?
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .wrap_connector(http);
        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(60))
            .pool_max_idle_per_host(4)
            .build(https);
        Ok(Self {
            client,
            traces_uri,
            logs_uri,
            headers,
            timeout: Duration::from_millis(cfg.timeout_ms.max(1)),
        })
    }

    /// Sendet einen Protobuf-Body; Erfolg = Status 2xx.
    pub(crate) async fn post(&self, signal: Signal, body: Vec<u8>) -> Result<(), ExportError> {
        let uri = match signal {
            Signal::Traces => self.traces_uri.clone(),
            Signal::Logs => self.logs_uri.clone(),
        };
        let mut req = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(CONTENT_TYPE, "application/x-protobuf")
            .body(Full::new(Bytes::from(body)))
            .map_err(|e| ExportError::Other(e.to_string()))?;
        for (k, v) in &self.headers {
            req.headers_mut().insert(k.clone(), v.clone());
        }
        let send = async {
            let resp = self
                .client
                .request(req)
                .await
                .map_err(|e| ExportError::Other(format!("Collector nicht erreichbar: {e}")))?;
            let status = resp.status();
            // Antwort lesen (Verbindung bleibt wiederverwendbar), Inhalt ist uninteressant.
            let _ = Limited::new(resp.into_body(), MAX_RESPONSE).collect().await;
            if status.is_success() {
                Ok(())
            } else {
                Err(ExportError::Other(format!(
                    "Collector antwortet mit HTTP {status}"
                )))
            }
        };
        tokio::time::timeout(self.timeout, send)
            .await
            .map_err(|_| ExportError::Other("Zeitlimit beim Collector".into()))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(endpoint: &str) -> OtlpConfig {
        OtlpConfig {
            endpoint: endpoint.into(),
            ..OtlpConfig::default()
        }
    }

    #[test]
    fn endpoint_validation() {
        let c = OtlpClient::new(&cfg("http://collector:4318/")).unwrap();
        assert_eq!(c.traces_uri.to_string(), "http://collector:4318/v1/traces");
        assert_eq!(c.logs_uri.to_string(), "http://collector:4318/v1/logs");
        assert!(OtlpClient::new(&cfg("collector:4318")).is_err());
        assert!(OtlpClient::new(&cfg("ftp://x")).is_err());
        let mut bad = cfg("https://x");
        bad.headers.insert("bad header".into(), "v".into());
        assert!(OtlpClient::new(&bad).is_err());
    }
}
