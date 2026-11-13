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
//!   Fenster Sekunde/Minute/Stunde/Tag (RPM/RPD, TPM/TPD). Alle Ebenen werden mit **einem**
//!   [`CounterBackend::reserve`] atomar geprüft und gebucht.
//! - Token-Reservierung = geschätzte Prompt-Tokens (≈ Bytes/4) + `max_tokens` (ohne Angabe:
//!   `limits.default_max_tokens`); abgeglichen wird mit der gemeldeten (bzw. bei Abbruch
//!   geschätzten) Usage.
//! - Die Deployment-Kapazität (Routing) nutzt dieselben Zähler-Backends, siehe
//!   [`crate::routing`].
//!
//! Ausführlich: `docs/konzept/11-limits-und-budgets.md`.

mod headers;
mod plan;

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aigw_billing::{BudgetExceeded, BudgetLevel, BudgetTarget, Budgets};
use aigw_core::config::LimitsConfig;
use aigw_core::ir::{ApiError, ChatRequest, ErrorClass, ErrorKind, UpstreamError, Usage};
use aigw_core::observe::RequestRecord;
use aigw_core::snapshot::{KeyCtx, ModelGroupCtx, Snapshot};
use aigw_counters::{CounterBackend, LimitCheck, LocalCounters, Rejection, Reservation};
use time::OffsetDateTime;

use crate::auth::AuthContext;
use crate::observe::{estimate_tokens, request_bytes};

pub(crate) use headers::RateHeaders;
pub use plan::Metric;
use plan::{Planned, plan};

/// Geschätzter Token-Bedarf einer Anfrage (Grundlage der Reservierungen).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenEstimate {
    /// Geschätzte Eingabe-Tokens (Textumfang / 4).
    pub input: u64,
    /// Höchstens erwartete Ausgabe-Tokens (`max_tokens` bzw. Standard).
    pub output: u64,
}

impl TokenEstimate {
    /// Schätzung für `req`; ohne `max_tokens` gilt `default_max_tokens`.
    pub fn of(req: &ChatRequest, default_max_tokens: u64) -> Self {
        Self {
            input: estimate_tokens(request_bytes(req)),
            output: req.max_tokens.unwrap_or(default_max_tokens),
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
        Self {
            counters,
            budgets,
            default_max_tokens: cfg.default_max_tokens,
            response_headers: cfg.response_headers,
        }
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

        // Budgets: harte Grenze vor dem Upstream-Aufruf (Stand: lokal + letzter Abgleich).
        if let (Some(budgets), Some(k)) = (&self.budgets, key) {
            let targets = budget_targets(k);
            if targets.iter().any(|t| t.budget.hard_limit)
                && let Err(e) = budgets.check(&targets, now_ms).await
            {
                return Err(Box::new(Denied::budget(&e, now_ms)));
            }
        }

        // Limits aller Ebenen in einem Aufruf.
        let mut planned: Vec<Planned> = Vec::new();
        plan(key.map(Arc::as_ref), group.map(Arc::as_ref), &mut planned);
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
        })
    }
}

fn unix_ms(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Budgets eines Keys: Key, Team, Organisation (in dieser Reihenfolge).
fn budget_targets(k: &KeyCtx) -> Vec<BudgetTarget<'_>> {
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
/// (konservativ) und es wird nichts abgerechnet.
pub struct Admission {
    counters: Arc<dyn CounterBackend>,
    budgets: Option<Arc<Budgets>>,
    key: Option<Arc<KeyCtx>>,
    snapshot: Option<Arc<Snapshot>>,
    parts: Parts,
    headers: RateHeaders,
}

impl fmt::Debug for Admission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Admission")
            .field("parts", &self.parts)
            .field("headers", &self.headers)
            .finish_non_exhaustive()
    }
}

impl Admission {
    /// Stand der knappsten Limits je Metrik (für die Antwort-Header).
    pub(crate) fn headers(&self) -> &RateHeaders {
        &self.headers
    }

    /// Schließt die Anfrage ab (synchron, ohne I/O im Aufrufer): Kosten aus der Preistabelle
    /// zum Anfragezeitpunkt in `record.cost`, Kosten auf die Budgets buchen, Reservierungen
    /// mit der tatsächlichen Usage abgleichen (bei entfernten Backends im Hintergrund).
    pub(crate) fn complete(self, record: &mut RequestRecord) {
        let cost = self.price(record);
        if let Some(c) = cost {
            record.cost = Some(c);
            if let (Some(budgets), Some(k)) = (&self.budgets, &self.key) {
                budgets.charge(&budget_targets(k), c, unix_ms(record.started_at));
            }
        }
        settle(self.counters, self.parts, &record.usage);
    }

    fn price(&self, r: &RequestRecord) -> Option<aigw_core::resource::Amount> {
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
        let window = p.map_or("Fenster", |p| plan::window_label(p.window));
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

    fn budget(e: &BudgetExceeded, now_ms: i64) -> Self {
        let level = match e.level {
            BudgetLevel::ApiKey => "API-Key",
            BudgetLevel::Team => "Team",
            BudgetLevel::Organization => "Organisation",
        };
        let msg = format!(
            "Budget erschöpft ({level}, {}): {} von {} verbraucht",
            aigw_billing::period_code(e.period),
            e.used,
            e.amount
        );
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
