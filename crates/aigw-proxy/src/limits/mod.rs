// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Limits und Budgets im Request-Pfad (Architektur §3 Schritt 4, §8; AIGW-30, AIGW-31).
//!
//! **Eine Einhängestelle:** Der Handler ruft nach Schlüsselprüfung und Modellfreigabe und vor
//! dem Routing genau einmal [`Limiter::admit`] auf. Das ergibt entweder eine [`Admission`]
//! (Reservierung aller Limits, geprüfte Budgets) oder eine Ablehnung (`429` im
//! Eingangsformat mit `Retry-After` und Rate-Limit-Headern). Die `Admission` wandert in den
//! [`Recorder`](crate::observe::Recorder) und wird an dessen Ende – nach dem letzten Byte bzw.
//! beim Abbruch – mit der tatsächlichen Usage abgeschlossen ([`Admission::complete`]):
//! Kosten aus der Preistabelle in `RequestRecord.cost`, Budgets buchen, Reservierungen
//! abgleichen. Retries und Fallbacks laufen innerhalb dieser einen Reservierung.
//!
//! - Limits je **Key, Team, Organisation** (optional nur für eine Modellgruppe) und je
//!   **Modellgruppe global**, Metriken `requests`, `tokens`, `input-tokens`, `output-tokens`,
//!   gleitende Fenster Sekunde/Minute/Stunde/Tag/30 Tage (RPM/RPD, TPM/TPD) und
//!   Kalenderzeiträume Tag/Woche/Monat in `billing.timezone` (F25). Alle Ebenen werden mit
//!   **einem** [`CounterBackend::reserve`] atomar geprüft und gebucht.
//! - `concurrency` (S-12): gleichzeitig laufende Anfragen je Key, Team, Organisation und
//!   Modellgruppe ([`CounterBackend::acquire_slots`], atomar über alle Ebenen). Die Plätze hält
//!   die [`Admission`]; sie werden frei, sobald sie abgeschlossen oder fallen gelassen wird
//!   (letztes Byte, Abbruch, Fehler). `local`/`postgres-sync`: je Instanz, `redis`: im
//!   Cluster. Ablehnung `429` mit Code `concurrency_limit_exceeded` und `Retry-After: 1`.
//! - Token-Reservierung = geschätzte Prompt-Tokens + `max_tokens` (ohne Angabe:
//!   `limits.default_max_tokens`); abgeglichen wird mit der gemeldeten (bzw. bei Abbruch
//!   geschätzten) Usage. Die Prompt-Tokens zählt der Tokenizer der Modellgruppe
//!   ([`aigw_tokenizer`], F46: o200k/cl100k exakt, `tokenizer.json`, Heuristik für
//!   Claude/Gemini), siehe [`Limiter::estimate`].
//! - Budgets mit `enforcement: strict` (Standard) reservieren die geschätzten Kosten (Schätzung
//!   × Preis des teuersten Deployments der Gruppe) vor dem Upstream-Aufruf und gleichen sie
//!   nach der Usage ab (F106); mit `counters.backend: redis` exakt im Cluster
//!   ([`RedisBudgets`]), sonst im Abgleich-Takt des Budget-Ledgers.
//! - Die Deployment-Kapazität (Routing) nutzt dieselben Zähler-Backends, siehe
//!   [`crate::routing`].
//!
//! Ausführlich: `docs/konzept/11-limits-und-budgets.md`.

mod headers;
mod plan;
mod redis_budget;
mod tokens;

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aigw_billing::{BudgetExceeded, BudgetHold, BudgetLevel, BudgetTarget, Budgets, ExceededKind};
use aigw_core::config::LimitsConfig;
use aigw_core::ir::{ApiError, ChatRequest, ErrorClass, ErrorKind, UpstreamError, Usage};
use aigw_core::observe::{PluginCall, RequestRecord};
use aigw_core::resource::Amount;
use aigw_core::snapshot::{KeyCtx, ModelGroupCtx, Snapshot};
use aigw_counters::{
    CounterBackend, LimitCheck, LocalCounters, Rejection, Reservation, SlotCheck, SlotLease,
    SlotRejection,
};
use aigw_tokenizer::Tokenizer;
use time::OffsetDateTime;

use crate::auth::AuthContext;
use crate::observe::{estimate_tokens, request_bytes};

pub(crate) use headers::RateHeaders;
pub use plan::Metric;
use plan::{Periods, Planned, PlannedSlot, plan, plan_slots};
pub use redis_budget::RedisBudgets;
use tokens::Tokenizers;
pub(crate) use tokens::{output_tokens, request_tokens};

/// Geschätzter Token-Bedarf einer Anfrage (Grundlage der Reservierungen).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenEstimate {
    /// Geschätzte Eingabe-Tokens (Textumfang / 4).
    pub input: u64,
    /// Höchstens erwartete Ausgabe-Tokens (`max_tokens` bzw. Standard).
    pub output: u64,
}

impl TokenEstimate {
    /// Grobe Schätzung für `req` (≈ 4 Bytes je Token, ohne Tokenizer); ohne `max_tokens` gilt
    /// `default_max_tokens`. Bei `n > 1` (F12) zählt die Ausgabe je Variante. Im Request-Pfad
    /// gilt [`Limiter::estimate`].
    pub fn of(req: &ChatRequest, default_max_tokens: u64) -> Self {
        let n = u64::from(req.choices());
        Self {
            input: estimate_tokens(request_bytes(req)),
            output: req
                .max_tokens
                .unwrap_or(default_max_tokens)
                .saturating_mul(n),
        }
    }

    /// Ein- plus Ausgabe.
    pub fn total(self) -> u64 {
        self.input.saturating_add(self.output)
    }

    fn amount(self, m: Metric) -> u64 {
        match m {
            Metric::Requests => 1,
            Metric::Tokens => self.total(),
            Metric::InputTokens => self.input,
            Metric::OutputTokens => self.output,
        }
    }
}

/// Limits und Budgets der Datenebene.
pub struct Limiter {
    counters: Arc<dyn CounterBackend>,
    budgets: Option<Arc<Budgets>>,
    default_max_tokens: u64,
    response_headers: bool,
    tokenizers: Tokenizers,
    /// Kalenderzeiträume der Limits (F25, `billing.timezone`).
    periods: Periods,
}

impl fmt::Debug for Limiter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Limiter")
            .field("counters", &self.counters.kind())
            .field("budgets", &self.budgets.is_some())
            .field("default_max_tokens", &self.default_max_tokens)
            .field("response_headers", &self.response_headers)
            .finish()
    }
}

impl Default for Limiter {
    fn default() -> Self {
        Self::local()
    }
}

impl Limiter {
    /// Limiter mit Zähler-Backend, optionalen Budgets und Einstellungen aus `limits`.
    pub fn new(
        counters: Arc<dyn CounterBackend>,
        budgets: Option<Arc<Budgets>>,
        cfg: &LimitsConfig,
    ) -> Self {
        Tokenizers::warm_up();
        // Kalenderzeiträume wie die Budgets (gleiche Zeitzone), ohne Budgets UTC.
        let periods = Periods::new(
            budgets
                .as_ref()
                .map_or_else(aigw_billing::Calendar::utc, |b| b.calendar().clone()),
        );
        Self {
            counters,
            budgets,
            default_max_tokens: cfg.default_max_tokens,
            response_headers: cfg.response_headers,
            tokenizers: Tokenizers::new(cfg.tokenizer_dir.as_ref().map(std::path::PathBuf::from)),
            periods,
        }
    }

    /// Kalender der Limit-Zeiträume `calendar-day|week|month` (Standard: der der Budgets,
    /// sonst UTC).
    #[must_use]
    pub fn with_calendar(mut self, calendar: aigw_billing::Calendar) -> Self {
        self.periods = Periods::new(calendar);
        self
    }

    /// Zeitzone der Kalenderzeiträume.
    pub fn timezone(&self) -> &str {
        self.periods.calendar().timezone()
    }

    /// Lokale Zähler (ohne Bereinigungs-Task), ohne Budgets, Standardeinstellungen – für
    /// Tests und [`ProxyState::with_routes`](crate::ProxyState::with_routes).
    pub fn local() -> Self {
        Self::new(
            Arc::new(LocalCounters::new()),
            None,
            &LimitsConfig::default(),
        )
    }

    /// Zähler-Backend (auch für die Deployment-Kapazität).
    pub fn counters(&self) -> &Arc<dyn CounterBackend> {
        &self.counters
    }

    /// Budgets (falls aktiv).
    pub fn budgets(&self) -> Option<&Arc<Budgets>> {
        self.budgets.as_ref()
    }

    /// Ausgabe-Tokens für Reservierungen ohne `max_tokens`.
    pub fn default_max_tokens(&self) -> u64 {
        self.default_max_tokens
    }

    /// Rate-Limit-Header auch bei Erfolg setzen?
    pub fn response_headers(&self) -> bool {
        self.response_headers
    }

    /// Tokenizer einer Modellgruppe (`settings.tokenizer`, sonst automatisch; ohne Gruppe:
    /// Heuristik).
    pub fn tokenizer(&self, group: Option<&ModelGroupCtx>) -> Tokenizer {
        self.tokenizers.for_group(group)
    }

    /// Geschätzter Token-Bedarf einer Chat-Anfrage mit dem Tokenizer der Modellgruppe
    /// (Eingabe) und `max_tokens` bzw. `limits.default_max_tokens` (Ausgabe).
    pub fn estimate(&self, req: &ChatRequest, group: Option<&Arc<ModelGroupCtx>>) -> TokenEstimate {
        let tok = self.tokenizer(group.map(Arc::as_ref));
        // Bei `n > 1` (F12) zählt die Ausgabe je Variante.
        TokenEstimate {
            input: request_tokens(&tok, req),
            output: req
                .max_tokens
                .unwrap_or(self.default_max_tokens)
                .saturating_mul(u64::from(req.choices())),
        }
    }

    /// Prüft Budgets und reserviert alle Limits einer Anfrage (einmal je Client-Anfrage).
    ///
    /// `group` ist die freigegebene Modellgruppe (ohne Snapshot `None`: dann gelten nur
    /// Limits ohne Modellbezug). Ohne Key (z. B. [`AllowAll`](crate::AllowAll)) gibt es nur
    /// die Limits der Modellgruppe.
    pub(crate) async fn admit(
        &self,
        auth: &AuthContext,
        group: Option<&Arc<ModelGroupCtx>>,
        est: TokenEstimate,
        now: SystemTime,
    ) -> Result<Admission, Box<Denied>> {
        let now_ms = unix_ms(now);
        let key = auth.key.as_ref();

        // Gleichzeitigkeit zuerst (S-12): lehnt danach Budget oder Limit ab, gibt das
        // Verwerfen der Lease die Plätze sofort frei (Fenster-Zähler blieben gebucht).
        let mut slot_plan: Vec<PlannedSlot> = Vec::new();
        plan_slots(key.map(Arc::as_ref), group.map(Arc::as_ref), &mut slot_plan);
        let slots = if slot_plan.is_empty() {
            None
        } else {
            let checks: Vec<SlotCheck<'_>> = slot_plan
                .iter()
                .map(|p| SlotCheck::new(&p.key, p.limit))
                .collect();
            match self.counters.acquire_slots(&checks).await {
                Ok(l) => Some(l),
                Err(rej) => return Err(Box::new(Denied::concurrency(&slot_plan, &rej))),
            }
        };

        // Budgets: harte Grenze vor dem Upstream-Aufruf; `strict` reserviert die geschätzten
        // Kosten (F106). Lehnt danach ein Limit ab, gibt das Verwerfen des Holds sie frei.
        let mut budget_hold = None;
        if let (Some(budgets), Some(k)) = (&self.budgets, key) {
            let targets = budget_targets(k);
            if targets.iter().any(|t| t.budget.hard_limit) {
                let cost =
                    estimate_cost(auth.snapshot.as_deref(), group.map(Arc::as_ref), est, now);
                match budgets.reserve(&targets, cost, now_ms).await {
                    Ok(h) => budget_hold = (!h.is_empty()).then_some(h),
                    Err(e) => return Err(Box::new(Denied::budget(&e, now_ms))),
                }
            }
        }

        // Limits aller Ebenen in einem Aufruf.
        let mut planned: Vec<Planned> = Vec::new();
        plan(
            key.map(Arc::as_ref),
            group.map(Arc::as_ref),
            &self.periods,
            u64::try_from(now_ms).unwrap_or(0),
            &mut planned,
        );
        let mut parts = Parts::default();
        let mut quotas = RateHeaders::default();
        if !planned.is_empty() {
            let checks: Vec<LimitCheck<'_>> = planned
                .iter()
                .map(|p| {
                    let amount = est.amount(p.metric);
                    if p.metric == Metric::Requests {
                        LimitCheck::fixed(&p.key, p.window, p.limit, amount)
                    } else {
                        LimitCheck::new(&p.key, p.window, p.limit, amount)
                    }
                })
                .collect();
            match self.counters.reserve(&checks).await {
                Ok(res) => {
                    quotas = RateHeaders::from_quotas(&planned, res.quotas());
                    parts = Parts::split(res, &planned);
                }
                Err(rej) => return Err(Box::new(Denied::limit(&planned, &rej))),
            }
        }
        Ok(Admission {
            counters: Arc::clone(&self.counters),
            budgets: self.budgets.clone(),
            key: key.cloned(),
            snapshot: auth.snapshot.clone(),
            parts,
            headers: quotas,
            budget_hold,
            estimate: est,
            tokenizer: self.tokenizer(group.map(Arc::as_ref)),
            slots,
            input_plan: planned
                .into_iter()
                .filter(|p| matches!(p.metric, Metric::Tokens | Metric::InputTokens))
                .collect(),
            group: group.cloned(),
            growth: None,
        })
    }
}

/// Geschätzte Kosten einer Anfrage für die Budget-Reservierung: Schätzung × Preis, beim
/// teuersten aktiven Deployment der Gruppe (Routing/Fallbacks innerhalb der Gruppe sind damit
/// abgedeckt). Ohne Gruppe, Snapshot oder Preis: 0 (keine Reservierung, nur Prüfung).
fn estimate_cost(
    snap: Option<&Snapshot>,
    group: Option<&ModelGroupCtx>,
    est: TokenEstimate,
    now: SystemTime,
) -> Amount {
    let (Some(snap), Some(group)) = (snap, group) else {
        return Amount::ZERO;
    };
    let usage = Usage {
        input_tokens: est.input,
        output_tokens: est.output,
        ..Usage::default()
    };
    let at = OffsetDateTime::from(now);
    group
        .deployments
        .iter()
        .filter_map(|d| {
            let s = &d.deployment.spec;
            aigw_billing::cost_at(snap, s.provider, &s.provider_model, at, &usage)
        })
        .max()
        .unwrap_or(Amount::ZERO)
}

pub(crate) fn unix_ms(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Budgets eines Keys: Key, Team, Organisation (in dieser Reihenfolge).
pub(crate) fn budget_targets(k: &KeyCtx) -> Vec<BudgetTarget<'_>> {
    let mut out = Vec::with_capacity(3);
    if let Some(b) = &k.key.spec.budget {
        out.push(BudgetTarget {
            level: BudgetLevel::ApiKey,
            target: k.key.id,
            budget: b,
        });
    }
    if let Some(t) = &k.team
        && let Some(b) = &t.spec.budget
    {
        out.push(BudgetTarget {
            level: BudgetLevel::Team,
            target: t.id,
            budget: b,
        });
    }
    if let Some(o) = &k.organization
        && let Some(b) = &o.spec.budget
    {
        out.push(BudgetTarget {
            level: BudgetLevel::Organization,
            target: o.id,
            budget: b,
        });
    }
    out
}

/// Reservierung, aufgeteilt nach Metrik (unterschiedlicher tatsächlicher Verbrauch).
#[derive(Debug, Default)]
struct Parts {
    tokens: Option<Reservation>,
    input: Option<Reservation>,
    output: Option<Reservation>,
}

impl Parts {
    /// `planned` ist nach Metrik sortiert (Anfragen, Tokens, Eingabe, Ausgabe).
    fn split(mut res: Reservation, planned: &[Planned]) -> Self {
        let count = |m: Metric| planned.iter().filter(|p| p.metric == m).count();
        let req = count(Metric::Requests);
        let tok = count(Metric::Tokens);
        let inp = count(Metric::InputTokens);
        let mut tokens = res.split_off(req);
        let mut input = tokens.split_off(tok);
        let output = input.split_off(inp);
        // Anfragezähler werden nie korrigiert ([`SettleMode::Keep`]) – kein Abgleich nötig.
        drop(res);
        let some = |r: Reservation| (!r.is_empty()).then_some(r);
        Self {
            tokens: some(tokens),
            input: some(input),
            output: some(output),
        }
    }

    fn is_empty(&self) -> bool {
        self.tokens.is_none() && self.input.is_none() && self.output.is_none()
    }
}

/// Zugelassene Anfrage: hält die Reservierungen bis zum Ende der Anfrage.
///
/// Wird sie ohne [`Admission::complete`] verworfen, bleiben die Reservierungen voll gebucht
/// (konservativ) und es wird nichts abgerechnet. Gleichzeitigkeits-Plätze werden in beiden
/// Fällen freigegeben.
pub struct Admission {
    counters: Arc<dyn CounterBackend>,
    budgets: Option<Arc<Budgets>>,
    key: Option<Arc<KeyCtx>>,
    snapshot: Option<Arc<Snapshot>>,
    parts: Parts,
    headers: RateHeaders,
    /// Reservierte geschätzte Kosten (strikte Budgets, F106).
    budget_hold: Option<BudgetHold>,
    /// Schätzung, mit der reserviert wurde.
    estimate: TokenEstimate,
    /// Tokenizer der Modellgruppe (Usage-Schätzung bei Abbruch/fehlender Usage).
    tokenizer: Tokenizer,
    /// Belegte Gleichzeitigkeits-Plätze (S-12); frei mit dem Ende der `Admission`.
    slots: Option<SlotLease>,
    /// Token-/Eingabe-Limits der Admission (für die Nachreservierung, S-38).
    input_plan: Vec<Planned>,
    /// Modellgruppe (Preis der Nachreservierung, S-38).
    group: Option<Arc<ModelGroupCtx>>,
    /// Nachreservierung, weil Pre-Plugins die Eingabe vergrößert haben (S-38).
    growth: Option<Growth>,
}

/// Nachreservierte Eingabe-Tokens (S-38): getrennte Reservierung, beim Abgleich zählt sie
/// genau `delta` (die ursprüngliche Reservierung wird mit `tatsächlich − delta` abgeglichen).
#[derive(Debug)]
struct Growth {
    delta: u64,
    parts: Parts,
    budget_hold: Option<BudgetHold>,
}

impl fmt::Debug for Admission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Admission")
            .field("parts", &self.parts)
            .field("headers", &self.headers)
            .field("slots", &self.slots)
            .finish_non_exhaustive()
    }
}

impl Admission {
    /// Stand der knappsten Limits je Metrik (für die Antwort-Header).
    pub(crate) fn headers(&self) -> &RateHeaders {
        &self.headers
    }

    /// Geschätzte Eingabe-Tokens (Grundlage der Reservierung).
    pub(crate) fn input_estimate(&self) -> u64 {
        self.estimate.input
    }

    /// Tokenizer der Modellgruppe.
    pub(crate) fn tokenizer(&self) -> &Tokenizer {
        &self.tokenizer
    }

    /// S-38: Pre-Plugins (z. B. `request-transform`, Webhook `modify`, WASM) dürfen den Prompt
    /// vergrößern. Die Admission hat aber nur die Eingabe der Client-Anfrage reserviert.
    /// Liegt die neue Schätzung `input` darüber, wird die Differenz auf allen Token- und
    /// Eingabe-Limits sowie (bei harten Budgets) als Kosten nachreserviert – alle oder keine.
    /// Reicht ein Limit oder Budget nicht, wird die Anfrage abgelehnt wie bei der Admission.
    /// Kleinere oder gleiche Eingaben ändern nichts (der Abgleich korrigiert ohnehin).
    pub(crate) async fn grow_input(
        &mut self,
        input: u64,
        now: SystemTime,
    ) -> Result<(), Box<Denied>> {
        let base = self.estimate.input;
        if input <= base || self.growth.is_some() {
            return Ok(());
        }
        let delta = input - base;
        let now_ms = unix_ms(now);
        let mut budget_hold = None;
        if let (Some(budgets), Some(k)) = (&self.budgets, &self.key) {
            let targets = budget_targets(k);
            if targets.iter().any(|t| t.budget.hard_limit) {
                let est = TokenEstimate {
                    input: delta,
                    output: 0,
                };
                let cost = estimate_cost(self.snapshot.as_deref(), self.group.as_deref(), est, now);
                match budgets.reserve(&targets, cost, now_ms).await {
                    Ok(h) => budget_hold = (!h.is_empty()).then_some(h),
                    Err(e) => return Err(Box::new(Denied::budget(&e, now_ms))),
                }
            }
        }
        let mut parts = Parts::default();
        if !self.input_plan.is_empty() {
            let checks: Vec<LimitCheck<'_>> = self
                .input_plan
                .iter()
                .map(|p| LimitCheck::new(&p.key, p.window, p.limit, delta))
                .collect();
            match self.counters.reserve(&checks).await {
                Ok(res) => parts = Parts::split(res, &self.input_plan),
                Err(rej) => return Err(Box::new(Denied::limit(&self.input_plan, &rej))),
            }
        }
        tracing::debug!(
            admitted = base,
            input,
            "Eingabe durch Plugins gewachsen – nachreserviert (S-38)"
        );
        self.estimate.input = input;
        self.growth = Some(Growth {
            delta,
            parts,
            budget_hold,
        });
        Ok(())
    }

    /// Schließt die Anfrage ab (synchron, ohne I/O im Aufrufer): Kosten aus der Preistabelle
    /// zum Anfragezeitpunkt in `record.cost`, Kosten auf die Budgets buchen, Reservierungen
    /// mit der tatsächlichen Usage abgleichen (bei entfernten Backends im Hintergrund).
    ///
    /// Modellaufrufe von Plugins (`plugin_calls`, z. B. Prüfmodell von `content-safety`;
    /// AIGW-89/S-13) zählen mit: ihre Kosten (Preis des antwortenden Deployments) landen in
    /// `record.cost` (Anteil in `record.plugin_cost`) und auf den Budgets, ihre Tokens auf den
    /// Token-Limits des Keys/Teams.
    pub(crate) fn complete(mut self, record: &mut RequestRecord, plugin_calls: &[PluginCall]) {
        // Die Anfrage ist zu Ende: Plätze sofort frei.
        drop(self.slots.take());
        let mut cost = self.price(record);
        let plugin_cost = self.plugin_price(record, plugin_calls);
        if let Some(p) = plugin_cost {
            record.plugin_cost = Some(p);
            cost = Some(cost.unwrap_or(Amount::ZERO).saturating_add(p));
        }
        if let Some(c) = cost {
            record.cost = Some(c);
        }
        if let (Some(budgets), Some(k)) = (&self.budgets, &self.key) {
            let targets = budget_targets(k);
            let at = unix_ms(record.started_at);
            let c = cost.unwrap_or(Amount::ZERO);
            // Nachreservierung (S-38) freigeben, die tatsächlichen Kosten bucht die Hauptbuchung.
            if let Some(hold) = self.growth.as_mut().and_then(|g| g.budget_hold.take()) {
                budgets.settle(hold, &targets, Amount::ZERO, at);
            }
            match self.budget_hold.take() {
                // Reservierung gegen die tatsächlichen Kosten tauschen (auch 0 = Freigabe).
                Some(hold) => budgets.settle(hold, &targets, c, at),
                None => budgets.charge(&targets, c, at),
            }
        }
        let mut usage = record.usage.clone();
        for c in plugin_calls {
            usage.add(&c.usage);
        }
        match self.growth.take() {
            // S-38: Nachreservierung zählt genau `delta` (höchstens die tatsächliche Eingabe),
            // die Hauptreservierung den Rest – zusammen der tatsächliche Verbrauch.
            Some(g) => {
                let d = g.delta.min(usage.input_tokens);
                let mut main = usage.clone();
                main.input_tokens -= d;
                let grown = Usage {
                    input_tokens: d,
                    ..Usage::default()
                };
                settle(Arc::clone(&self.counters), self.parts, &main);
                settle(self.counters, g.parts, &grown);
            }
            None => settle(self.counters, self.parts, &usage),
        }
    }

    /// Summe der Kosten der Plugin-Modellaufrufe (`None` ohne Preis für alle).
    fn plugin_price(&self, r: &RequestRecord, calls: &[PluginCall]) -> Option<Amount> {
        let snap = self.snapshot.as_ref()?;
        let at = OffsetDateTime::from(r.started_at);
        calls
            .iter()
            .filter_map(|c| {
                aigw_billing::cost_at(
                    snap,
                    c.provider?,
                    c.provider_model.as_deref()?,
                    at,
                    &c.usage,
                )
            })
            .reduce(Amount::saturating_add)
    }

    fn price(&self, r: &RequestRecord) -> Option<Amount> {
        let snap = self.snapshot.as_ref()?;
        let (provider, model) = (r.provider?, r.provider_model.as_deref()?);
        // Fehlgeschlagene Anfragen ohne Verbrauch kosten nichts (auch kein Preis je Anfrage).
        if r.usage.total_tokens() == 0 && r.is_error() {
            return None;
        }
        aigw_billing::cost_at(
            snap,
            provider,
            model,
            OffsetDateTime::from(r.started_at),
            &r.usage,
        )
    }
}

/// Gleicht die Teile einer Reservierung ab. `local`/`postgres-sync` buchen sofort im RAM;
/// trotzdem läuft der Abgleich als Task, damit der Aufrufer (auch `Drop`) nie wartet.
fn settle(counters: Arc<dyn CounterBackend>, parts: Parts, usage: &Usage) {
    if parts.is_empty() {
        return;
    }
    let actual = (
        usage.total_tokens(),
        usage.input_tokens,
        usage.output_tokens,
    );
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        // Ohne Laufzeit (nur beim Herunterfahren denkbar) bleibt die Reservierung gebucht.
        return;
    };
    rt.spawn(async move {
        let Parts {
            tokens,
            input,
            output,
        } = parts;
        for (part, actual) in [(tokens, actual.0), (input, actual.1), (output, actual.2)] {
            if let Some(r) = part {
                counters.settle(r, actual).await;
            }
        }
    });
}

/// Ablehnung durch Limits oder Budget.
#[derive(Debug)]
pub(crate) struct Denied {
    /// Fehler (`429`) samt `Retry-After`.
    pub(crate) error: UpstreamError,
    /// Rate-Limit-Header (leer bei Budget-Ablehnung).
    pub(crate) headers: RateHeaders,
}

impl Denied {
    fn limit(planned: &[Planned], rej: &Rejection) -> Self {
        let p = planned.get(rej.index);
        let what = p.map_or("Anfragen", |p| p.metric.label());
        let level = p.map_or("", |p| p.level.label());
        let window = p.map_or("Fenster", |p| plan::window_label(p.spec_window));
        let msg = if rej.retry_after.is_some() {
            format!(
                "Rate-Limit erreicht: höchstens {} {what} je {window} ({level})",
                rej.limit
            )
        } else {
            format!(
                "Anfrage zu groß für das Limit von {} {what} je {window} ({level}); \
                 max_tokens verringern oder Limit erhöhen",
                rej.limit
            )
        };
        let mut err = UpstreamError::new(
            ErrorClass::RateLimit,
            ApiError::new(ErrorKind::RateLimit, msg).with_code("rate_limit_exceeded"),
        )
        .with_retry_after(rej.retry_after);
        err.retryable = false;
        Self {
            error: err,
            headers: RateHeaders::from_rejection(planned, rej),
        }
    }

    fn concurrency(planned: &[PlannedSlot], rej: &SlotRejection) -> Self {
        let level = planned.get(rej.index).map_or("", |p| p.level.label());
        let msg = format!(
            "Zu viele gleichzeitige Anfragen: höchstens {} gleichzeitig ({level}); laufende \
             Anfragen abwarten und erneut versuchen",
            rej.limit
        );
        let mut err = UpstreamError::new(
            ErrorClass::RateLimit,
            ApiError::new(ErrorKind::RateLimit, msg).with_code("concurrency_limit_exceeded"),
        )
        .with_retry_after(Some(Duration::from_secs(1)));
        err.retryable = false;
        Self {
            error: err,
            headers: RateHeaders::default(),
        }
    }

    fn budget(e: &BudgetExceeded, now_ms: i64) -> Self {
        let level = match e.level {
            BudgetLevel::ApiKey => "API-Key",
            BudgetLevel::Team => "Team",
            BudgetLevel::Organization => "Organisation",
        };
        let period = aigw_billing::period_code(e.period);
        let msg = match e.kind {
            ExceededKind::Exhausted => format!(
                "Budget erschöpft ({level}, {period}): {} von {} verbraucht",
                e.used, e.amount
            ),
            ExceededKind::Reserved => format!(
                "Budget ausgelastet ({level}, {period}): {} verbraucht und {} für laufende \
                 Anfragen reserviert, diese Anfrage braucht geschätzt {} (Betrag {}); später \
                 erneut versuchen",
                e.used, e.reserved, e.requested, e.amount
            ),
            ExceededKind::Estimate => format!(
                "Budget reicht nicht ({level}, {period}): geschätzte Kosten {} übersteigen den \
                 Rest von {} ({} von {} verbraucht); max_tokens verringern",
                e.requested,
                Amount::from_nanos(e.amount.nanos().saturating_sub(e.used.nanos()).max(0)),
                e.used,
                e.amount
            ),
        };
        let mut err = UpstreamError::new(
            ErrorClass::Quota,
            ApiError::new(ErrorKind::RateLimit, msg).with_code("budget_exceeded"),
        )
        .with_retry_after(e.retry_after(now_ms));
        err.retryable = false;
        Self {
            error: err,
            headers: RateHeaders::default(),
        }
    }
}

/// Rundet eine Wartezeit für `Retry-After` auf ganze Sekunden auf.
pub(crate) fn retry_after_secs(d: Duration) -> u64 {
    d.as_secs() + u64::from(d.subsec_nanos() > 0)
}

#[cfg(test)]
mod tests;
