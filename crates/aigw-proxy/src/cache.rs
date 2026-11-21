// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Schritt 6 der Pipeline: Antwort-Cache (AIGW-39/40, Architektur §3/§10,
//! `docs/konzept/14-cache.md`).
//!
//! Der Cache sitzt **in der Pipeline** – nach allen Pre-Plugins, vor dem Routing – und nicht
//! als Plugin-`Respond`:
//!
//! - Alle Pre-Plugins (Guardrails, PII-Maskierung, `request-transform`) laufen immer; der
//!   Schlüssel entsteht aus der Anfrage, die tatsächlich an den Upstream ginge, unabhängig von
//!   der `order` der Bindungen.
//! - Treffer durchlaufen wie Upstream-Antworten die Post- und Stream-Plugins (Ausgabe-
//!   Guardrails, Rückübersetzung von Pseudonymen); gespeichert wird die **rohe**
//!   Upstream-Antwort vor den Post-Plugins.
//! - Der semantische Cache braucht das interne Routing (Embedding-Modellgruppe), der Treffer
//!   den Recorder (Usage 0, `RequestRecord.cache`).
//!
//! Wo der Cache gilt, bestimmt das Plugin `cache` ([`aigw_plugins::builtin::cache`]) über
//! Bindungen mit Vererbung; ohne Bindung bleibt dieser Schritt ein Index-Lookup.
//!
//! Client-Steuerung (falls `clientControl`): `Cache-Control: no-cache` (nicht nachschlagen,
//! aber speichern), `no-store` (nachschlagen, nicht speichern), beides = umgehen;
//! `x-aigw-cache-ttl: <s>` (0 = nicht speichern, gedeckelt durch `clientMaxTtlSecs`).

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use aigw_cache::{
    Lookup, LookupRequest, ResponseCache, Scope, SemanticLookup, SkipReason, StoreTicket,
    StreamCollector,
};
use aigw_core::ir::{ChatRequest, ChatResponse, Content, PartKind, StopReason, StreamEvent};
use aigw_core::observe::CacheOutcome;
use aigw_plugins::builtin::cache::{CacheMode, CachePolicy, CacheScope};
use axum::http::HeaderMap;
use futures_util::Stream;

use crate::auth::AuthContext;
use crate::provider::EventStream;

/// Antwort-Header mit dem Cache-Ergebnis.
pub const CACHE_HEADER: &str = "x-aigw-cache";
/// Anfrage-Header: TTL (Sekunden) für das Speichern dieser Antwort.
pub const CACHE_TTL_HEADER: &str = "x-aigw-cache-ttl";
/// Antwort-Header: Grund für `x-aigw-cache: bypass` (`temperature` = Anfrage ohne
/// `temperature: 0` bei `cacheNonZeroTemperature: false`, `client` = Client-Steuerung).
pub const CACHE_REASON_HEADER: &str = "x-aigw-cache-reason";

/// Client-Steuerung aus den Anfrage-Headern.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ClientControl {
    no_cache: bool,
    no_store: bool,
    ttl: Option<u64>,
}

impl ClientControl {
    fn from_headers(h: &HeaderMap) -> Self {
        let mut c = Self::default();
        for v in h.get_all(axum::http::header::CACHE_CONTROL) {
            let Ok(v) = v.to_str() else { continue };
            for d in v.split(',') {
                match d.trim().to_ascii_lowercase().as_str() {
                    "no-cache" => c.no_cache = true,
                    "no-store" => c.no_store = true,
                    _ => {}
                }
            }
        }
        c.ttl = h
            .get(CACHE_TTL_HEADER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        c
    }
}

/// Antwort-Cache für Embeddings (F174): nur exakt (gleiche Eingaben, Dimension, Modellgruppe
/// und Scope), sonst dieselbe Richtlinie wie für Chat (Plugin `cache`: Scope, TTL,
/// Client-Steuerung). Gespeichert wird die IR-Antwort; das Ausgabeformat (`float`/`base64`)
/// wählt jede Anfrage selbst.
pub(crate) struct EmbeddingsStep {
    cache: Arc<ResponseCache>,
    key: aigw_cache::Hash32,
    ttl: Duration,
    may_store: bool,
    outcome: CacheOutcome,
}

impl EmbeddingsStep {
    /// Nachschlagen; `None` ohne Cache bzw. ohne Richtlinie. Bei einem Treffer liegt die
    /// Antwort bei.
    pub(crate) async fn begin(
        cache: Option<&Arc<ResponseCache>>,
        policy: Option<Arc<CachePolicy>>,
        auth: &AuthContext,
        model_group: &str,
        req: &aigw_core::ir::embeddings::EmbeddingsRequest,
        headers: &HeaderMap,
    ) -> Option<(Self, Option<aigw_core::ir::embeddings::EmbeddingsResponse>)> {
        let cache = cache?;
        if !cache.config().enabled {
            return None;
        }
        let policy = policy?;
        let ctl = if policy.client_control {
            ClientControl::from_headers(headers)
        } else {
            ClientControl::default()
        };
        let ttl_secs = match ctl.ttl {
            Some(t) => t.min(policy.client_max_ttl_secs),
            None => policy.ttl_secs,
        };
        let may_store = !ctl.no_store && ttl_secs > 0;
        let key = aigw_cache::embeddings_key(&resolve_scope(policy.scope, auth), model_group, req);
        let mut step = Self {
            cache: Arc::clone(cache),
            key,
            ttl: Duration::from_secs(ttl_secs),
            may_store,
            outcome: CacheOutcome::Bypass,
        };
        let mut hit = None;
        if !ctl.no_cache {
            hit = cache.lookup_embeddings(&key).await;
            step.outcome = if hit.is_some() {
                CacheOutcome::HitExact
            } else {
                CacheOutcome::Miss
            };
        }
        if hit.is_none() && !may_store {
            cache.metrics().skipped(SkipReason::NoStore);
        }
        cache.metrics().lookup(step.outcome);
        Some((step, hit))
    }

    /// Ergebnis für Header und Request-Log.
    pub(crate) fn outcome(&self) -> CacheOutcome {
        self.outcome
    }

    /// Speichert eine Upstream-Antwort (nach einem Miss).
    pub(crate) fn store(&self, resp: &aigw_core::ir::embeddings::EmbeddingsResponse) {
        if self.may_store && self.outcome != CacheOutcome::HitExact && !resp.data.is_empty() {
            self.cache.store_embeddings(self.key, resp, self.ttl);
        }
    }
}

/// Scope laut Richtlinie, aufgelöst aus Key, Team und Organisation. Fehlt die Ebene (Key ohne
/// Team), gilt die nächst engere – nie eine weitere.
fn resolve_scope(policy: CacheScope, auth: &AuthContext) -> Scope {
    if policy == CacheScope::Global {
        return Scope::Global;
    }
    let Some(k) = auth.key.as_deref() else {
        return Scope::Anonymous;
    };
    let key = || Scope::Key(k.key.id.to_string());
    let team = || k.team.as_ref().map(|t| Scope::Team(t.id.to_string()));
    match policy {
        CacheScope::Key => key(),
        CacheScope::Team => team().unwrap_or_else(key),
        CacheScope::Organization => k
            .organization
            .as_ref()
            .map(|o| Scope::Organization(o.id.to_string()))
            .or_else(team)
            .unwrap_or_else(key),
        CacheScope::Global => Scope::Global,
    }
}

/// Darf die Anfrage laut Richtlinie gecacht werden (Temperatur)?
fn cacheable_request(req: &ChatRequest, policy: &CachePolicy) -> bool {
    policy.cache_non_zero_temperature || req.sampling.temperature == Some(0.0)
}

/// Darf die Antwort gespeichert werden?
fn storable(resp: &ChatResponse, policy: &CachePolicy) -> Result<(), SkipReason> {
    let Some(choice) = resp.choices.first() else {
        return Err(SkipReason::Incomplete);
    };
    for c in &resp.choices {
        match &c.stop_reason {
            Some(StopReason::EndTurn | StopReason::MaxTokens | StopReason::StopSequence) | None => {
            }
            Some(StopReason::ToolUse) => {
                if !policy.cache_tool_calls {
                    return Err(SkipReason::ToolCalls);
                }
            }
            Some(_) => return Err(SkipReason::StopReason),
        }
        if !policy.cache_tool_calls
            && let Content::Parts(parts) = &c.message.content
            && parts
                .iter()
                .any(|p| matches!(p.kind, PartKind::ToolCall { .. }))
        {
            return Err(SkipReason::ToolCalls);
        }
    }
    if choice.stop_reason.is_none() {
        return Err(SkipReason::Incomplete);
    }
    Ok(())
}

/// Cache-Schritt einer Anfrage.
pub(crate) struct CacheStep {
    cache: Arc<ResponseCache>,
    policy: Arc<CachePolicy>,
    outcome: CacheOutcome,
    hit: Option<ChatResponse>,
    /// Grund bei `bypass` (Header `x-aigw-cache-reason`).
    reason: Option<&'static str>,
    /// Zum Speichern (nur bei Miss bzw. `no-cache`, wenn Speichern erlaubt).
    ticket: Option<StoreTicket>,
    ttl: Duration,
}

impl CacheStep {
    /// Nachschlagen; `None`, wenn für die Anfrage kein Cache gilt (keine Bindung, Cache aus).
    pub(crate) async fn begin(
        cache: Option<&Arc<ResponseCache>>,
        policy: Option<Arc<CachePolicy>>,
        auth: &AuthContext,
        request_id: &str,
        model_group: &str,
        req: &ChatRequest,
        headers: &HeaderMap,
    ) -> Option<Self> {
        let cache = cache?;
        if !cache.config().enabled {
            return None;
        }
        let policy = policy?;
        let ctl = if policy.client_control {
            ClientControl::from_headers(headers)
        } else {
            ClientControl::default()
        };
        let ttl_secs = match ctl.ttl {
            Some(t) => t.min(policy.client_max_ttl_secs),
            None => policy.ttl_secs,
        };
        let may_store = !ctl.no_store && ttl_secs > 0;
        let mut step = Self {
            cache: Arc::clone(cache),
            policy: Arc::clone(&policy),
            outcome: CacheOutcome::Bypass,
            hit: None,
            reason: None,
            ticket: None,
            ttl: Duration::from_secs(ttl_secs),
        };
        if !cacheable_request(req, &policy) || (ctl.no_cache && !may_store) {
            step.reason = Some(if cacheable_request(req, &policy) {
                "client"
            } else {
                "temperature"
            });
            cache.metrics().lookup(CacheOutcome::Bypass);
            return Some(step);
        }
        let embedding_model = policy.semantic.embedding_model.as_deref().or(cache
            .config()
            .semantic
            .embedding_model
            .as_deref());
        let semantic = match (policy.mode, embedding_model) {
            (CacheMode::Semantic, Some(m)) if cache.semantic_available() => Some(SemanticLookup {
                embedding_model: m,
                threshold: policy.semantic.threshold_for(model_group),
                max_chars: policy.semantic.max_chars,
            }),
            _ => None,
        };
        // Auslöser des Embedding-Aufrufs: Kosten trägt der anfragende Key (F169).
        let embed_ctx = semantic.as_ref().map(|_| aigw_cache::EmbedContext {
            request_id: request_id.to_owned(),
            caller: Some(Arc::new(auth.clone()) as Arc<dyn std::any::Any + Send + Sync>),
        });
        let lookup = cache
            .lookup(&LookupRequest {
                scope: resolve_scope(policy.scope, auth),
                model_group,
                request: req,
                semantic,
                skip_lookup: ctl.no_cache,
                embed_ctx: embed_ctx.as_ref(),
            })
            .await;
        match lookup {
            Lookup::Hit { kind, response } => {
                step.outcome = kind.outcome();
                step.hit = Some(*response);
            }
            Lookup::Miss(ticket) => {
                step.outcome = if ctl.no_cache {
                    step.reason = Some("client");
                    CacheOutcome::Bypass
                } else {
                    CacheOutcome::Miss
                };
                if may_store {
                    step.ticket = Some(ticket);
                } else {
                    cache.metrics().skipped(SkipReason::NoStore);
                }
            }
        }
        cache.metrics().lookup(step.outcome);
        Some(step)
    }

    /// Ergebnis für Header und Request-Log.
    pub(crate) fn outcome(&self) -> CacheOutcome {
        self.outcome
    }

    /// Grund bei `bypass` (`temperature` | `client`).
    pub(crate) fn reason(&self) -> Option<&'static str> {
        self.reason
    }

    /// Treffer entnehmen.
    pub(crate) fn take_hit(&mut self) -> Option<ChatResponse> {
        self.hit.take()
    }

    /// Zeichen je simuliertem Chunk.
    pub(crate) fn chunk_chars(&self) -> usize {
        self.cache.config().replay_chunk_chars
    }

    /// Nicht gestreamte (bzw. gepufferte) Upstream-Antwort speichern – vor den Post-Plugins.
    pub(crate) fn store_response(&mut self, resp: &ChatResponse) {
        let Some(ticket) = self.ticket.take() else {
            return;
        };
        match storable(resp, &self.policy) {
            Ok(()) => {
                self.cache.store(ticket, resp, self.ttl);
            }
            Err(r) => self.cache.metrics().skipped(r),
        }
    }

    /// Gestreamte Upstream-Antwort mitschneiden und am regulären Ende speichern.
    pub(crate) fn tee(&mut self, events: EventStream) -> EventStream {
        let Some(ticket) = self.ticket.take() else {
            return events;
        };
        let limit = usize::try_from(self.cache.config().max_entry_bytes).unwrap_or(usize::MAX);
        Box::pin(CacheTee {
            inner: events,
            collector: Some(StreamCollector::new(limit)),
            ticket: Some(ticket),
            cache: Arc::clone(&self.cache),
            policy: Arc::clone(&self.policy),
            ttl: self.ttl,
        })
    }
}

/// Ereignisstrom, der die Antwort für den Cache zusammensetzt. Gespeichert wird nur bei
/// regulärem Ende (Endgrund, kein Fehlerereignis); bricht der Client ab, wird nichts
/// gespeichert.
struct CacheTee {
    inner: EventStream,
    collector: Option<StreamCollector>,
    ticket: Option<StoreTicket>,
    cache: Arc<ResponseCache>,
    policy: Arc<CachePolicy>,
    ttl: Duration,
}

impl Stream for CacheTee {
    type Item = StreamEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<StreamEvent>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(ev)) => {
                if let Some(c) = this.collector.as_mut() {
                    c.push(&ev);
                }
                Poll::Ready(Some(ev))
            }
            Poll::Ready(None) => {
                if let (Some(c), Some(ticket)) = (this.collector.take(), this.ticket.take()) {
                    let overflow = c.overflowed();
                    match c.finish() {
                        Some(resp) => match storable(&resp, &this.policy) {
                            Ok(()) => {
                                this.cache.store(ticket, &resp, this.ttl);
                            }
                            Err(r) => this.cache.metrics().skipped(r),
                        },
                        None if overflow => this.cache.metrics().skipped(SkipReason::TooLarge),
                        None => this.cache.metrics().skipped(SkipReason::Incomplete),
                    }
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Embedder des semantischen Caches über das **interne Routing** des Gateways: die
/// Embedding-Modellgruppe wird wie `POST /v1/embeddings` (AIGW-21) geroutet – Auswahl,
/// Retries, Fallbacks, Circuit Breaker, Deployment-Kapazität –, ohne Key-Prüfung.
///
/// **Abrechnung (F169):** Mit [`RoutedEmbedder::with_billing`] (Standard über
/// [`RoutedEmbedder::for_state`]) wird jeder Aufruf aus einer Anfrage als eigener
/// [`RequestRecord`](aigw_core::observe::RequestRecord) erfasst – Endpunkt `embeddings`,
/// `cache = cache-embedding`, Key/Team/Organisation der auslösenden Anfrage,
/// `client_request_id` = deren Request-ID – und seine Kosten werden auf die Budgets des
/// anfragenden Keys gebucht (ohne Reservierung; Rate-Limits zählen ihn nicht).
pub struct RoutedEmbedder {
    routes: Arc<dyn crate::ModelRoutes>,
    counters: Arc<dyn aigw_counters::CounterBackend>,
    billing: Option<Billing>,
}

/// Abrechnung der Embedding-Aufrufe (F169).
struct Billing {
    observer: Arc<dyn aigw_core::observe::RequestObserver>,
    limits: Arc<crate::Limiter>,
}

impl std::fmt::Debug for RoutedEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoutedEmbedder")
            .field("billing", &self.billing.is_some())
            .finish_non_exhaustive()
    }
}

impl RoutedEmbedder {
    /// Embedder über `routes`; `counters` = Backend der Deployment-Kapazität. Ohne
    /// Abrechnung (siehe [`RoutedEmbedder::with_billing`]).
    pub fn new(
        routes: Arc<dyn crate::ModelRoutes>,
        counters: Arc<dyn aigw_counters::CounterBackend>,
    ) -> Self {
        Self {
            routes,
            counters,
            billing: None,
        }
    }

    /// Erfasst Embedding-Aufrufe als eigene Einträge bei `observer` und bucht ihre Kosten
    /// über die Budgets von `limits` auf den anfragenden Key (F169).
    #[must_use]
    pub fn with_billing(
        mut self,
        observer: Arc<dyn aigw_core::observe::RequestObserver>,
        limits: Arc<crate::Limiter>,
    ) -> Self {
        self.billing = Some(Billing { observer, limits });
        self
    }

    /// Embedder für den Zustand der Datenebene (mit Abrechnung).
    pub fn for_state(st: &crate::ProxyState) -> Self {
        Self::new(Arc::clone(&st.routes), Arc::clone(st.limits.counters()))
            .with_billing(Arc::clone(&st.observer), Arc::clone(&st.limits))
    }

    async fn run(
        &self,
        model: &str,
        text: &str,
    ) -> crate::routing::exec::Routed<aigw_core::ir::embeddings::EmbeddingsResponse> {
        use aigw_core::ir::embeddings::{EmbeddingInput, EmbeddingsRequest};
        let Some(route) = self.routes.route(model) else {
            return crate::routing::exec::Routed {
                result: Err(aigw_core::ir::UpstreamError::from(
                    aigw_core::ir::ApiError::new(
                        aigw_core::ir::ErrorKind::NotFound,
                        format!("Embedding-Modellgruppe {model:?} ist nicht konfiguriert"),
                    ),
                )),
                attempts: Vec::new(),
                last: None,
                fallback: None,
                fallback_skipped: Vec::new(),
            };
        };
        let req = EmbeddingsRequest {
            model: model.to_owned(),
            input: EmbeddingInput::Text(vec![text.to_owned()]),
            single_input: true,
            encoding_format: None,
            dimensions: None,
            user: None,
            extra: Default::default(),
            source: None,
        };
        crate::routing::exec::execute_op(
            self.routes.as_ref(),
            route,
            &crate::endpoints::EmbeddingsOp,
            req,
            crate::routing::exec::CapacityCtx {
                counters: &self.counters,
                tokens: crate::observe::estimate_tokens(text.len()),
                grants: None,
            },
        )
        .await
    }

    /// Eigener Eintrag und Budgetbuchung für einen Embedding-Aufruf (F169).
    #[allow(clippy::too_many_arguments)]
    fn bill(
        billing: &Billing,
        caller: &AuthContext,
        ctx: &aigw_cache::EmbedContext,
        model: &str,
        text: &str,
        routed: &crate::routing::exec::Routed<aigw_core::ir::embeddings::EmbeddingsResponse>,
        started_at: std::time::SystemTime,
        start: std::time::Instant,
    ) {
        use aigw_core::observe::RequestRecord;
        use aigw_core::wire::WireFormat;
        let mut r = RequestRecord::new(
            uuid::Uuid::now_v7().to_string(),
            WireFormat::OpenAiEmbeddings,
            200,
        );
        r.started_at = started_at;
        r.duration = start.elapsed();
        r.model_alias = Some(model.to_owned());
        r.cache = Some(CacheOutcome::Embedding);
        // F191: Verweis auf die auslösende Anfrage (Log-Explorer: parentRequestId).
        r.parent_request_id = Some(ctx.request_id.clone());
        r.api_key_id = caller.api_key_id();
        r.team_id = caller.team_id();
        r.organization_id = caller.organization_id();
        r.attempts.clone_from(&routed.attempts);
        if let Some(m) = &routed.last {
            let d = m.provider().describe();
            r.deployment_id = d.deployment_id;
            r.model_group = d.model_group.map(str::to_owned);
            r.provider = d.provider;
            r.provider_model = Some(d.provider_model.to_owned());
        }
        let group = caller
            .snapshot
            .as_ref()
            .and_then(|s| s.model_group(model).cloned());
        match &routed.result {
            Ok(crate::routing::exec::Outcome::Json(resp)) => {
                r.usage = resp.usage.clone();
                r.usage_estimated = resp.usage_estimated;
                if r.usage.input_tokens == 0 {
                    // Anbieter ohne Usage: mit dem Tokenizer der Embedding-Gruppe schätzen.
                    r.usage.input_tokens = billing.limits.tokenizer(group.as_deref()).count(text);
                    r.usage_estimated = true;
                }
            }
            Ok(crate::routing::exec::Outcome::Stream(_)) => {}
            Err(e) => {
                r.status = e.error.status();
                r.error_kind = Some(e.error.kind);
            }
        }
        if !r.is_error()
            && let (Some(snap), Some(provider), Some(pm)) = (
                caller.snapshot.as_ref(),
                r.provider,
                r.provider_model.as_deref(),
            )
            && let Some((cost, b)) = aigw_billing::cost_breakdown_at(
                snap,
                provider,
                pm,
                time::OffsetDateTime::from(started_at),
                &r.usage,
            )
        {
            r.cost = Some(cost);
            r.cost_breakdown = b;
        }
        if let (Some(cost), Some(budgets), Some(k)) =
            (r.cost, billing.limits.budgets(), caller.key.as_ref())
        {
            budgets.charge(
                &crate::limits::budget_targets(k),
                cost,
                crate::limits::unix_ms(started_at),
            );
        }
        billing.observer.record(r);
    }
}

#[async_trait::async_trait]
impl aigw_cache::Embedder for RoutedEmbedder {
    async fn embed(&self, model: &str, text: &str) -> Result<Vec<f32>, String> {
        self.embed_for(model, text, &aigw_cache::EmbedContext::default())
            .await
    }

    async fn embed_for(
        &self,
        model: &str,
        text: &str,
        ctx: &aigw_cache::EmbedContext,
    ) -> Result<Vec<f32>, String> {
        let started_at = std::time::SystemTime::now();
        let start = std::time::Instant::now();
        let routed = self.run(model, text).await;
        if let (Some(billing), Some(caller)) = (
            &self.billing,
            ctx.caller
                .as_ref()
                .and_then(|c| c.downcast_ref::<AuthContext>()),
        ) {
            Self::bill(
                billing, caller, ctx, model, text, &routed, started_at, start,
            );
        }
        match routed.result.map_err(|e| e.to_string())? {
            crate::routing::exec::Outcome::Json(resp) => resp
                .data
                .into_iter()
                .next()
                .map(|e| e.vector)
                .ok_or_else(|| "Embedding-Antwort ohne Vektor".to_owned()),
            crate::routing::exec::Outcome::Stream(_) => {
                Err("unerwartete Stream-Antwort".to_owned())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use aigw_core::ir::{Choice, Message, Part, Role};
    use axum::http::HeaderValue;

    use super::*;

    fn resp(reason: StopReason, tool: bool) -> ChatResponse {
        let content = if tool {
            Content::Parts(vec![Part::new(PartKind::ToolCall {
                id: "c".into(),
                name: "f".into(),
                arguments: "{}".into(),
            })])
        } else {
            Content::Text("x".into())
        };
        ChatResponse {
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    content,
                    name: None,
                    extra: Default::default(),
                },
                stop_reason: Some(reason),
                stop_sequence: None,
                extra: Default::default(),
            }],
            ..ChatResponse::default()
        }
    }

    #[test]
    fn client_control_headers() {
        let mut h = HeaderMap::new();
        h.insert(
            "cache-control",
            HeaderValue::from_static("No-Cache, max-age=0"),
        );
        h.append("cache-control", HeaderValue::from_static("no-store"));
        h.insert(CACHE_TTL_HEADER, HeaderValue::from_static(" 120 "));
        let c = ClientControl::from_headers(&h);
        assert!(c.no_cache && c.no_store);
        assert_eq!(c.ttl, Some(120));
        assert_eq!(
            ClientControl::from_headers(&HeaderMap::new()),
            ClientControl::default()
        );
    }

    #[test]
    fn storable_rules() {
        let p = CachePolicy::default();
        assert_eq!(storable(&resp(StopReason::EndTurn, false), &p), Ok(()));
        assert_eq!(storable(&resp(StopReason::MaxTokens, false), &p), Ok(()));
        assert_eq!(
            storable(&resp(StopReason::ToolUse, true), &p),
            Err(SkipReason::ToolCalls)
        );
        assert_eq!(
            storable(&resp(StopReason::ContentFilter, false), &p),
            Err(SkipReason::StopReason)
        );
        assert_eq!(
            storable(&ChatResponse::default(), &p),
            Err(SkipReason::Incomplete)
        );
        let tools = CachePolicy {
            cache_tool_calls: true,
            ..CachePolicy::default()
        };
        assert_eq!(storable(&resp(StopReason::ToolUse, true), &tools), Ok(()));
    }

    #[test]
    fn temperature_rule() {
        let p = CachePolicy::default();
        let mut r = ChatRequest::default();
        assert!(!cacheable_request(&r, &p)); // ohne Angabe: Anbieter-Standard > 0
        r.sampling.temperature = Some(0.0);
        assert!(cacheable_request(&r, &p));
        r.sampling.temperature = Some(0.7);
        assert!(!cacheable_request(&r, &p));
        let any = CachePolicy {
            cache_non_zero_temperature: true,
            ..CachePolicy::default()
        };
        assert!(cacheable_request(&r, &any));
    }
}
