// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Ereignis-Webhooks: asynchron, gepuffert, Wiederholung mit Backoff, Dead-Letter-Zähler,
//! Deduplizierung im Cluster.
//!
//! Ablauf je Ereignis ([`EventDispatcher::emit`] blockiert nie):
//!
//! 1. Puffer (`mpsc`, `queue_capacity`); voll → verworfen (`dropped`, Warnung).
//! 2. Gibt es Empfänger für den Typ und hat das Ereignis einen Dedup-Schlüssel, beansprucht
//!    die Instanz ihn über [`EventDedup`] (Tabelle `event_dedup`). Hat eine andere Instanz ihn
//!    schon, wird das Ereignis verworfen (`deduplicated`). Ist die Datenbank nicht
//!    erreichbar, wird trotzdem gesendet (lieber doppelt als gar nicht, `dedup_errors`).
//! 3. Zustellung an jeden passenden Empfänger (höchstens `concurrency` gleichzeitig):
//!    `POST` mit JSON `{id, type, occurredAt, data}`, signiert (`x-aigw-signature`),
//!    `x-aigw-delivery` = Ereignis-ID (gleich über Wiederholungen → Idempotenz beim
//!    Empfänger). 2xx = zugestellt. Zeitüberschreitung, Verbindungsfehler, 408/425/429/5xx
//!    werden mit exponentiellem Backoff (mit Zufallsanteil, `Retry-After` wird beachtet)
//!    wiederholt; nach `max_attempts` bzw. bei anderen 4xx zählt `dead_letter` (Log mit
//!    Ereignis-ID; das Ereignis selbst wird nicht persistiert).

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use aigw_core::config::{EventWebhookConfig, EventsConfig};
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, StatusCode, Uri};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};

use crate::client::{DELIVERY_HEADER, WebhookClient};
use crate::secret::{CredentialSource, SecretResolver};

/// Header mit dem Ereignistyp.
pub const EVENT_TYPE_HEADER: &str = "x-aigw-event-type";
/// Header mit der Nummer des Zustellversuchs (ab 1).
pub const ATTEMPT_HEADER: &str = "x-aigw-attempt";

/// Dedup-Schlüssel eines Ereignisses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dedup {
    /// Schlüssel (eindeutig je Ereignis-Anlass, z. B. Budget + Zeitraum + Schwelle).
    pub key: String,
    /// Bis wann (ms seit Epoche) gleiche Ereignisse unterdrückt werden.
    pub expires_at_ms: i64,
}

/// Ein Ereignis.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// Eindeutige ID (UUIDv7).
    pub id: uuid::Uuid,
    /// Typ, z. B. `budget.warning`.
    pub kind: String,
    /// Zeitpunkt (ms seit Epoche).
    pub occurred_at_ms: i64,
    /// Deduplizierung im Cluster.
    pub dedup: Option<Dedup>,
    /// Nutzdaten (`data` im JSON).
    pub data: Value,
}

fn now_ms() -> i64 {
    i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
}

impl Event {
    /// Neues Ereignis „jetzt“.
    pub fn new(kind: impl Into<String>, data: Value) -> Self {
        Self {
            id: uuid::Uuid::now_v7(),
            kind: kind.into(),
            occurred_at_ms: now_ms(),
            dedup: None,
            data,
        }
    }

    /// Mit Dedup-Schlüssel.
    #[must_use]
    pub fn with_dedup(mut self, key: impl Into<String>, expires_at_ms: i64) -> Self {
        self.dedup = Some(Dedup {
            key: key.into(),
            expires_at_ms,
        });
        self
    }

    /// JSON-Body, wie er an Empfänger geht.
    pub fn body(&self) -> Bytes {
        let at =
            OffsetDateTime::from_unix_timestamp_nanos(i128::from(self.occurred_at_ms) * 1_000_000)
                .ok()
                .and_then(|t| t.format(&Rfc3339).ok());
        Bytes::from(
            json!({
                "id": self.id,
                "type": self.kind,
                "occurredAt": at,
                "data": self.data,
            })
            .to_string(),
        )
    }
}

/// Deduplizierung im Cluster (Betrieb: `aigw_store::Store`, Tabelle `event_dedup`).
#[async_trait::async_trait]
pub trait EventDedup: Send + Sync + fmt::Debug {
    /// Beansprucht `key` bis `expires_at_ms`; `Ok(true)` = diese Instanz sendet.
    async fn claim(
        &self,
        key: &str,
        event_type: &str,
        now_ms: i64,
        expires_at_ms: i64,
    ) -> Result<bool, String>;
}

/// Ein Empfänger.
#[derive(Debug, Clone)]
pub struct EventTarget {
    /// Name (Log).
    pub name: String,
    /// Ziel.
    pub uri: Uri,
    /// Ereignistypen (`budget.warning`, `budget.*`, `*`); leer = alle.
    pub events: Vec<String>,
    /// HMAC-Schlüssel.
    pub secret: SecretResolver,
    /// Zeitlimit je Versuch.
    pub timeout: Duration,
    /// Versuche insgesamt.
    pub max_attempts: u32,
}

impl EventTarget {
    /// Aus der Konfiguration (SSRF-Prüfung der URL, Geheimnis auflösen).
    pub fn from_config(
        client: &WebhookClient,
        cfg: &EventWebhookConfig,
        source: Option<Arc<dyn CredentialSource>>,
    ) -> Result<Self, String> {
        let ctx = |e: String| format!("events.webhooks[{}]: {e}", cfg.name);
        if cfg.name.trim().is_empty() {
            return Err("events.webhooks: name darf nicht leer sein".into());
        }
        let uri = client.check_url(&cfg.url).map_err(ctx)?;
        let secret = SecretResolver::from_config(cfg.secret.as_ref(), source).map_err(ctx)?;
        if !secret.is_some() {
            tracing::warn!(
                webhook = %cfg.name,
                "Ereignis-Webhook ohne Geheimnis – Aufrufe sind nicht signiert"
            );
        }
        if cfg.timeout_ms == 0 || cfg.max_attempts == 0 {
            return Err(ctx("timeout_ms und max_attempts müssen > 0 sein".into()));
        }
        Ok(Self {
            name: cfg.name.clone(),
            uri,
            events: cfg.events.clone(),
            secret,
            timeout: Duration::from_millis(cfg.timeout_ms),
            max_attempts: cfg.max_attempts,
        })
    }

    /// Will der Empfänger Ereignisse vom Typ `kind`?
    pub fn wants(&self, kind: &str) -> bool {
        self.events.is_empty()
            || self.events.iter().any(|p| {
                p == "*"
                    || p == kind
                    || p.strip_suffix(".*").is_some_and(|pre| {
                        kind.strip_prefix(pre).is_some_and(|r| r.starts_with('.'))
                    })
            })
    }
}

/// Einstellungen der Zustellung.
#[derive(Debug, Clone)]
pub struct DispatcherOptions {
    /// Plätze im Puffer.
    pub queue_capacity: usize,
    /// Gleichzeitige Zustellungen.
    pub concurrency: usize,
    /// Erste Wartezeit vor einer Wiederholung.
    pub retry_initial: Duration,
    /// Längste Wartezeit.
    pub retry_max: Duration,
    /// Größte gelesene Antwort.
    pub max_response_bytes: usize,
}

impl Default for DispatcherOptions {
    fn default() -> Self {
        Self::from_config(&EventsConfig::default())
    }
}

impl DispatcherOptions {
    /// Aus `events.*`.
    pub fn from_config(cfg: &EventsConfig) -> Self {
        Self {
            queue_capacity: cfg.queue_capacity.max(1),
            concurrency: cfg.concurrency.max(1),
            retry_initial: Duration::from_millis(cfg.retry_initial_ms.max(1)),
            retry_max: Duration::from_millis(cfg.retry_max_ms.max(cfg.retry_initial_ms).max(1)),
            max_response_bytes: 64 * 1024,
        }
    }
}

/// Zähler der Ereignis-Zustellung.
#[derive(Debug, Default)]
pub struct EventMetrics {
    /// Angenommene Ereignisse.
    pub enqueued: AtomicU64,
    /// Erfolgreiche Zustellungen (je Empfänger).
    pub delivered: AtomicU64,
    /// Wiederholungen.
    pub retries: AtomicU64,
    /// Endgültig gescheiterte Zustellungen (je Empfänger).
    pub dead_letter: AtomicU64,
    /// Wegen vollem Puffer verworfene Ereignisse.
    pub dropped: AtomicU64,
    /// Im Cluster schon von einer anderen Instanz gesendete Ereignisse.
    pub deduplicated: AtomicU64,
    /// Fehler bei der Deduplizierung (Ereignis trotzdem gesendet).
    pub dedup_errors: AtomicU64,
}

impl EventMetrics {
    fn inc(c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }

    /// Wert eines Zählers.
    pub fn get(c: &AtomicU64) -> u64 {
        c.load(Ordering::Relaxed)
    }

    /// Prometheus-Textformat (`aigw_event_webhook_*_total`).
    pub fn render_prometheus(&self, out: &mut String) {
        use std::fmt::Write as _;
        for (name, help, c) in [
            ("enqueued", "Angenommene Ereignisse", &self.enqueued),
            (
                "delivered",
                "Zugestellte Ereignisse (je Empfänger)",
                &self.delivered,
            ),
            ("retries", "Wiederholte Zustellversuche", &self.retries),
            (
                "dead_letter",
                "Endgültig gescheiterte Zustellungen (je Empfänger)",
                &self.dead_letter,
            ),
            (
                "dropped",
                "Wegen vollem Puffer verworfene Ereignisse",
                &self.dropped,
            ),
            (
                "deduplicated",
                "Im Cluster schon gesendete Ereignisse",
                &self.deduplicated,
            ),
            (
                "dedup_errors",
                "Fehler bei der Deduplizierung (trotzdem gesendet)",
                &self.dedup_errors,
            ),
        ] {
            let _ = writeln!(
                out,
                "# HELP aigw_event_webhook_{name}_total {help}\n\
                 # TYPE aigw_event_webhook_{name}_total counter\n\
                 aigw_event_webhook_{name}_total {}",
                Self::get(c)
            );
        }
    }
}

struct Ctx {
    client: WebhookClient,
    targets: Vec<Arc<EventTarget>>,
    dedup: Option<Arc<dyn EventDedup>>,
    opts: DispatcherOptions,
    metrics: Arc<EventMetrics>,
    sem: Arc<Semaphore>,
}

/// Nimmt Ereignisse entgegen (günstig klonbar).
#[derive(Clone)]
pub struct EventDispatcher {
    tx: mpsc::Sender<Event>,
    metrics: Arc<EventMetrics>,
}

impl fmt::Debug for EventDispatcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventDispatcher").finish_non_exhaustive()
    }
}

/// Hintergrund-Task der Zustellung.
#[derive(Debug)]
pub struct EventTask {
    handle: Option<JoinHandle<()>>,
    stop: watch::Sender<bool>,
}

impl EventTask {
    /// Stellt gepufferte Ereignisse noch zu (höchstens `timeout`) und beendet die Task.
    pub async fn shutdown(mut self, timeout: Duration) {
        let _ = self.stop.send(true);
        if let Some(mut h) = self.handle.take()
            && tokio::time::timeout(timeout, &mut h).await.is_err()
        {
            tracing::warn!("Ereignis-Webhooks: Zustellung beim Herunterfahren abgebrochen");
            h.abort();
        }
    }
}

impl Drop for EventTask {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }
}

impl EventDispatcher {
    /// Startet die Zustellung. Muss innerhalb einer Tokio-Laufzeit aufgerufen werden.
    pub fn start(
        client: WebhookClient,
        targets: Vec<EventTarget>,
        dedup: Option<Arc<dyn EventDedup>>,
        opts: DispatcherOptions,
    ) -> (Self, EventTask) {
        let (tx, rx) = mpsc::channel(opts.queue_capacity);
        let metrics = Arc::new(EventMetrics::default());
        let (stop, stop_rx) = watch::channel(false);
        let ctx = Arc::new(Ctx {
            client,
            targets: targets.into_iter().map(Arc::new).collect(),
            dedup,
            sem: Arc::new(Semaphore::new(opts.concurrency)),
            opts,
            metrics: Arc::clone(&metrics),
        });
        let handle = tokio::spawn(run(rx, ctx, stop_rx));
        (
            Self { tx, metrics },
            EventTask {
                handle: Some(handle),
                stop,
            },
        )
    }

    /// Nimmt ein Ereignis an (blockiert nie). `false` = verworfen (Puffer voll/geschlossen).
    pub fn emit(&self, ev: Event) -> bool {
        match self.tx.try_send(ev) {
            Ok(()) => {
                EventMetrics::inc(&self.metrics.enqueued);
                true
            }
            Err(e) => {
                EventMetrics::inc(&self.metrics.dropped);
                let ev = match e {
                    mpsc::error::TrySendError::Full(ev) | mpsc::error::TrySendError::Closed(ev) => {
                        ev
                    }
                };
                tracing::warn!(
                    event_id = %ev.id,
                    event_type = %ev.kind,
                    "Ereignis-Webhooks: Puffer voll – Ereignis verworfen"
                );
                false
            }
        }
    }

    /// Zähler.
    pub fn metrics(&self) -> &Arc<EventMetrics> {
        &self.metrics
    }
}

async fn run(mut rx: mpsc::Receiver<Event>, ctx: Arc<Ctx>, mut stop: watch::Receiver<bool>) {
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Some(ev) => handle(ev, &ctx, &mut tasks).await,
                None => break,
            },
            _ = stop.changed() => break,
        }
        while tasks.try_join_next().is_some() {}
    }
    rx.close();
    while let Ok(ev) = rx.try_recv() {
        handle(ev, &ctx, &mut tasks).await;
    }
    while tasks.join_next().await.is_some() {}
}

async fn handle(ev: Event, ctx: &Arc<Ctx>, tasks: &mut JoinSet<()>) {
    let targets: Vec<Arc<EventTarget>> = ctx
        .targets
        .iter()
        .filter(|t| t.wants(&ev.kind))
        .cloned()
        .collect();
    if targets.is_empty() {
        return;
    }
    if let (Some(d), Some(dedup)) = (&ev.dedup, &ctx.dedup) {
        match dedup
            .claim(&d.key, &ev.kind, now_ms(), d.expires_at_ms)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                EventMetrics::inc(&ctx.metrics.deduplicated);
                tracing::debug!(
                    event_type = %ev.kind,
                    dedup_key = %d.key,
                    "Ereignis schon von einer anderen Instanz gesendet"
                );
                return;
            }
            Err(e) => {
                EventMetrics::inc(&ctx.metrics.dedup_errors);
                tracing::warn!(
                    event_type = %ev.kind,
                    error = %e,
                    "Deduplizierung nicht möglich – Ereignis wird trotzdem gesendet"
                );
            }
        }
    }
    let ev = Arc::new(ev);
    let body = ev.body();
    for t in targets {
        let Ok(permit) = Arc::clone(&ctx.sem).acquire_owned().await else {
            return;
        };
        let (ctx, ev, body) = (Arc::clone(ctx), Arc::clone(&ev), body.clone());
        tasks.spawn(async move {
            deliver(&ctx, &t, &ev, body).await;
            drop(permit);
        });
    }
}

fn jitter(d: Duration) -> Duration {
    // Faktor 0,5–1,0 gegen gleichzeitige Wiederholungen vieler Instanzen.
    let mut b = [0u8; 1];
    let r = aws_lc_rs::rand::fill(&mut b).map_or(255, |()| b[0]);
    d.mul_f64(0.5 + f64::from(r) / 510.0)
}

fn retry_after(h: &HeaderMap) -> Option<Duration> {
    h.get(http::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

fn retryable_status(s: StatusCode) -> bool {
    s.is_server_error() || matches!(s.as_u16(), 408 | 425 | 429)
}

async fn deliver(ctx: &Ctx, t: &EventTarget, ev: &Event, body: Bytes) {
    let mut delay = ctx.opts.retry_initial;
    let max = t.max_attempts.max(1);
    for attempt in 1..=max {
        let mut headers = HeaderMap::new();
        if let Ok(v) = HeaderValue::from_str(&ev.id.to_string()) {
            headers.insert(DELIVERY_HEADER, v);
        }
        if let Ok(v) = HeaderValue::from_str(&ev.kind) {
            headers.insert(EVENT_TYPE_HEADER, v);
        }
        headers.insert(ATTEMPT_HEADER, HeaderValue::from(attempt));
        let (retry, wait, reason) = match t.secret.get() {
            Err(e) => (true, None, e),
            Ok(secret) => match ctx
                .client
                .post_json(
                    &t.uri,
                    headers,
                    body.clone(),
                    secret.as_ref(),
                    t.timeout,
                    ctx.opts.max_response_bytes,
                )
                .await
            {
                Ok(r) if r.status.is_success() => {
                    EventMetrics::inc(&ctx.metrics.delivered);
                    tracing::debug!(
                        webhook = %t.name,
                        event_id = %ev.id,
                        event_type = %ev.kind,
                        attempt,
                        "Ereignis zugestellt"
                    );
                    return;
                }
                Ok(r) => (
                    retryable_status(r.status),
                    retry_after(&r.headers),
                    format!("Status {}", r.status.as_u16()),
                ),
                Err(e) => (e.is_retryable(), None, e.to_string()),
            },
        };
        if !retry || attempt == max {
            EventMetrics::inc(&ctx.metrics.dead_letter);
            tracing::error!(
                webhook = %t.name,
                event_id = %ev.id,
                event_type = %ev.kind,
                attempts = attempt,
                error = %reason,
                "Ereignis-Webhook endgültig gescheitert (Dead Letter)"
            );
            return;
        }
        EventMetrics::inc(&ctx.metrics.retries);
        tracing::warn!(
            webhook = %t.name,
            event_id = %ev.id,
            attempt,
            error = %reason,
            "Ereignis-Webhook fehlgeschlagen – neuer Versuch folgt"
        );
        let sleep = jitter(delay)
            .max(wait.unwrap_or_default())
            .min(ctx.opts.retry_max);
        tokio::time::sleep(sleep).await;
        delay = (delay * 2).min(ctx.opts.retry_max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(events: &[&str]) -> EventTarget {
        EventTarget {
            name: "t".into(),
            uri: Uri::from_static("https://example.org/"),
            events: events.iter().map(|s| (*s).to_owned()).collect(),
            secret: SecretResolver::None,
            timeout: Duration::from_secs(1),
            max_attempts: 1,
        }
    }

    #[test]
    fn event_filter() {
        assert!(target(&[]).wants("budget.warning"));
        assert!(target(&["*"]).wants("budget.warning"));
        assert!(target(&["budget.*"]).wants("budget.exhausted"));
        assert!(!target(&["budget.*"]).wants("budgetx.y"));
        assert!(target(&["budget.warning"]).wants("budget.warning"));
        assert!(!target(&["budget.warning"]).wants("budget.exhausted"));
    }

    #[test]
    fn body_shape() {
        let ev = Event::new("budget.warning", json!({"percent": 80}));
        let v: Value = serde_json::from_slice(&ev.body()).unwrap_or_default();
        assert_eq!(v["type"], "budget.warning");
        assert_eq!(v["data"]["percent"], 80);
        assert!(v["occurredAt"].as_str().is_some_and(|s| s.ends_with('Z')));
        assert_eq!(v["id"], ev.id.to_string());
    }
}
