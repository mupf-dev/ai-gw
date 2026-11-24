// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Budget je Deployment im Routing (AIGW-126).
//!
//! - [`MemberBudget`] – Budget eines Deployments samt Zugriff auf die Budgetstände
//!   ([`Budgets`], gleiche Ledger-Tabelle `budget_usage` mit Ebene `deployment`).
//! - Auswahl: Ein Deployment, dessen Verbrauch `switchAtPercent` des Betrags erreicht hat,
//!   gilt als nicht verfügbar (wie ein offener Breaker, nur ohne Fehler). Vor jedem Versuch
//!   reserviert `strict` die geschätzten Kosten (Preis des Deployments × geschätzte Tokens);
//!   passt die Schätzung nicht mehr in den Rest, wird das Deployment für diese Anfrage
//!   übersprungen.
//! - Abrechnung: Der [`BudgetSlot`] einer Anfrage hält die Reservierung des zuletzt
//!   angefragten Deployments; `Admission::complete` tauscht sie gegen die tatsächlichen Kosten
//!   (Preisliste, Usage der Antwort). Fehlgeschlagene Versuche kosten nichts.
//! - `budget-balanced`: Schlüssel der Auswahl ist der Verbrauch in Prozent (bzw. die
//!   absoluten Kosten, wenn nicht alle Kandidaten ein Budget haben), gelesen aus dem RAM der
//!   Budgetstände (ohne Datenbankzugriff je Anfrage).

use std::fmt;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};

use aigw_billing::{BudgetHold, BudgetLevel, BudgetTarget, Budgets};
use aigw_core::ir::Usage;
use aigw_core::resource::{Amount, Budget, DeploymentBudget, Id};

use super::Member;

/// Wanduhr in ms seit Epoche (Budget-Zeiträume; das Routing selbst rechnet monoton).
pub(crate) fn wall_ms() -> i64 {
    crate::limits::unix_ms(std::time::SystemTime::now())
}

/// Budget eines Deployments im Routing.
pub struct MemberBudget {
    budgets: Arc<Budgets>,
    id: Id,
    /// Budget mit vollem Betrag (Buchung, Warnschwellen, Ereignisse).
    budget: Budget,
    /// Budget mit Betrag × `switchAtPercent` (Auswahl, Reservierung).
    switch: Budget,
    /// Preis je 1 Mio. Tokens (Eingabe, Ausgabe; Nano-Einheiten) für die Schätzung.
    price: (i64, i64),
}

impl fmt::Debug for MemberBudget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemberBudget")
            .field("id", &self.id)
            .field("budget", &self.budget)
            .field("switch_amount", &self.switch.amount)
            .finish_non_exhaustive()
    }
}

impl MemberBudget {
    /// Budget des Deployments `id`; `price` = (Eingabe, Ausgabe) je 1 Mio. Tokens laut
    /// Preisliste (für die Reservierung; ohne Preis 0 = keine Reservierung).
    pub fn new(
        budgets: Arc<Budgets>,
        id: Id,
        spec: &DeploymentBudget,
        price: (Option<Amount>, Option<Amount>),
    ) -> Self {
        let n = |a: Option<Amount>| a.map_or(0, |a| a.nanos().max(0));
        Self {
            budgets,
            id,
            budget: spec.as_budget(),
            switch: spec.switch_budget(),
            price: (n(price.0), n(price.1)),
        }
    }

    /// Ziel mit vollem Betrag (Buchung).
    pub(crate) fn target(&self) -> BudgetTarget<'_> {
        BudgetTarget {
            level: BudgetLevel::Deployment,
            target: self.id,
            budget: &self.budget,
        }
    }

    /// Ziel mit Umschalt-Betrag (Auswahl, Reservierung).
    pub(crate) fn switch_target(&self) -> BudgetTarget<'_> {
        BudgetTarget {
            level: BudgetLevel::Deployment,
            target: self.id,
            budget: &self.switch,
        }
    }

    /// Budgetstände.
    pub(crate) fn budgets(&self) -> &Arc<Budgets> {
        &self.budgets
    }

    /// Betrag (voll).
    pub fn amount(&self) -> Amount {
        self.budget.amount
    }

    /// Verbrauch im laufenden Zeitraum (RAM dieser Instanz, abgeglichen mit dem Ledger).
    pub fn used(&self, now_ms: i64) -> Amount {
        Amount::from_nanos(self.budgets.status(&self.target(), now_ms).0)
    }

    /// Umschaltgrenze erreicht? (nur Verbrauch; Reservierungen prüft `reserve`.)
    pub(crate) fn exhausted(&self, now_ms: i64) -> bool {
        let (used, _) = self.budgets.status(&self.switch_target(), now_ms);
        used >= self.switch.amount.nanos()
    }

    /// Verbrauch inkl. laufender Reservierungen dieser Instanz (Nano-Einheiten).
    pub(crate) fn busy(&self, now_ms: i64) -> i64 {
        let (used, reserved) = self.budgets.status(&self.target(), now_ms);
        used.saturating_add(reserved).max(0)
    }

    /// Verbrauch inkl. Reservierungen in Millionstel des Betrags (`budget-balanced`).
    pub(crate) fn busy_ppm(&self, now_ms: i64) -> u64 {
        let amount = i128::from(self.budget.amount.nanos().max(1));
        let v = i128::from(self.busy(now_ms)) * 1_000_000 / amount;
        u64::try_from(v).unwrap_or(u64::MAX)
    }

    /// Geschätzte Kosten eines Versuchs (Eingabe- und Ausgabe-Tokens × Preis).
    pub(crate) fn estimate(&self, input: u64, output: u64) -> Amount {
        let cost = (i128::from(input) * i128::from(self.price.0)
            + i128::from(output) * i128::from(self.price.1))
            / 1_000_000;
        Amount::from_nanos(i64::try_from(cost).unwrap_or(i64::MAX))
    }

    /// Wann der laufende Zeitraum endet (ms; `None` bei `total`).
    pub(crate) fn resets_at_ms(&self, now_ms: i64) -> Option<i64> {
        self.budgets.period(self.budget.period, now_ms).end_ms
    }
}

/// Reservierung des zuletzt angefragten Deployments einer Anfrage (bis zur Abrechnung in
/// `Admission::complete`). Ein neuer Versuch ersetzt den Eintrag; die alte Reservierung wird
/// dabei freigegeben.
#[derive(Default)]
pub(crate) struct BudgetSlot(Mutex<Option<SlotEntry>>);

struct SlotEntry {
    member: Arc<Member>,
    hold: Option<BudgetHold>,
}

impl fmt::Debug for BudgetSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BudgetSlot").finish_non_exhaustive()
    }
}

impl BudgetSlot {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<SlotEntry>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Merkt das Deployment des aktuellen Versuchs (samt Reservierung).
    pub(crate) fn set(&self, member: Arc<Member>, hold: Option<BudgetHold>) {
        let old = self.lock().replace(SlotEntry { member, hold });
        drop(old);
    }

    /// Bucht die tatsächlichen Kosten `cost` der Antwort auf das Deployment (Budget und
    /// `budget-balanced`-Zähler) und gibt die Reservierung frei. Ohne Preis aus der
    /// Preisliste des Snapshots (`cost = None`, z. B. ohne Snapshot-Anmeldung) zählt die Usage
    /// zum Preis, mit dem das Deployment reserviert.
    pub(crate) fn settle(&self, cost: Option<Amount>, usage: &Usage, at_ms: i64) {
        let Some(SlotEntry { member, hold }) = self.lock().take() else {
            return;
        };
        let c = cost
            .or_else(|| {
                member
                    .budget
                    .as_ref()
                    .map(|b| b.estimate(usage.input_tokens, usage.output_tokens))
            })
            .unwrap_or(Amount::ZERO);
        if c.nanos() > 0 {
            member
                .state
                .spent_nanos
                .fetch_add(u64::try_from(c.nanos()).unwrap_or(0), Ordering::Relaxed);
        }
        if let Some(b) = &member.budget {
            match hold {
                Some(h) => b.budgets.settle(h, &[b.target()], c, at_ms),
                None if c.nanos() > 0 => b.budgets.charge(&[b.target()], c, at_ms),
                None => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::{GroupPolicy, GroupRoute, MemberSpec};
    use aigw_billing::Calendar;
    use aigw_core::resource::{BudgetPeriod, RoutingStrategy, new_id};

    fn amount(s: &str) -> Amount {
        s.parse().unwrap()
    }

    fn spec(a: &str, switch: Option<u8>) -> DeploymentBudget {
        DeploymentBudget {
            amount: amount(a),
            period: BudgetPeriod::Total,
            warn_at_percent: Vec::new(),
            switch_at_percent: switch,
            enforcement: Default::default(),
        }
    }

    fn route(
        strategy: RoutingStrategy,
        budgets: &Arc<Budgets>,
        members: &[(&str, u32, Option<u8>)],
    ) -> GroupRoute {
        let specs = members
            .iter()
            .map(|&(a, priority, switch)| MemberSpec {
                priority,
                budget: Some(MemberBudget::new(
                    Arc::clone(budgets),
                    new_id(),
                    &spec(a, switch),
                    (None, None),
                )),
                ..MemberSpec::new(Arc::new(crate::mock::MockProvider::new()))
            })
            .collect();
        GroupRoute::new("g", strategy, GroupPolicy::passthrough(), Vec::new(), specs)
    }

    /// Wählt `n`-mal und bucht je Anfrage `cost` auf das gewählte Deployment; liefert die
    /// Anzahl je Deployment (`None` = keins verfügbar).
    fn run(route: &GroupRoute, n: usize, cost: &str) -> (Vec<usize>, usize) {
        let mut hits = vec![0; route.members().len()];
        let mut none = 0;
        for _ in 0..n {
            match route.select_for_bench(false) {
                Some(m) => {
                    hits[m.idx] += 1;
                    let b = m.budget().unwrap();
                    b.budgets.charge(&[b.target()], amount(cost), wall_ms());
                }
                None => none += 1,
            }
        }
        (hits, none)
    }

    fn used(route: &GroupRoute) -> Vec<f64> {
        route
            .members()
            .iter()
            .map(|m| {
                let n = m.budget().unwrap().used(wall_ms()).nanos();
                #[allow(clippy::cast_precision_loss)]
                let v = n as f64 / 1e9;
                v
            })
            .collect()
    }

    #[test]
    fn priority_rotation_switches_to_next_account() {
        let budgets = Arc::new(Budgets::new(Calendar::utc(), None));
        // 4 Konten nacheinander (priority 0..3), je 1,00 Budget, Umschalten bei 100 %.
        let r = route(
            RoutingStrategy::Weighted,
            &budgets,
            &[
                ("1", 0, None),
                ("1", 1, None),
                ("1", 2, None),
                ("1", 3, None),
            ],
        );
        let (hits, none) = run(&r, 4, "0.25");
        assert_eq!((hits.clone(), none), (vec![4, 0, 0, 0], 0));
        let (hits, _) = run(&r, 4, "0.25");
        assert_eq!(hits, vec![0, 4, 0, 0]);
        let (hits, none) = run(&r, 10, "0.25");
        assert_eq!((hits, none), (vec![0, 0, 4, 4], 2), "danach alle erschöpft");
    }

    #[test]
    fn switch_at_percent_keeps_a_reserve() {
        let budgets = Arc::new(Budgets::new(Calendar::utc(), None));
        let r = route(
            RoutingStrategy::Weighted,
            &budgets,
            &[("10", 0, Some(90)), ("10", 1, None)],
        );
        // 9 × 1,00 = 90 % → ab dann das zweite Konto.
        let (hits, _) = run(&r, 12, "1");
        assert_eq!(hits, vec![9, 3]);
        assert_eq!(used(&r), vec![9.0, 3.0]);
    }

    #[test]
    fn budget_balanced_spreads_costs_evenly() {
        let budgets = Arc::new(Budgets::new(Calendar::utc(), None));
        let r = route(
            RoutingStrategy::BudgetBalanced,
            &budgets,
            &[
                ("100", 0, None),
                ("100", 0, None),
                ("100", 0, None),
                ("100", 0, None),
            ],
        );
        let (_, none) = run(&r, 4000, "0.01");
        assert_eq!(none, 0);
        let u = used(&r);
        let mean = u.iter().sum::<f64>() / 4.0;
        assert!((mean - 10.0).abs() < 1e-9, "{u:?}");
        for v in &u {
            assert!((v - mean).abs() <= mean * 0.02, "ungleich verteilt: {u:?}");
        }
    }

    #[test]
    fn budget_balanced_fills_different_budgets_proportionally() {
        let budgets = Arc::new(Budgets::new(Calendar::utc(), None));
        let r = route(
            RoutingStrategy::BudgetBalanced,
            &budgets,
            &[
                ("100", 0, None),
                ("200", 0, None),
                ("300", 0, None),
                ("400", 0, None),
            ],
        );
        run(&r, 5000, "0.1");
        let u = used(&r);
        let total: f64 = u.iter().sum();
        for (v, share) in u.iter().zip([0.1, 0.2, 0.3, 0.4]) {
            let got = v / total;
            assert!((got - share).abs() <= 0.01, "Anteile {u:?}");
        }
        // Erst wenn alle (anteilig) voll sind, ist keins mehr verfügbar.
        let (_, none) = run(&r, 600, "1");
        assert!(none > 0);
        for (m, a) in r.members().iter().zip([100.0, 200.0, 300.0, 400.0]) {
            let n = m.budget().unwrap().used(wall_ms()).nanos();
            #[allow(clippy::cast_precision_loss)]
            let v = n as f64 / 1e9;
            assert!(v >= a, "nicht ausgeschöpft: {v} < {a}");
        }
    }

    #[test]
    fn budget_balanced_without_budget_uses_absolute_costs() {
        let r = GroupRoute::new(
            "g",
            RoutingStrategy::BudgetBalanced,
            GroupPolicy::passthrough(),
            Vec::new(),
            (0..3)
                .map(|_| MemberSpec::new(Arc::new(crate::mock::MockProvider::new())))
                .collect(),
        );
        let mut hits = [0usize; 3];
        for _ in 0..3000 {
            let m = r.select_for_bench(false).unwrap();
            hits[m.idx] += 1;
            m.state.spent_nanos.fetch_add(1_000, Ordering::Relaxed);
        }
        for h in hits {
            assert!((950..=1050).contains(&h), "{hits:?}");
        }
    }

    #[test]
    fn estimate_and_ppm() {
        let budgets = Arc::new(Budgets::new(Calendar::utc(), None));
        let b = MemberBudget::new(
            Arc::clone(&budgets),
            new_id(),
            &spec("200", Some(98)),
            (Some(amount("3")), Some(amount("15"))),
        );
        // 1000 Eingabe × 3/Mio + 500 Ausgabe × 15/Mio = 0,003 + 0,0075
        assert_eq!(b.estimate(1000, 500), amount("0.0105"));
        budgets.charge(&[b.target()], amount("50"), wall_ms());
        assert_eq!(b.busy_ppm(wall_ms()), 250_000);
        assert!(!b.exhausted(wall_ms()));
        budgets.charge(&[b.target()], amount("146"), wall_ms());
        assert!(b.exhausted(wall_ms()), "98 % von 200 = 196 erreicht");
    }
}
