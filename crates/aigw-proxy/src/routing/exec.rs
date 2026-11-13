// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Ausführung einer Anfrage über eine Modellgruppe: Auswahl, Retries, Backoff, `Retry-After`,
//! Gesamtbudget und Fallback-Ketten (AIGW-25).
//!
//! **Retries nur vor dem ersten Byte an den Client:** Bei Streams wird das erste Ereignis des
//! Anbieters abgewartet, bevor der Client Antwort-Header bekommt. Ist es ein Fehler, zählt der
//! Versuch als fehlgeschlagen und darf wiederholt werden. Ab dem ersten weitergereichten
//! Ereignis gibt es keinen Retry mehr; spätere Fehler gehen als Fehlerereignis an den Client
//! und zählen nur noch für den Circuit Breaker.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use aigw_core::ir::{
    ApiError, ChatRequest, ChatResponse, ErrorClass, ErrorKind, StreamEvent, UpstreamError,
};
use aigw_core::observe::Attempt;
use aigw_core::resource::FallbackTrigger;
use aigw_counters::{CounterBackend, LimitCheck, Reservation};
use futures_util::{Stream, StreamExt as _};

use super::breaker::Admit;
use super::select::rand_below;
use super::state::now_ms;
use super::{GroupRoute, Member, RetryPolicy, Scratch};
use crate::provider::EventStream;
use crate::routes::ModelRoutes;

/// Längste beachtete Sperre nach `Retry-After`.
const MAX_COOLDOWN: Duration = Duration::from_secs(300);
/// Höchstens so viele Modellgruppen je Anfrage (Ursprung + Fallbacks).
const MAX_GROUPS: usize = 8;

/// Erfolgreiche Antwort.
pub(crate) enum Outcome {
    /// Nicht gestreamt.
    Json(ChatResponse),
    /// Stream (erstes Ereignis bereits geprüft).
    Stream(EventStream),
}

/// Zähler für die Deployment-Kapazität (`capacity.rpm`/`tpm`).
#[derive(Clone, Copy)]
pub(crate) struct CapacityCtx<'a> {
    /// Dasselbe Backend wie die Limits (lokal, `postgres-sync` oder Redis).
    pub(crate) counters: &'a Arc<dyn CounterBackend>,
    /// Reservierte Tokens je Versuch (geschätzte Eingabe + `max_tokens`).
    pub(crate) tokens: u64,
}

/// Ergebnis der Kapazitätsprüfung eines Versuchs.
enum CapacityCheck {
    /// Frei (ggf. mit Reservierung, die am Ende des Versuchs abgeglichen wird).
    Granted(Option<Reservation>),
    /// Erschöpft; das Deployment ist bis zur Wartezeit für alle Anfragen gesperrt.
    Blocked,
    /// Passt nur für diese Anfrage nicht (zu viele Tokens für den Rest der Minute).
    TooLarge,
}

/// Reserviert die Kapazität eines Deployments für einen Versuch (RPM: 1, TPM: Schätzung;
/// abgeglichen mit der tatsächlichen Usage am Ende des Versuchs).
async fn reserve_capacity(member: &Member, cap: CapacityCtx<'_>) -> CapacityCheck {
    let c = member.capacity;
    if c.rpm.is_none() && c.tpm.is_none() {
        return CapacityCheck::Granted(None);
    }
    let (req_key, tok_key) = (&*member.capacity_keys.0, &*member.capacity_keys.1);
    let mut checks: [Option<LimitCheck<'_>>; 2] = [None, None];
    if let Some(rpm) = c.rpm {
        checks[0] = Some(LimitCheck::fixed(
            req_key,
            aigw_counters::Window::Minute,
            rpm,
            1,
        ));
    }
    if let Some(tpm) = c.tpm {
        checks[1] = Some(LimitCheck::new(
            tok_key,
            aigw_counters::Window::Minute,
            tpm,
            cap.tokens,
        ));
    }
    let checks: Vec<LimitCheck<'_>> = checks.into_iter().flatten().collect();
    match cap.counters.reserve(&checks).await {
        Ok(mut r) => {
            // Nur der Token-Teil wird abgeglichen.
            let tokens = if c.rpm.is_some() { r.split_off(1) } else { r };
            CapacityCheck::Granted((!tokens.is_empty()).then_some(tokens))
        }
        Err(rej) => {
            let requests = c.rpm.is_some() && rej.index == 0;
            let now = now_ms();
            match rej.retry_after {
                // Anfragen erschöpft oder Token-Kontingent aufgebraucht: für alle sperren.
                Some(wait) if requests || rej.used >= rej.limit => {
                    member.state.block_capacity(now, wait);
                    CapacityCheck::Blocked
                }
                _ => CapacityCheck::TooLarge,
            }
        }
    }
}

/// Ergebnis der Ausführung inkl. aller Versuche.
pub(crate) struct Routed {
    pub(crate) result: Result<Outcome, UpstreamError>,
    pub(crate) attempts: Vec<Attempt>,
    /// Zuletzt angefragtes Deployment.
    pub(crate) last: Option<Arc<Member>>,
    /// Modellgruppe des letzten Versuchs, falls sie per Fallback erreicht wurde.
    pub(crate) fallback: Option<String>,
}

/// Fehlschlag einer Modellgruppe; `class = None`: kein Deployment verfügbar.
struct Failure {
    err: UpstreamError,
    class: Option<ErrorClass>,
}

/// Fehler, die auf das Deployment zeigen, aber dort nicht wiederholbar sind: anderes
/// Deployment versuchen, dieses nicht noch einmal.
fn deployment_specific(class: ErrorClass) -> bool {
    matches!(
        class,
        ErrorClass::Auth | ErrorClass::NotFound | ErrorClass::Quota
    )
}

/// Für den Circuit Breaker: `Some(true)` = Fehler des Deployments, `Some(false)` = Deployment
/// hat geantwortet (auch 4xx des Clients), `None` = nicht werten (Rate-Limit).
pub(crate) fn breaker_failure(class: ErrorClass) -> Option<bool> {
    match class {
        ErrorClass::Timeout
        | ErrorClass::Connect
        | ErrorClass::ServerError
        | ErrorClass::Overloaded
        | ErrorClass::Protocol
        | ErrorClass::Auth
        | ErrorClass::NotFound
        | ErrorClass::Quota => Some(true),
        ErrorClass::RateLimit => None,
        _ => Some(false),
    }
}

/// Greift ein Fallback mit diesen Auslösern bei dieser Fehlerklasse?
pub(crate) fn trigger_matches(on: &[FallbackTrigger], class: Option<ErrorClass>) -> bool {
    let any = |c: Option<ErrorClass>| c.is_none_or(|c| c.is_retryable() || deployment_specific(c));
    if on.is_empty() {
        return any(class);
    }
    on.iter().any(|t| match t {
        FallbackTrigger::Any => any(class),
        FallbackTrigger::RateLimit => {
            matches!(class, Some(ErrorClass::RateLimit | ErrorClass::Quota))
        }
        FallbackTrigger::ContextLength => class == Some(ErrorClass::ContextLength),
        FallbackTrigger::ContentFilter => class == Some(ErrorClass::ContentFilter),
        FallbackTrigger::Unavailable => matches!(
            class,
            None | Some(
                ErrorClass::Timeout
                    | ErrorClass::Connect
                    | ErrorClass::ServerError
                    | ErrorClass::Overloaded
                    | ErrorClass::Auth
                    | ErrorClass::NotFound
            )
        ),
    })
}

/// Backoff vor dem `n`-ten erneuten Versuch auf demselben Deployment (`n ≥ 1`):
/// exponentiell, gedeckelt, „equal jitter“ (Hälfte fest, Hälfte zufällig).
pub(crate) fn backoff(policy: &RetryPolicy, n: u32) -> Duration {
    let exp = policy
        .initial_backoff
        .saturating_mul(1u32 << n.saturating_sub(1).min(16));
    let cap = exp.min(policy.max_backoff);
    let half = cap / 2;
    let jitter = u64::try_from(half.as_nanos()).unwrap_or(u64::MAX);
    half + Duration::from_nanos(rand_below(jitter.saturating_add(1)))
}

fn unavailable(group: &GroupRoute, excluded: &[usize]) -> UpstreamError {
    let mut e = UpstreamError::new(
        ErrorClass::Overloaded,
        ApiError::new(
            ErrorKind::Overloaded,
            format!(
                "Modellgruppe {:?}: kein Deployment verfügbar (Circuit Breaker offen, \
                 Kapazität erschöpft oder vom Anbieter gesperrt)",
                group.name
            ),
        )
        .with_code("no_deployment_available"),
    )
    .with_retry_after(group.wait_hint(excluded, now_ms()));
    e.retryable = false;
    e
}

/// Führt `req` über `group` aus – mit Retries und Fallbacks.
pub(crate) async fn execute(
    routes: &dyn ModelRoutes,
    group: Arc<GroupRoute>,
    req: ChatRequest,
    cap: CapacityCtx<'_>,
) -> Routed {
    let stream = req.is_stream();
    let budget_end = Instant::now() + group.policy.retry.budget;
    // Kopie je Versuch nur, wenn überhaupt ein zweiter Versuch möglich ist.
    let single_shot = group.policy.retry.max_attempts <= 1 && group.fallbacks.is_empty();
    let mut req = Some(req);
    let mut routed = Routed {
        result: Err(UpstreamError::from(ApiError::new(
            ErrorKind::Internal,
            "kein Versuch",
        ))),
        attempts: Vec::new(),
        last: None,
        fallback: None,
    };
    let mut scratch = Scratch::default();
    let mut visited: Vec<String> = vec![group.name.clone()];
    let mut next_fallback = 0;
    let origin = Arc::clone(&group);
    let mut current = group;
    loop {
        match run_group(
            &current,
            &mut req,
            single_shot,
            stream,
            budget_end,
            cap,
            &mut routed,
            &mut scratch,
        )
        .await
        {
            Ok(outcome) => {
                routed.result = Ok(outcome);
                return routed;
            }
            Err(failure) => {
                let next = if Instant::now() >= budget_end || visited.len() >= MAX_GROUPS {
                    None
                } else {
                    find_fallback(routes, &origin, &mut next_fallback, &visited, failure.class)
                };
                match next {
                    Some(g) => {
                        tracing::debug!(
                            from = %current.name,
                            to = %g.name,
                            class = ?failure.class,
                            "Fallback auf andere Modellgruppe"
                        );
                        visited.push(g.name.clone());
                        routed.fallback = Some(g.name.clone());
                        current = g;
                    }
                    None => {
                        routed.result = Err(failure.err);
                        return routed;
                    }
                }
            }
        }
    }
}

fn find_fallback(
    routes: &dyn ModelRoutes,
    origin: &GroupRoute,
    next: &mut usize,
    visited: &[String],
    class: Option<ErrorClass>,
) -> Option<Arc<GroupRoute>> {
    while let Some(f) = origin.fallbacks.get(*next) {
        *next += 1;
        if visited.contains(&f.model_group) || !trigger_matches(&f.on, class) {
            continue;
        }
        match routes.route(&f.model_group) {
            Some(g) if !visited.iter().any(|v| *v == g.name) => return Some(g),
            Some(_) => {}
            None => tracing::warn!(
                model_group = %origin.name,
                fallback = %f.model_group,
                "Fallback-Modellgruppe nicht vorhanden"
            ),
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
async fn run_group(
    group: &Arc<GroupRoute>,
    req: &mut Option<ChatRequest>,
    single_shot: bool,
    stream: bool,
    budget_end: Instant,
    cap: CapacityCtx<'_>,
    routed: &mut Routed,
    scratch: &mut Scratch,
) -> Result<Outcome, Failure> {
    let policy = group.policy.retry;
    let mut tried: Vec<usize> = Vec::new();
    let mut excluded: Vec<usize> = Vec::new();
    let mut last_failure: Option<Failure> = None;
    let mut n = 0u32;
    let mut waited = false;
    while n < policy.max_attempts {
        if n > 0 && Instant::now() >= budget_end {
            break;
        }
        let now = now_ms();
        let Some((member, admit)) = group.pick(&tried, &excluded, stream, now, scratch) else {
            // Alles gesperrt: auf eine kurze Sperre (`Retry-After`) warten, sonst aufgeben.
            if !waited
                && let Some(wait) = group.cooldown_hint(&excluded, now)
                && wait <= policy.max_retry_after
                && Instant::now() + wait < budget_end
            {
                waited = true;
                tokio::time::sleep(wait).await;
                continue;
            }
            break;
        };
        waited = false;
        // Kapazität über die Zähler (instanzübergreifend je nach Backend).
        let capacity = match reserve_capacity(&member, cap).await {
            CapacityCheck::Granted(r) => r,
            CapacityCheck::Blocked => continue,
            CapacityCheck::TooLarge => {
                excluded.push(member.idx);
                continue;
            }
        };
        if n > 0 && tried.contains(&member.idx) {
            let wait =
                backoff(&policy, n).min(budget_end.saturating_duration_since(Instant::now()));
            tokio::time::sleep(wait).await;
        }
        n += 1;
        let Some(attempt_req) = (if single_shot { req.take() } else { req.clone() }) else {
            break;
        };
        routed.last = Some(Arc::clone(&member));
        let started = Instant::now();
        let mut active = Active::start(Arc::clone(&member), group, admit, cap, capacity);
        let result = attempt(&member, attempt_req, stream).await;
        let elapsed = started.elapsed();
        let desc = member.provider.describe();
        let mut rec = Attempt {
            model_group: Some(group.name.clone()),
            deployment: desc.deployment.to_owned(),
            deployment_id: desc.deployment_id,
            error: None,
            upstream_status: None,
            duration: elapsed,
        };
        match result {
            Ok(Outcome::Json(resp)) => {
                member.state.latency.update(elapsed);
                active.tokens = resp.usage.as_ref().map_or(0, |u| u.total_tokens());
                active.finish(None);
                routed.attempts.push(rec);
                return Ok(Outcome::Json(resp));
            }
            Ok(Outcome::Stream(events)) => {
                member.state.ttft.update(elapsed);
                routed.attempts.push(rec);
                return Ok(Outcome::Stream(Box::pin(Tracked {
                    inner: events,
                    active,
                    failed: None,
                })));
            }
            Err(e) => {
                rec.error = Some(e.class);
                rec.upstream_status = e.upstream_status;
                routed.attempts.push(rec);
                active.finish(Some(e.class));
                drop(active);
                tracing::debug!(
                    model_group = %group.name,
                    deployment = %desc.deployment,
                    attempt = n,
                    error = %e,
                    "Versuch fehlgeschlagen"
                );
                // Sperre nach `Retry-After` (nicht bei festen Routen ohne Routing-Funktionen).
                if let Some(after) = e.retry_after
                    && (group.policy.breaker.enabled || policy.max_attempts > 1)
                {
                    member.state.set_cooldown(now_ms(), after.min(MAX_COOLDOWN));
                }
                let class = e.class;
                if e.retryable {
                    tried.push(member.idx);
                } else if deployment_specific(class) {
                    excluded.push(member.idx);
                } else {
                    // Fehler der Anfrage selbst (400, Kontextlänge, Content-Filter …): kein
                    // weiterer Versuch in dieser Gruppe, nur ein passender Fallback.
                    return Err(Failure {
                        err: e,
                        class: Some(class),
                    });
                }
                last_failure = Some(Failure {
                    err: e,
                    class: Some(class),
                });
            }
        }
    }
    Err(last_failure.unwrap_or_else(|| Failure {
        err: unavailable(group, &excluded),
        class: None,
    }))
}

/// Ein Versuch; bei Streams wird das erste Ereignis abgewartet.
async fn attempt(
    member: &Arc<Member>,
    req: ChatRequest,
    stream: bool,
) -> Result<Outcome, UpstreamError> {
    let provider = Arc::clone(&member.provider);
    if !stream {
        return provider.chat(req).await.map(Outcome::Json);
    }
    let mut events = provider.chat_stream(req).await?;
    match events.next().await {
        Some(StreamEvent::Error(e)) => Err(UpstreamError::from(e)),
        Some(first) => Ok(Outcome::Stream(Box::pin(
            futures_util::stream::once(std::future::ready(first)).chain(events),
        ))),
        None => Err(UpstreamError::new(
            ErrorClass::Protocol,
            ApiError::new(
                ErrorKind::BadGateway,
                "Anbieter beendete den Stream ohne Ereignis",
            )
            .with_code("upstream_stream_truncated"),
        )),
    }
}

/// Laufende Anfrage auf einem Deployment: zählt offene Anfragen, gleicht die TPM-Reservierung
/// am Ende mit der Usage ab und verbucht das Ergebnis beim Circuit Breaker.
struct Active {
    member: Arc<Member>,
    breaker: super::BreakerConfig,
    tokens: u64,
    done: bool,
    counters: Option<Arc<dyn CounterBackend>>,
    capacity: Option<Reservation>,
}

impl Active {
    fn start(
        member: Arc<Member>,
        group: &GroupRoute,
        _admit: Admit,
        cap: CapacityCtx<'_>,
        capacity: Option<Reservation>,
    ) -> Self {
        member.state.in_flight.fetch_add(1, Ordering::AcqRel);
        Self {
            member,
            breaker: group.policy.breaker,
            tokens: 0,
            done: false,
            counters: capacity.as_ref().map(|_| Arc::clone(cap.counters)),
            capacity,
        }
    }

    /// Ergebnis verbuchen (`None` = Erfolg).
    fn finish(&mut self, error: Option<ErrorClass>) {
        if std::mem::replace(&mut self.done, true) {
            return;
        }
        let failure = match error {
            None => Some(false),
            Some(c) => breaker_failure(c),
        };
        if let Some(failure) = failure {
            self.member
                .state
                .breaker
                .record(&self.breaker, now_ms(), failure);
        }
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        self.member.state.in_flight.fetch_sub(1, Ordering::AcqRel);
        // TPM-Reservierung auf die gemeldete Usage korrigieren (Fehler/Abbruch ohne Usage: 0).
        if let (Some(r), Some(counters)) = (self.capacity.take(), self.counters.take())
            && let Ok(rt) = tokio::runtime::Handle::try_current()
        {
            let tokens = self.tokens;
            rt.spawn(async move { counters.settle(r, tokens).await });
        }
    }
}

/// Stream nach dem ersten Ereignis: sammelt Usage (TPM) und Fehler (Breaker); bei Abbruch
/// durch den Client wird nichts gewertet.
struct Tracked {
    inner: EventStream,
    active: Active,
    failed: Option<ErrorClass>,
}

impl Stream for Tracked {
    type Item = StreamEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<StreamEvent>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(ev)) => {
                match &ev {
                    StreamEvent::Usage(u) => this.active.tokens = u.total_tokens(),
                    StreamEvent::Error(e) => {
                        this.failed = Some(ErrorClass::from_kind(e.kind));
                    }
                    _ => {}
                }
                Poll::Ready(Some(ev))
            }
            Poll::Ready(None) => {
                this.active.finish(this.failed);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_with_jitter_and_cap() {
        let p = RetryPolicy {
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_millis(1000),
            ..RetryPolicy::default()
        };
        for _ in 0..100 {
            let b1 = backoff(&p, 1);
            assert!(b1 >= Duration::from_millis(50) && b1 <= Duration::from_millis(100));
            let b3 = backoff(&p, 3);
            assert!(b3 >= Duration::from_millis(200) && b3 <= Duration::from_millis(400));
            let b9 = backoff(&p, 9);
            assert!(b9 >= Duration::from_millis(500) && b9 <= Duration::from_millis(1000));
        }
    }

    #[test]
    fn fallback_triggers() {
        use ErrorClass as C;
        use FallbackTrigger as T;
        // Leer = any = retrybar oder deployment-bezogen oder nichts verfügbar.
        assert!(trigger_matches(&[], Some(C::ServerError)));
        assert!(trigger_matches(&[], Some(C::Quota)));
        assert!(trigger_matches(&[], None));
        assert!(!trigger_matches(&[], Some(C::ContentFilter)));
        assert!(!trigger_matches(&[], Some(C::ContextLength)));
        assert!(!trigger_matches(&[], Some(C::InvalidRequest)));
        assert!(trigger_matches(&[T::ContextLength], Some(C::ContextLength)));
        assert!(!trigger_matches(&[T::ContextLength], Some(C::ServerError)));
        assert!(trigger_matches(&[T::RateLimit], Some(C::Quota)));
        assert!(trigger_matches(&[T::Unavailable], None));
        assert!(!trigger_matches(&[T::Unavailable], Some(C::RateLimit)));
        assert!(trigger_matches(&[T::ContentFilter], Some(C::ContentFilter)));
    }
}
