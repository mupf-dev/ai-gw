// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Routing und Resilienz (Architektur §6; AIGW-24, AIGW-25, AIGW-26).
//!
//! - [`GroupRoute`] – eine Modellgruppe mit ihren Deployments ([`Member`]), nach Priorität in
//!   Gruppen geordnet (0 vor 1), Strategie, [`GroupPolicy`] und Fallback-Kette.
//! - Auswahl ([`select`]): `weighted`, `least-latency`, `least-busy`, `cost`. Eine
//!   Prioritätsgruppe ist gesperrt, solange alle ihre Deployments offen (Circuit Breaker), nach
//!   `Retry-After` gesperrt oder an der Kapazitätsgrenze (RPM/TPM, gezählt über
//!   `aigw-counters`) sind.
//! - Ausführung (`exec`): Retries vor dem ersten Byte an den Client, anderes Deployment
//!   bevorzugt, exponentieller Backoff mit Jitter, `Retry-After`, Gesamtbudget, Fallback-Ketten
//!   über Modellgruppen nach Fehlerklasse.
//! - [`DeploymentState`] – EWMA, offene Anfragen, Breaker, Zähler; je Deployment-ID, überlebt
//!   Snapshot-Wechsel.
//! - [`SnapshotRoutes`] – baut die Routing-Tabelle aus dem Konfigurations-Snapshot und bei
//!   jeder neuen `config_version` neu; führt optionale aktive Health Checks aus.
//!
//! Ausführliche Beschreibung: `docs/konzept/10-routing.md`.

mod breaker;
pub(crate) mod exec;
mod health;
mod policy;
mod select;
mod snapshot;
mod state;

use std::fmt;
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use aigw_core::resource::{Capacity, Fallback, RoutingStrategy};

use crate::provider::ChatProvider;

pub use breaker::BreakerState;
pub use policy::{BreakerConfig, GroupPolicy, HealthCheckConfig, RetryPolicy};
pub use snapshot::{RouteTable, RoutesTask, SnapshotRoutes};
pub use state::DeploymentState;

use breaker::Admit;

/// Ein Deployment innerhalb einer [`GroupRoute`].
pub struct Member {
    pub(crate) provider: Arc<dyn ChatProvider>,
    pub(crate) state: Arc<DeploymentState>,
    pub(crate) weight: u32,
    pub(crate) priority: u32,
    pub(crate) capacity: Capacity,
    /// Zählerschlüssel der Kapazität (`dep:<id>:requests`, `dep:<id>:tokens`).
    pub(crate) capacity_keys: (Box<str>, Box<str>),
    /// Preis (Eingabe + Ausgabe je 1 Mio. Tokens, Nano-Einheiten) für `cost`.
    pub(crate) cost: Option<u64>,
    /// Position in [`GroupRoute::members`].
    pub(crate) idx: usize,
}

impl fmt::Debug for Member {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Member")
            .field("deployment", &self.provider.describe().deployment)
            .field("weight", &self.weight)
            .field("priority", &self.priority)
            .field("capacity", &self.capacity)
            .field("cost", &self.cost)
            .field("state", &self.state)
            .finish()
    }
}

impl Member {
    /// Ausführbares Deployment.
    pub fn provider(&self) -> &Arc<dyn ChatProvider> {
        &self.provider
    }

    /// Laufzeitzustand.
    pub fn state(&self) -> &Arc<DeploymentState> {
        &self.state
    }

    /// Kann jetzt angefragt werden (Breaker, `Retry-After`-Sperre, Kapazität)?
    fn available(&self, breaker: &BreakerConfig, now_ms: u64) -> bool {
        (!breaker.enabled || self.state.breaker.available(now_ms))
            && self.state.cooldown_remaining_ms(now_ms) == 0
            && !self.at_capacity(now_ms)
    }

    /// Kapazität laut letzter Ablehnung des Zählers erschöpft (nur Atomics, kein Zählerzugriff;
    /// geprüft wird beim Start eines Versuchs, siehe `exec`).
    fn at_capacity(&self, now_ms: u64) -> bool {
        self.state.capacity_remaining_ms(now_ms) > 0
    }

    /// Wie lange das Deployment noch gesperrt ist (0 = verfügbar).
    fn blocked_ms(&self, breaker: &BreakerConfig, now_ms: u64) -> u64 {
        let b = if breaker.enabled {
            self.state.breaker.remaining_ms(now_ms)
        } else {
            0
        };
        let c = self.state.cooldown_remaining_ms(now_ms);
        let cap = self.state.capacity_remaining_ms(now_ms);
        b.max(c).max(cap)
    }
}

/// Beschreibung eines Deployments für [`GroupRoute::new`].
pub struct MemberSpec {
    /// Ausführbares Deployment.
    pub provider: Arc<dyn ChatProvider>,
    /// Laufzeitzustand (geteilt über Snapshot-Wechsel); `None` = neuer Zustand.
    pub state: Option<Arc<DeploymentState>>,
    /// Gewicht für `weighted`.
    pub weight: u32,
    /// Prioritätsgruppe (0 vor 1).
    pub priority: u32,
    /// Kapazitätsgrenzen.
    pub capacity: Capacity,
    /// Preis für `cost` (kleiner = günstiger).
    pub cost: Option<u64>,
}

impl fmt::Debug for MemberSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemberSpec")
            .field("deployment", &self.provider.describe().deployment)
            .field("weight", &self.weight)
            .field("priority", &self.priority)
            .finish_non_exhaustive()
    }
}

impl MemberSpec {
    /// Deployment mit Gewicht 1, Priorität 0, ohne Kapazitätsgrenze und Preis.
    pub fn new(provider: Arc<dyn ChatProvider>) -> Self {
        Self {
            provider,
            state: None,
            weight: 1,
            priority: 0,
            capacity: Capacity::default(),
            cost: None,
        }
    }
}

/// Route einer Modellgruppe.
pub struct GroupRoute {
    name: String,
    strategy: RoutingStrategy,
    policy: GroupPolicy,
    /// Nach Priorität sortiert.
    members: Vec<Arc<Member>>,
    /// Bereiche gleicher Priorität in `members`.
    tiers: Vec<Range<usize>>,
    fallbacks: Vec<Fallback>,
}

impl fmt::Debug for GroupRoute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GroupRoute")
            .field("name", &self.name)
            .field("strategy", &self.strategy)
            .field("policy", &self.policy)
            .field("members", &self.members)
            .field("fallbacks", &self.fallbacks)
            .finish()
    }
}

/// Wiederverwendbare Puffer der Auswahl (eine Allokation je Anfrage statt je Versuch).
#[derive(Debug, Default)]
pub(crate) struct Scratch {
    cands: Vec<usize>,
    near: Vec<usize>,
}

impl GroupRoute {
    /// Neue Route.
    pub fn new(
        name: impl Into<String>,
        strategy: RoutingStrategy,
        policy: GroupPolicy,
        fallbacks: Vec<Fallback>,
        mut members: Vec<MemberSpec>,
    ) -> Self {
        members.sort_by_key(|m| m.priority);
        let members: Vec<Arc<Member>> = members
            .into_iter()
            .enumerate()
            .map(|(idx, m)| {
                let d = m.provider.describe();
                let id = d
                    .deployment_id
                    .map_or_else(|| d.deployment.to_owned(), |id| id.to_string());
                let capacity_keys = (
                    format!("dep:{id}:requests").into_boxed_str(),
                    format!("dep:{id}:tokens").into_boxed_str(),
                );
                Arc::new(Member {
                    provider: m.provider,
                    state: m.state.unwrap_or_default(),
                    weight: m.weight,
                    priority: m.priority,
                    capacity: m.capacity,
                    capacity_keys,
                    cost: m.cost,
                    idx,
                })
            })
            .collect();
        let mut tiers = Vec::new();
        let mut start = 0;
        for i in 1..=members.len() {
            if i == members.len() || members[i].priority != members[start].priority {
                tiers.push(start..i);
                start = i;
            }
        }
        Self {
            name: name.into(),
            strategy,
            policy,
            members,
            tiers,
            fallbacks,
        }
    }

    /// Route mit genau einem Deployment, ohne Retries, Breaker und Fallbacks.
    pub fn single(name: impl Into<String>, provider: Arc<dyn ChatProvider>) -> Self {
        Self::new(
            name,
            RoutingStrategy::Weighted,
            GroupPolicy::passthrough(),
            Vec::new(),
            vec![MemberSpec::new(provider)],
        )
    }

    /// Name der Modellgruppe.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Strategie.
    pub fn strategy(&self) -> RoutingStrategy {
        self.strategy
    }

    /// Einstellungen.
    pub fn policy(&self) -> &GroupPolicy {
        &self.policy
    }

    /// Deployments (nach Priorität sortiert).
    pub fn members(&self) -> &[Arc<Member>] {
        &self.members
    }

    /// Fallback-Kette.
    pub fn fallbacks(&self) -> &[Fallback] {
        &self.fallbacks
    }

    /// Wählt ein Deployment: Prioritätsgruppen in Reihenfolge, zuerst nur noch nicht
    /// versuchte Deployments (`tried`), dann – falls keines verfügbar – auch schon versuchte.
    /// `excluded` wird nie gewählt (nicht wiederholbare, deployment-bezogene Fehler).
    pub(crate) fn pick(
        &self,
        tried: &[usize],
        excluded: &[usize],
        stream: bool,
        now_ms: u64,
        scratch: &mut Scratch,
    ) -> Option<(Arc<Member>, Admit)> {
        let breaker = &self.policy.breaker;
        for pass in 0..2 {
            if pass == 1 && tried.is_empty() {
                break;
            }
            for tier in &self.tiers {
                scratch.cands.clear();
                scratch.cands.extend(tier.clone().filter(|i| {
                    !excluded.contains(i)
                        && (pass == 1 || !tried.contains(i))
                        && self.members[*i].available(breaker, now_ms)
                }));
                while !scratch.cands.is_empty() {
                    let pos = select::choose(
                        self.strategy,
                        &self.members,
                        &scratch.cands,
                        stream,
                        &mut scratch.near,
                    );
                    let m = &self.members[scratch.cands[pos]];
                    match m.state.breaker.try_admit(breaker, now_ms) {
                        Admit::Rejected => {
                            scratch.cands.swap_remove(pos);
                        }
                        admit => return Some((Arc::clone(m), admit)),
                    }
                }
            }
        }
        None
    }

    /// Kürzeste Zeit, bis wieder ein (nicht ausgeschlossenes) Deployment verfügbar ist.
    pub(crate) fn wait_hint(&self, excluded: &[usize], now_ms: u64) -> Option<Duration> {
        self.members
            .iter()
            .filter(|m| !excluded.contains(&m.idx))
            .map(|m| m.blocked_ms(&self.policy.breaker, now_ms))
            .filter(|ms| *ms > 0)
            .min()
            .map(Duration::from_millis)
    }

    /// Kürzeste `Retry-After`-Sperre unter Deployments, die sonst verfügbar wären (Breaker zu,
    /// Kapazität frei). Auf offene Breaker wird nie gewartet.
    pub(crate) fn cooldown_hint(&self, excluded: &[usize], now_ms: u64) -> Option<Duration> {
        let breaker = &self.policy.breaker;
        self.members
            .iter()
            .filter(|m| {
                !excluded.contains(&m.idx)
                    && (!breaker.enabled || m.state.breaker.available(now_ms))
                    && !m.at_capacity(now_ms)
            })
            .map(|m| m.state.cooldown_remaining_ms(now_ms))
            .filter(|ms| *ms > 0)
            .min()
            .map(Duration::from_millis)
    }

    /// Wählt ein Deployment wie bei einer neuen Anfrage (für Benchmarks und Tests).
    pub fn select_for_bench(&self, stream: bool) -> Option<Arc<Member>> {
        let mut scratch = Scratch::default();
        self.pick(&[], &[], stream, state::now_ms(), &mut scratch)
            .map(|(m, _)| m)
    }
}
