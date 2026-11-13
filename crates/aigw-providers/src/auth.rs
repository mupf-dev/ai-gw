// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Tokenbasierte Anbieter-Authentifizierung (Vertex-OAuth, Azure Entra ID).
//!
//! - [`TokenSource`] – der Hot Path ruft [`TokenSource::bearer_token`] synchron auf:
//!   Implementierungen halten ein gültiges Token im RAM (kein Netzwerkzugriff pro Anfrage).
//! - [`RefreshingToken`] – gemeinsamer Token-Cache für abrufbare Tokens (Vertex-Service-Account,
//!   Entra ID): Ein [`TokenFetcher`] holt ein Token samt Laufzeit, der Cache hält es bis
//!   Ablauf − [`SAFETY_MARGIN`] und erneuert ab [`DEFAULT_REFRESH_LEAD`] vor Ablauf im
//!   Hintergrund (die Anfrage nutzt das noch gültige Token). Nur ohne gültiges Token wartet eine
//!   Anfrage; alle dabei wartenden Anfragen teilen sich **einen** Abruf und dessen Ergebnis –
//!   auch einen Fehler (kein Thundering Herd gegen den Token-Endpunkt). Nach einer
//!   gescheiterten Hintergrund-Erneuerung wird frühestens nach [`BACKGROUND_RETRY`] erneut
//!   versucht.
//!
//! Tokens erscheinen nie in Logs oder `Debug`-Ausgaben ([`SecretString`]).

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock};
use std::time::Duration;

use aigw_core::ir::{ApiError, ErrorClass, ErrorKind, UpstreamError};
use aigw_core::resource::SecretString;
use bytes::Bytes;
use http::header::{ACCEPT, CONTENT_TYPE};
use http::{HeaderValue, Method, Uri};
use serde_json::Value;
use tokio::time::Instant;

use crate::client::UpstreamClient;

/// Liefert das aktuelle Bearer-Token eines Anbieters.
pub trait TokenSource: Send + Sync + fmt::Debug {
    /// Aktuelles Token (ohne `Bearer `-Präfix). Fehler, wenn (noch) keines vorliegt.
    fn bearer_token(&self) -> Result<SecretString, ApiError>;
}

/// Festes Token (Tests, kurzlebige Umgebungen).
#[derive(Debug, Clone)]
pub struct StaticToken(pub SecretString);

impl TokenSource for StaticToken {
    fn bearer_token(&self) -> Result<SecretString, ApiError> {
        Ok(self.0.clone())
    }
}

/// Erneuerung im Hintergrund ab dieser Restlaufzeit.
pub const DEFAULT_REFRESH_LEAD: Duration = Duration::from_secs(300);
/// Ein Token mit weniger Restlaufzeit gilt als abgelaufen (Laufzeit der Anfrage, Uhrenversatz);
/// bei sehr kurzen Laufzeiten höchstens die halbe Laufzeit.
pub const SAFETY_MARGIN: Duration = Duration::from_secs(30);
/// Zeitlimit für einen Token-Abruf.
pub(crate) const TOKEN_TIMEOUT: Duration = Duration::from_secs(30);
/// Größte gelesene Token-Antwort.
pub(crate) const MAX_TOKEN_RESPONSE: usize = 64 * 1024;
/// Abstand nach einer gescheiterten Hintergrund-Erneuerung.
pub const BACKGROUND_RETRY: Duration = Duration::from_secs(5);

/// Ergebnis eines Token-Abrufs.
pub struct FetchedToken {
    /// Zugriffstoken.
    pub token: SecretString,
    /// Restlaufzeit ab Beginn des Abrufs.
    pub expires_in: Duration,
}

impl fmt::Debug for FetchedToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FetchedToken")
            .field("expires_in", &self.expires_in)
            .finish_non_exhaustive()
    }
}

/// Zukunft eines Token-Abrufs.
pub type FetchFuture<'a> =
    Pin<Box<dyn Future<Output = Result<FetchedToken, UpstreamError>> + Send + 'a>>;

/// Holt ein Token beim Aussteller (Token-Endpunkt, Metadaten-Dienst …).
pub trait TokenFetcher: Send + Sync + fmt::Debug + 'static {
    /// Ein Abruf über `client` (immer über aigw-outbound).
    fn fetch<'a>(&'a self, client: &'a UpstreamClient) -> FetchFuture<'a>;

    /// Bezeichnung für Logs (z. B. „Vertex“, „Entra ID“).
    fn label(&self) -> &'static str;

    /// HTTP-Client für den Abruf aus dem Client des Deployments (Standard: derselbe).
    /// Managed Identity schaltet hier gezielt den Metadaten-Endpunkt frei.
    fn client_for(&self, deployment: &UpstreamClient) -> Result<UpstreamClient, UpstreamError> {
        Ok(deployment.clone())
    }
}

#[derive(Clone)]
struct Cached {
    token: SecretString,
    /// Ab hier ungültig (Ablauf − Sicherheitsabstand).
    valid_until: Instant,
}

/// Zustand des Vordergrund-Abrufs (unter dem Abruf-Mutex).
#[derive(Default)]
struct Flight {
    /// Fehler des letzten Abrufs (für Anfragen, die auf ihn gewartet haben).
    last_error: Option<UpstreamError>,
}

/// Token-Cache mit Hintergrund-Erneuerung und gebündeltem Abruf (siehe Modul-Doku).
pub struct RefreshingToken<F> {
    fetcher: F,
    refresh_lead: Duration,
    cached: RwLock<Option<Cached>>,
    flight: tokio::sync::Mutex<Flight>,
    /// Zählt abgeschlossene Vordergrund-Abrufe (erkennt „während des Wartens abgerufen“).
    generation: AtomicU64,
    background: AtomicBool,
    background_not_before: Mutex<Option<Instant>>,
    fetches: AtomicU64,
    client: OnceLock<Result<UpstreamClient, UpstreamError>>,
}

impl<F: TokenFetcher> fmt::Debug for RefreshingToken<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RefreshingToken")
            .field("fetcher", &self.fetcher)
            .field("fetches", &self.fetches.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// Fehler bei der Anmeldung beim Anbieter (Token-Abruf): immer 502 mit Code
/// `upstream_authentication_error`; `class` steuert Retries/Fallbacks.
pub(crate) fn token_error(
    class: ErrorClass,
    message: impl Into<String>,
    status: Option<u16>,
) -> UpstreamError {
    let err = UpstreamError::new(
        class,
        ApiError::new(ErrorKind::BadGateway, message).with_code("upstream_authentication_error"),
    );
    match status {
        Some(s) => err.with_status(s),
        None => err,
    }
}

/// Fehlerklasse aus dem HTTP-Status eines Token-Endpunkts: 429 → Rate-Limit, 5xx →
/// Serverfehler (beide retrybar), sonst Auth (Konfiguration/Zugangsdaten, nicht retrybar).
pub(crate) fn token_status_class(status: u16) -> ErrorClass {
    match status {
        429 => ErrorClass::RateLimit,
        s if s >= 500 => ErrorClass::ServerError,
        _ => ErrorClass::Auth,
    }
}

/// Fehler des ausgehenden Clients beim Token-Abruf → [`UpstreamError`].
pub(crate) fn token_outbound_error(e: &aigw_outbound::OutboundError) -> UpstreamError {
    use aigw_outbound::OutboundError as E;
    match e {
        E::Timeout(_) => token_error(ErrorClass::Timeout, "Zeitlimit beim Token-Abruf", None),
        E::TooLarge(_) => token_error(ErrorClass::Protocol, "Token-Antwort zu groß", Some(502)),
        E::Redirect { .. } => token_error(
            ErrorClass::Auth,
            "Token-Endpunkt leitet weiter (nicht erlaubt)",
            Some(502),
        ),
        E::Blocked(_) => token_error(
            ErrorClass::Auth,
            "Token-Endpunkt durch SSRF-Regeln gesperrt",
            None,
        ),
        _ => token_error(ErrorClass::Connect, "Token-Endpunkt nicht erreichbar", None),
    }
}

impl<F: TokenFetcher> RefreshingToken<F> {
    /// Neuer, leerer Cache.
    pub fn with_fetcher(fetcher: F) -> Self {
        Self {
            fetcher,
            refresh_lead: DEFAULT_REFRESH_LEAD,
            cached: RwLock::new(None),
            flight: tokio::sync::Mutex::new(Flight::default()),
            generation: AtomicU64::new(0),
            background: AtomicBool::new(false),
            background_not_before: Mutex::new(None),
            fetches: AtomicU64::new(0),
            client: OnceLock::new(),
        }
    }

    /// Setzt die Vorlaufzeit der Hintergrund-Erneuerung.
    #[must_use]
    pub fn with_refresh_lead(mut self, lead: Duration) -> Self {
        self.refresh_lead = lead;
        self
    }

    /// Der Abrufer.
    pub fn fetcher(&self) -> &F {
        &self.fetcher
    }

    /// Anzahl der bisherigen Token-Abrufe (Diagnose, Tests).
    pub fn fetch_count(&self) -> u64 {
        self.fetches.load(Ordering::Relaxed)
    }

    fn current(&self) -> Option<Cached> {
        self.cached
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// HTTP-Client für den Token-Abruf: aus dem Client des Deployments (gleiche SSRF-Regeln und
    /// Vertrauensanker; gesetzt beim ersten `Provider::prepare`). Teilen sich mehrere
    /// Deployments eine Token-Quelle, gilt der Client des ersten.
    pub fn use_client(&self, client: &UpstreamClient) {
        if self.client.get().is_none() {
            let _ = self.client.set(self.fetcher.client_for(client));
        }
    }

    fn client(&self) -> Result<&UpstreamClient, UpstreamError> {
        self.client
            .get_or_init(|| {
                UpstreamClient::with_defaults()
                    .map_err(|e| {
                        token_error(
                            ErrorClass::Internal,
                            format!("HTTP-Client für Token-Abruf: {e}"),
                            Some(500),
                        )
                    })
                    .and_then(|c| self.fetcher.client_for(&c))
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    /// Sorgt für ein gültiges Token: sofort zurück, wenn eines im Cache liegt (ggf. mit
    /// Erneuerung im Hintergrund), sonst Abruf (einer für alle wartenden Anfragen).
    pub async fn ensure(self: &Arc<Self>) -> Result<(), UpstreamError> {
        let now = Instant::now();
        if let Some(c) = self.current()
            && c.valid_until > now
        {
            if c.valid_until <= now + self.refresh_lead {
                self.spawn_background(now);
            }
            return Ok(());
        }
        let seen = self.generation.load(Ordering::Acquire);
        let mut flight = self.flight.lock().await;
        if self
            .current()
            .is_some_and(|c| c.valid_until > Instant::now())
        {
            return Ok(());
        }
        // Während des Wartens lief ein Abruf: dessen Fehler teilen statt sofort neu abzurufen.
        if self.generation.load(Ordering::Acquire) != seen
            && let Some(e) = &flight.last_error
        {
            return Err(e.clone());
        }
        let result = self.fetch().await;
        flight.last_error = result.as_ref().err().cloned();
        self.generation.fetch_add(1, Ordering::AcqRel);
        result
    }

    fn spawn_background(self: &Arc<Self>, now: Instant) {
        if self
            .background_not_before
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some_and(|t| now < t)
            || self.background.swap(true, Ordering::AcqRel)
        {
            return;
        }
        let me = Arc::clone(self);
        tokio::spawn(async move {
            let flight = me.flight.lock().await;
            let still_due = me
                .current()
                .is_none_or(|c| c.valid_until <= Instant::now() + me.refresh_lead);
            if still_due && let Err(e) = me.fetch().await {
                tracing::warn!(
                    source = me.fetcher.label(),
                    error = %e.error.message,
                    "Token-Erneuerung im Hintergrund fehlgeschlagen"
                );
                *me.background_not_before
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) =
                    Some(Instant::now() + BACKGROUND_RETRY);
            }
            drop(flight);
            me.background.store(false, Ordering::Release);
        });
    }

    async fn fetch(&self) -> Result<(), UpstreamError> {
        self.fetches.fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();
        let client = self.client()?;
        let got = self.fetcher.fetch(client).await.inspect_err(|e| {
            tracing::warn!(
                source = self.fetcher.label(),
                class = e.class.as_str(),
                status = e.upstream_status,
                error = %e.error.message,
                "Token-Abruf fehlgeschlagen"
            );
        })?;
        // Sehr kurze Laufzeiten: höchstens die Hälfte als Sicherheitsabstand.
        let margin = SAFETY_MARGIN.min(got.expires_in / 2);
        let valid_until = started + got.expires_in - margin;
        *self.cached.write().unwrap_or_else(PoisonError::into_inner) = Some(Cached {
            token: got.token,
            valid_until,
        });
        Ok(())
    }
}

impl<F: TokenFetcher> TokenSource for RefreshingToken<F> {
    fn bearer_token(&self) -> Result<SecretString, ApiError> {
        match self.current() {
            Some(c) if c.valid_until > Instant::now() => Ok(c.token),
            _ => Err(ApiError::new(
                ErrorKind::BadGateway,
                "kein gültiges Zugriffstoken für den Anbieter",
            )
            .with_code("upstream_authentication_error")),
        }
    }
}

/// `POST` eines Formulars an einen OAuth-Token-Endpunkt und Auswertung der Antwort
/// (`access_token`, `expires_in`). Fehlerbeschreibungen des Endpunkts (`error`,
/// `error_description`) landen nur im Log, nie beim Client.
pub(crate) async fn post_token_form(
    client: &UpstreamClient,
    token_url: &Uri,
    body: String,
) -> Result<FetchedToken, UpstreamError> {
    let req = http::Request::builder()
        .method(Method::POST)
        .uri(token_url.clone())
        .header(
            CONTENT_TYPE,
            HeaderValue::from_static("application/x-www-form-urlencoded"),
        )
        .header(ACCEPT, HeaderValue::from_static("application/json"))
        .body(Bytes::from(body))
        .map_err(|e| token_error(ErrorClass::Internal, e.to_string(), Some(500)))?;
    let reply = send_token_request(client, req).await?;
    parse_token_reply(reply.status, &reply.body, None)
}

/// Sendet eine Token-Anfrage über aigw-outbound (Zeit- und Größenlimit, keine
/// Weiterleitungen). Der Body der Anfrage (Geheimnisse) wird nie geloggt.
pub(crate) async fn send_token_request(
    client: &UpstreamClient,
    req: http::Request<Bytes>,
) -> Result<aigw_outbound::Reply, UpstreamError> {
    let uri = req.uri().clone();
    let limits = aigw_outbound::FetchLimits::new(TOKEN_TIMEOUT, MAX_TOKEN_RESPONSE);
    client.outbound().fetch(req, &limits).await.map_err(|e| {
        tracing::warn!(
            error = %e,
            host = uri.host().unwrap_or_default(),
            path = uri.path(),
            "Token-Endpunkt: Aufruf fehlgeschlagen"
        );
        token_outbound_error(&e)
    })
}

/// Wertet die Antwort eines Token-Endpunkts aus. `expires_on` (Unix-Sekunden, auch als
/// Zeichenkette) gilt, wenn `expires_in` fehlt (Managed Identity); `now_unix` dafür.
pub(crate) fn parse_token_reply(
    status: http::StatusCode,
    body: &[u8],
    now_unix: Option<u64>,
) -> Result<FetchedToken, UpstreamError> {
    if !status.is_success() {
        // Die Antwort (`error`, `error_description`) nennt nur den Grund, keine Geheimnisse.
        let reason = serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|v| {
                let pick = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
                pick("error_description")
                    .or_else(|| pick("error"))
                    .or_else(|| pick("message"))
            })
            .unwrap_or_default();
        let reason: String = reason.chars().take(500).collect();
        tracing::warn!(status = status.as_u16(), reason = %reason, "Token-Abruf abgelehnt");
        return Err(token_error(
            token_status_class(status.as_u16()),
            format!("Token-Austausch beim Anbieter fehlgeschlagen (HTTP {status})"),
            Some(status.as_u16()),
        ));
    }
    let v: Value = serde_json::from_slice(body).map_err(|_| {
        token_error(
            ErrorClass::Protocol,
            "Token-Antwort nicht lesbar",
            Some(502),
        )
    })?;
    let token = v
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            token_error(
                ErrorClass::Protocol,
                "Token-Antwort ohne access_token",
                Some(502),
            )
        })?;
    let num = |k: &str| {
        v.get(k).and_then(|x| {
            x.as_u64()
                .or_else(|| x.as_str().and_then(|s| s.trim().parse().ok()))
        })
    };
    let secs = num("expires_in")
        .or_else(|| {
            let now = now_unix?;
            num("expires_on").map(|on| on.saturating_sub(now))
        })
        .unwrap_or(3600);
    Ok(FetchedToken {
        token: SecretString::new(token),
        expires_in: Duration::from_secs(secs),
    })
}
