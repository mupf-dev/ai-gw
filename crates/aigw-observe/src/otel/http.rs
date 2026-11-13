// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! HTTP-Client für OTLP/HTTP (Protobuf): `POST {endpoint}/v1/traces` bzw. `/v1/logs`.
//!
//! Über den gemeinsamen Outbound-Baustein (`aigw-outbound`, AIGW-72): rustls/aws-lc-rs mit den
//! Vertrauensankern aus `upstream.roots`/`upstream.ca_file`, keine Weiterleitungen, Zeit- und
//! Größenlimit. Der Collector stammt aus der Startkonfiguration (Betreiber) – daher ohne
//! SSRF-Adressfilter; Klartext-HTTP ist für einen Collector im selben Netz (Sidecar,
//! `localhost:4318`) erlaubt.
//!
//! Wiederholung (F102): `429`, `502`, `503`, `504`, Verbindungsfehler und Zeitlimit werden mit
//! exponentiellem Backoff (mit Jitter) wiederholt, höchstens `observe.otlp.retry.max_attempts`
//! Versuche. Ein `Retry-After` (Sekunden) des Collectors wird beachtet; verlangt er mehr als
//! `max_backoff_ms`, wird der Batch verworfen statt den Export zu blockieren.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use aigw_core::config::{OtlpConfig, OtlpRetryConfig};
use aigw_outbound::{AddressGuard, FetchLimits, OutboundClient, OutboundError, OutboundOptions};
use bytes::Bytes;
use http::header::{CONTENT_TYPE, HeaderName, HeaderValue, RETRY_AFTER};
use http::{HeaderMap, Method, Request, StatusCode, Uri};

use super::OtlpMetrics;
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
    /// HTTP-Client/TLS-Initialisierung.
    #[error("OTLP-Client: {0}")]
    Client(String),
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

/// Wiederholungsregeln.
#[derive(Debug, Clone, Copy)]
struct Retry {
    max_attempts: u32,
    initial: Duration,
    max: Duration,
}

impl From<&OtlpRetryConfig> for Retry {
    fn from(c: &OtlpRetryConfig) -> Self {
        Self {
            max_attempts: c.max_attempts.max(1),
            initial: Duration::from_millis(c.initial_backoff_ms.max(1)),
            max: Duration::from_millis(c.max_backoff_ms.max(c.initial_backoff_ms).max(1)),
        }
    }
}

/// Ergebnis eines einzelnen Versuchs.
enum Attempt {
    Done,
    /// Vorübergehender Fehler, ggf. mit `Retry-After`.
    Retry(ExportError, Option<Duration>),
    Fatal(ExportError),
}

/// OTLP-HTTP-Client (günstig klonbar).
#[derive(Clone)]
pub(crate) struct OtlpClient {
    client: OutboundClient,
    traces_uri: Uri,
    logs_uri: Uri,
    headers: Vec<(HeaderName, HeaderValue)>,
    timeout: Duration,
    retry: Retry,
    metrics: Arc<OtlpMetrics>,
}

impl fmt::Debug for OtlpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OtlpClient")
            .field("traces_uri", &self.traces_uri)
            .field("logs_uri", &self.logs_uri)
            .field("retry", &self.retry)
            .finish_non_exhaustive()
    }
}

impl OtlpClient {
    pub(crate) fn new(
        cfg: &OtlpConfig,
        metrics: Arc<OtlpMetrics>,
    ) -> Result<Self, OtlpConfigError> {
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
        let client = OutboundClient::builder(AddressGuard::Trusted)
            .options(OutboundOptions {
                connect_timeout: Duration::from_secs(5),
                pool_idle_timeout: Duration::from_secs(60),
                pool_max_idle_per_host: 4,
                ..OutboundOptions::default()
            })
            .build()
            .map_err(|e| OtlpConfigError::Client(e.to_string()))?;
        Ok(Self {
            client,
            traces_uri,
            logs_uri,
            headers,
            timeout: Duration::from_millis(cfg.timeout_ms.max(1)),
            retry: Retry::from(&cfg.retry),
            metrics,
        })
    }

    /// Sendet einen Protobuf-Body; Erfolg = Status 2xx. Vorübergehende Fehler werden
    /// wiederholt (siehe Moduldoku).
    pub(crate) async fn post(&self, signal: Signal, body: Vec<u8>) -> Result<(), ExportError> {
        let body = Bytes::from(body);
        let mut backoff = self.retry.initial;
        let mut attempt = 1u32;
        loop {
            let (err, retry_after) = match self.post_once(signal, body.clone()).await {
                Attempt::Done => return Ok(()),
                Attempt::Fatal(e) => return Err(e),
                Attempt::Retry(e, ra) => (e, ra),
            };
            if attempt >= self.retry.max_attempts {
                return Err(err);
            }
            let wait = match retry_after {
                Some(ra) if ra > self.retry.max => {
                    return Err(ExportError::Other(format!(
                        "{err}; Retry-After {} s > max_backoff – Batch verworfen",
                        ra.as_secs()
                    )));
                }
                Some(ra) => ra,
                None => jitter(backoff),
            };
            let counter = match signal {
                Signal::Traces => &self.metrics.trace_export_retries,
                Signal::Logs => &self.metrics.log_export_retries,
            };
            counter.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(error = %err, attempt, wait_ms = wait.as_millis() as u64, signal = signal.path(), "OTLP-Export wird wiederholt");
            tokio::time::sleep(wait).await;
            backoff = (backoff * 2).min(self.retry.max);
            attempt += 1;
        }
    }

    async fn post_once(&self, signal: Signal, body: Bytes) -> Attempt {
        let uri = match signal {
            Signal::Traces => self.traces_uri.clone(),
            Signal::Logs => self.logs_uri.clone(),
        };
        let mut req = match Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(CONTENT_TYPE, "application/x-protobuf")
            .body(body)
        {
            Ok(r) => r,
            Err(e) => return Attempt::Fatal(ExportError::Other(e.to_string())),
        };
        for (k, v) in &self.headers {
            req.headers_mut().insert(k.clone(), v.clone());
        }
        // Antwort lesen (Verbindung bleibt wiederverwendbar), Inhalt ist uninteressant.
        match self
            .client
            .fetch(req, &FetchLimits::new(self.timeout, MAX_RESPONSE))
            .await
        {
            Ok(r) if r.status.is_success() => Attempt::Done,
            Ok(r) => {
                let e = ExportError::Other(format!("Collector antwortet mit HTTP {}", r.status));
                if retryable_status(r.status) {
                    Attempt::Retry(e, retry_after(&r.headers, self.retry.max))
                } else {
                    Attempt::Fatal(e)
                }
            }
            Err(OutboundError::Timeout(_)) => {
                Attempt::Retry(ExportError::Other("Zeitlimit beim Collector".into()), None)
            }
            Err(e) if e.is_retryable() => Attempt::Retry(
                ExportError::Other(format!("Collector nicht erreichbar: {e}")),
                None,
            ),
            // Antwort zu groß: der Export selbst war erfolgreich (Status unbekannt) – nicht
            // wiederholen, aber melden.
            Err(e) => Attempt::Fatal(ExportError::Other(format!("Collector: {e}"))),
        }
    }
}

/// Vorübergehende Fehler nach OTLP/HTTP-Spezifikation.
fn retryable_status(s: StatusCode) -> bool {
    matches!(s.as_u16(), 429 | 502 | 503 | 504)
}

/// `Retry-After` in Sekunden; ein HTTP-Datum wird als `cap` gewertet (konservativ).
fn retry_after(h: &HeaderMap, cap: Duration) -> Option<Duration> {
    let v = h.get(RETRY_AFTER)?.to_str().ok()?.trim();
    match v.parse::<u64>() {
        Ok(secs) => Some(Duration::from_secs(secs)),
        Err(_) if !v.is_empty() => Some(cap),
        Err(_) => None,
    }
}

/// Wartezeit zwischen 50 % und 100 % von `d` (verteilt gleichzeitige Wiederholungen).
fn jitter(d: Duration) -> Duration {
    use std::hash::{BuildHasher as _, Hasher as _};
    let r = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    let frac = 0.5 + (r % 1000) as f64 / 2000.0;
    d.mul_f64(frac)
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
        let m = || Arc::new(OtlpMetrics::default());
        let c = OtlpClient::new(&cfg("http://collector:4318/"), m()).unwrap();
        assert_eq!(c.traces_uri.to_string(), "http://collector:4318/v1/traces");
        assert_eq!(c.logs_uri.to_string(), "http://collector:4318/v1/logs");
        assert!(OtlpClient::new(&cfg("collector:4318"), m()).is_err());
        assert!(OtlpClient::new(&cfg("ftp://x"), m()).is_err());
        let mut bad = cfg("https://x");
        bad.headers.insert("bad header".into(), "v".into());
        assert!(OtlpClient::new(&bad, m()).is_err());
    }

    #[test]
    fn retry_after_and_jitter() {
        let cap = Duration::from_secs(5);
        let mut h = HeaderMap::new();
        assert_eq!(retry_after(&h, cap), None);
        h.insert(RETRY_AFTER, HeaderValue::from_static("2"));
        assert_eq!(retry_after(&h, cap), Some(Duration::from_secs(2)));
        h.insert(
            RETRY_AFTER,
            HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
        );
        assert_eq!(retry_after(&h, cap), Some(cap));
        for _ in 0..50 {
            let j = jitter(Duration::from_millis(1000));
            assert!(j >= Duration::from_millis(500) && j <= Duration::from_millis(1000));
        }
        assert!(retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(retryable_status(StatusCode::SERVICE_UNAVAILABLE));
        assert!(!retryable_status(StatusCode::BAD_REQUEST));
        assert!(!retryable_status(StatusCode::INTERNAL_SERVER_ERROR));
    }
}
