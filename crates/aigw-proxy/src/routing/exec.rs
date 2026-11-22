// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

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
    ApiError, ChatRequest, ChatResponse, ErrorClass, ErrorKind, StreamEvent, UpstreamError, Usage,
};
use aigw_core::observe::Attempt;
use aigw_core::resource::FallbackTrigger;
use aigw_counters::{CounterBackend, LimitCheck, Reservation};
use futures_util::stream::SelectAll;
use futures_util::{Stream, StreamExt as _};

use super::breaker::Admit;
use super::select::rand_below;
use super::state::now_ms;
use super::{GroupRoute, Member, RetryPolicy, Scratch};
use aigw_providers::Capability;

use crate::provider::{BoxFuture, ChatProvider, EventStream};
use crate::routes::ModelRoutes;

/// Längste beachtete Sperre nach `Retry-After`.
const MAX_COOLDOWN: Duration = Duration::from_secs(300);
/// Höchstens so viele Modellgruppen je Anfrage (Ursprung + Fallbacks, F69); die Admin-API
/// und GitOps lehnen längere Ketten ab.
const MAX_GROUPS: usize = aigw_core::validate::MAX_FALLBACKS + 1;

/// Erfolgreiche Antwort.
pub(crate) enum Outcome<T = ChatResponse> {
    /// Nicht gestreamt.
    Json(T),
    /// Stream (erstes Ereignis bereits geprüft).
    Stream(EventStream),
}

/// Eine Art von Anfrage, die über das Routing (Auswahl, Retries, Fallbacks, Breaker) läuft
/// (AIGW-21): Chat, Embeddings, Bilder, Audio. Alle teilen dieselbe Ausführung; nur der
/// eigentliche Aufruf eines Deployments unterscheidet sich.
pub(crate) trait Operation {
    /// IR-Anfrage (wird je Versuch geklont, wenn ein zweiter Versuch möglich ist).
    type Req: Clone + Send + 'static;
    /// IR-Antwort (nicht gestreamt).
    type Out: Send + 'static;
    /// Benötigte Fähigkeit der Modellgruppe und der Deployments.
    const CAPABILITY: Capability;
    /// Gestreamte Antwort (Retries nur bis zum ersten Ereignis).
    fn stream(&self) -> bool {
        false
    }
    /// Ein Versuch gegen ein Deployment.
    fn call(
        &self,
        provider: Arc<dyn ChatProvider>,
        req: Self::Req,
    ) -> BoxFuture<Result<Outcome<Self::Out>, UpstreamError>>;
    /// Verbrauchte Tokens (für TPM-Kapazität).
    fn tokens(out: &Self::Out) -> u64;
}

/// Chat (Chat Completions, Messages, Responses).
pub(crate) struct ChatOp {
    pub(crate) stream: bool,
}

impl Operation for ChatOp {
    type Req = ChatRequest;
    type Out = ChatResponse;
    const CAPABILITY: Capability = Capability::Chat;

    fn stream(&self) -> bool {
        self.stream
    }

    fn call(
        &self,
        provider: Arc<dyn ChatProvider>,
        req: ChatRequest,
    ) -> BoxFuture<Result<Outcome, UpstreamError>> {
        let n = req.choices();
        if n > 1 && !provider.native_choices() {
            // Anbieter ohne `n` (F12): n parallele Anfragen, Ergebnis zusammengeführt.
            return Box::pin(fan_out(provider, req, n, self.stream));
        }
        Box::pin(attempt(provider, req, self.stream))
    }

    fn tokens(out: &ChatResponse) -> u64 {
        out.usage.as_ref().map_or(0, Usage::total_tokens)
    }
}

/// Prüft, ob der Key eine Modellgruppe (kanonischer Name) nutzen darf (F80).
pub(crate) type Grants<'a> = &'a (dyn Fn(&str) -> bool + Sync);

/// Kontext der Ausführung: Zähler für die Deployment-Kapazität (`capacity.rpm`/`tpm`) und
/// Freigaben des Keys für Fallback-Gruppen.
#[derive(Clone, Copy)]
pub(crate) struct CapacityCtx<'a> {
    /// Dasselbe Backend wie die Limits (lokal, `postgres-sync` oder Redis).
    pub(crate) counters: &'a Arc<dyn CounterBackend>,
    /// Reservierte Tokens je Versuch (geschätzte Eingabe + `max_tokens`).
    pub(crate) tokens: u64,
    /// Freigaben des Keys (`None` = keine Prüfung, z. B. ohne Snapshot oder intern).
    pub(crate) grants: Option<Grants<'a>>,
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
pub(crate) struct Routed<T = ChatResponse> {
    pub(crate) result: Result<Outcome<T>, UpstreamError>,
    pub(crate) attempts: Vec<Attempt>,
    /// Zuletzt angefragtes Deployment.
    pub(crate) last: Option<Arc<Member>>,
    /// Modellgruppe des letzten Versuchs, falls sie per Fallback erreicht wurde.
    pub(crate) fallback: Option<String>,
    /// Fallback-Gruppen, die übersprungen wurden, weil sie für Key/Team nicht freigegeben
    /// sind (F80, Header `x-aigw-fallback-skipped`).
    pub(crate) fallback_skipped: Vec<String>,
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

/// Kein Deployment verfügbar. Sind alle nur wegen `Retry-After` oder erschöpfter Kapazität
/// gesperrt (F83), ist das eine Drosselung: `429` mit `Retry-After`; sonst (offene Circuit
/// Breaker) `503`.
fn unavailable(group: &GroupRoute, excluded: &[usize]) -> UpstreamError {
    let now = now_ms();
    let retry_after = group.wait_hint(excluded, now);
    let mut e = if group.throttled_only(excluded, now) {
        UpstreamError::new(
            ErrorClass::RateLimit,
            ApiError::new(
                ErrorKind::RateLimit,
                format!(
                    "Modellgruppe {:?}: alle Deployments ausgelastet (Kapazität erschöpft oder \
                     vom Anbieter gedrosselt) – später erneut versuchen",
                    group.name
                ),
            )
            .with_code("deployments_rate_limited"),
        )
    } else {
        UpstreamError::new(
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
    }
    .with_retry_after(retry_after);
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
    let op = ChatOp {
        stream: req.is_stream(),
    };
    execute_op(routes, group, &op, req, cap).await
}

/// Führt eine Anfrage beliebiger Art über `group` aus – mit Retries und Fallbacks. Nimmt die
/// Gruppe diese Art nicht an, gibt es sofort `400` (`unsupported_capability`); Deployments
/// ohne die Fähigkeit werden nie gewählt, Fallback-Gruppen ohne sie übersprungen.
pub(crate) async fn execute_op<O: Operation>(
    routes: &dyn ModelRoutes,
    group: Arc<GroupRoute>,
    op: &O,
    req: O::Req,
    cap: CapacityCtx<'_>,
) -> Routed<O::Out> {
    if !group.supports(O::CAPABILITY) {
        return Routed {
            result: Err(UpstreamError::from(group.unsupported_error(O::CAPABILITY))),
            attempts: Vec::new(),
            last: None,
            fallback: None,
            fallback_skipped: Vec::new(),
        };
    }
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
        fallback_skipped: Vec::new(),
    };
    let mut scratch = Scratch::default();
    let mut visited: Vec<String> = vec![group.name.clone()];
    let mut next_fallback = 0;
    let origin = Arc::clone(&group);
    let mut current = group;
    loop {
        match run_group(
            &current,
            op,
            &mut req,
            single_shot,
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
                    find_fallback(
                        routes,
                        &origin,
                        &mut next_fallback,
                        &visited,
                        failure.class,
                        O::CAPABILITY,
                        cap.grants,
                        &mut routed.fallback_skipped,
                    )
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

/// Nächste passende Fallback-Gruppe der Ursprungsgruppe. Gruppen, die für Key/Team nicht
/// freigegeben sind, werden übersprungen und gemerkt (F80) – außer die Ursprungsgruppe setzt
/// `settings.fallbackIgnoresGrants`.
#[allow(clippy::too_many_arguments)]
fn find_fallback(
    routes: &dyn ModelRoutes,
    origin: &GroupRoute,
    next: &mut usize,
    visited: &[String],
    class: Option<ErrorClass>,
    need: Capability,
    grants: Option<Grants<'_>>,
    skipped: &mut Vec<String>,
) -> Option<Arc<GroupRoute>> {
    while let Some(f) = origin.fallbacks.get(*next) {
        *next += 1;
        if visited.contains(&f.model_group) || !trigger_matches(&f.on, class) {
            continue;
        }
        match routes.route(&f.model_group) {
            Some(g) if !visited.iter().any(|v| *v == g.name) && g.supports(need) => {
                if !origin.policy.fallback_ignores_grants
                    && let Some(allowed) = grants
                    && !allowed(&g.name)
                {
                    tracing::debug!(
                        model_group = %origin.name,
                        fallback = %g.name,
                        "Fallback-Gruppe für den Key nicht freigegeben – übersprungen"
                    );
                    if !skipped.contains(&g.name) {
                        skipped.push(g.name.clone());
                    }
                    continue;
                }
                return Some(g);
            }
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
async fn run_group<O: Operation>(
    group: &Arc<GroupRoute>,
    op: &O,
    req: &mut Option<O::Req>,
    single_shot: bool,
    budget_end: Instant,
    cap: CapacityCtx<'_>,
    routed: &mut Routed<O::Out>,
    scratch: &mut Scratch,
) -> Result<Outcome<O::Out>, Failure> {
    let stream = op.stream();
    let policy = group.policy.retry;
    let mut tried: Vec<usize> = Vec::new();
    // Deployments ohne die nötige Fähigkeit kommen nie in Frage.
    let mut excluded: Vec<usize> = group
        .members
        .iter()
        .filter(|m| !m.provider.supports(O::CAPABILITY))
        .map(|m| m.idx)
        .collect();
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
        let result = op.call(Arc::clone(&member.provider), attempt_req).await;
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
                active.tokens = O::tokens(&resp);
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

/// Ein Chat-Versuch; bei Streams wird das erste Ereignis abgewartet.
async fn attempt(
    provider: Arc<dyn ChatProvider>,
    req: ChatRequest,
    stream: bool,
) -> Result<Outcome, UpstreamError> {
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

/// `n > 1` bei Anbietern ohne `n` (F12): `n` Anfragen (je ohne `n`) parallel an dasselbe
/// Deployment. Schlägt eine vor der Antwort bzw. dem ersten Ereignis fehl, gilt der ganze
/// Versuch als fehlgeschlagen (Retry/Fallback wie bei einer Einzelanfrage). Varianten tragen
/// `index` 0…n-1, die Usage ist die Summe.
async fn fan_out(
    provider: Arc<dyn ChatProvider>,
    mut req: ChatRequest,
    n: u32,
    stream: bool,
) -> Result<Outcome, UpstreamError> {
    req.n = None;
    let calls = (0..n).map(|_| attempt(Arc::clone(&provider), req.clone(), stream));
    let results = futures_util::future::join_all(calls).await;
    if stream {
        let mut streams = Vec::with_capacity(results.len());
        for r in results {
            match r? {
                Outcome::Stream(s) => streams.push(s),
                Outcome::Json(_) => {
                    return Err(UpstreamError::from(ApiError::new(
                        ErrorKind::Internal,
                        "unerwartete Antwort ohne Stream",
                    )));
                }
            }
        }
        return Ok(Outcome::Stream(Box::pin(MergeChoices::new(streams))));
    }
    let mut merged: Option<ChatResponse> = None;
    let mut usage: Option<Usage> = None;
    let mut choices = Vec::new();
    for r in results {
        let Outcome::Json(mut resp) = r? else {
            return Err(UpstreamError::from(ApiError::new(
                ErrorKind::Internal,
                "unerwartete Stream-Antwort",
            )));
        };
        if let Some(u) = resp.usage.take() {
            usage.get_or_insert_with(Usage::default).add(&u);
        }
        choices.append(&mut resp.choices);
        if merged.is_none() {
            merged = Some(resp);
        }
    }
    let mut resp = merged.unwrap_or_default();
    for (i, c) in choices.iter_mut().enumerate() {
        c.index = u32::try_from(i).unwrap_or(u32::MAX);
    }
    resp.choices = choices;
    resp.usage = usage;
    Ok(Outcome::Json(resp))
}

type IndexedStream = Pin<Box<dyn Stream<Item = (u32, StreamEvent)> + Send>>;

/// Führt `n` Ereignisströme (je eine Variante) zu einem Strom mit [`StreamEvent::Choice`]
/// zusammen (verschränkt, wie sie eintreffen): ein `Start` (des ersten Stroms), je Variante
/// ihre Deltas und ihr `Stop`, am Ende die summierte Usage. Ein Fehler in einer Variante
/// beendet den ganzen Strom (die übrigen Anfragen werden abgebrochen).
pub(crate) struct MergeChoices {
    inner: SelectAll<IndexedStream>,
    usage: Vec<Option<Usage>>,
    current: u32,
    started: bool,
    pending: Option<StreamEvent>,
    done: bool,
}

impl MergeChoices {
    pub(crate) fn new(streams: Vec<EventStream>) -> Self {
        let n = streams.len();
        let inner = futures_util::stream::select_all(streams.into_iter().enumerate().map(
            |(i, s)| -> IndexedStream {
                let i = u32::try_from(i).unwrap_or(u32::MAX);
                Box::pin(s.map(move |ev| (i, ev)))
            },
        ));
        Self {
            inner,
            usage: vec![None; n],
            current: 0,
            started: false,
            pending: None,
            done: false,
        }
    }
}

impl Stream for MergeChoices {
    type Item = StreamEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<StreamEvent>> {
        let this = self.get_mut();
        if let Some(ev) = this.pending.take() {
            return Poll::Ready(Some(ev));
        }
        loop {
            if this.done {
                return Poll::Ready(None);
            }
            match this.inner.poll_next_unpin(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    this.done = true;
                    let mut total: Option<Usage> = None;
                    for u in this.usage.iter().flatten() {
                        total.get_or_insert_with(Usage::default).add(u);
                    }
                    return Poll::Ready(total.map(StreamEvent::Usage));
                }
                Poll::Ready(Some((i, ev))) => match ev {
                    StreamEvent::Start { .. } => {
                        if !std::mem::replace(&mut this.started, true) {
                            return Poll::Ready(Some(ev));
                        }
                    }
                    StreamEvent::Usage(u) => {
                        if let Some(slot) =
                            usize::try_from(i).ok().and_then(|i| this.usage.get_mut(i))
                        {
                            *slot = Some(u);
                        }
                    }
                    StreamEvent::Error(_) => {
                        this.done = true;
                        return Poll::Ready(Some(ev));
                    }
                    // Einzelanfragen haben nur Variante 0.
                    StreamEvent::Choice { .. } => {}
                    ev => {
                        if i != this.current {
                            this.current = i;
                            this.pending = Some(ev);
                            return Poll::Ready(Some(StreamEvent::Choice { index: i }));
                        }
                        return Poll::Ready(Some(ev));
                    }
                },
            }
        }
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
                    StreamEvent::Choice { .. } => {}
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
