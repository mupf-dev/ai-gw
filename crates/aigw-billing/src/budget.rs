// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Budgets je API-Key, Team und Organisation (AIGW-31).
//!
//! [`Budgets`] führt den Verbrauch je (Ebene, Ziel, Zeitraum, Beginn) im RAM und gleicht ihn
//! im Hintergrund mit einem [`BudgetLedger`] ab (in Betrieb: Tabelle `budget_usage` in SQLite
//! oder Postgres, siehe `aigw_store::budget`). Das Verfahren entspricht `postgres-sync` der
//! Zähler (05 §3.2):
//!
//! - **Hot Path ohne Datenbank:** [`Budgets::check`] vergleicht den bekannten Verbrauch mit
//!   der harten Grenze, [`Budgets::charge`] bucht die Kosten einer Anfrage lokal. Nur der
//!   erste Zugriff auf ein Budget je Zeitraum und Instanz liest dessen Stand einmal aus dem
//!   Ledger (mit Zeitlimit; bei Fehlern wird mit 0 begonnen und gewarnt).
//! - **Abgleich** alle `budget_sync_ms` (Standard 1 s): lokale Deltas werden additiv
//!   geschrieben (`amount = amount + Δ`, keine Read-Modify-Write-Konflikte zwischen Instanzen)
//!   und die Summen aller Instanzen zurückgelesen.
//! - **Genauigkeit:** je Instanz exakt; im Cluster sieht jede Instanz fremden Verbrauch mit bis
//!   zu einem Abgleichintervall Verzögerung. Da die Kosten erst nach der Antwort feststehen,
//!   kann ein Budget außerdem um die Kosten gleichzeitig laufender Anfragen überschritten
//!   werden (keine Reservierung geschätzter Kosten).
//! - **Ausfall des Ledgers:** lokal weiterzählen, Deltas bleiben vorgemerkt und werden beim
//!   nächsten erfolgreichen Abgleich nachgeholt.
//!
//! Warnschwellen (`warnAtPercent`) und das Erreichen der Grenze lösen je Zeitraum und Instanz
//! einmal ein [`BudgetEvent`] aus ([`BudgetEventSink`]; Standard: Log, Webhook vorbereitet).

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aigw_core::resource::{Amount, Budget, BudgetPeriod, Id};
use dashmap::DashMap;
use tokio::sync::OnceCell;

use crate::calendar::{Calendar, PeriodRange};

/// Ebene eines Budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BudgetLevel {
    /// Organisation.
    Organization,
    /// Team.
    Team,
    /// API-Key.
    ApiKey,
}

impl BudgetLevel {
    /// Bezeichner wie in der Spalte `budget_usage.level`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Organization => "organization",
            Self::Team => "team",
            Self::ApiKey => "api-key",
        }
    }

    /// Gegenstück zu [`BudgetLevel::as_str`].
    pub fn from_code(code: &str) -> Option<Self> {
        Some(match code {
            "organization" => Self::Organization,
            "team" => Self::Team,
            "api-key" => Self::ApiKey,
            _ => return None,
        })
    }
}

impl fmt::Display for BudgetLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Schlüssel eines Budgetstands (= Primärschlüssel von `budget_usage`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BudgetKey {
    /// Ebene.
    pub level: BudgetLevel,
    /// ID von Key, Team bzw. Organisation.
    pub target: Id,
    /// Zeitraum.
    pub period: BudgetPeriod,
    /// Beginn des Zeitraums (ms seit Epoche, in der Kalender-Zeitzone berechnet; `0` bei
    /// `total`).
    pub period_start_ms: i64,
}

/// Ein auf eine Anfrage anzuwendendes Budget (aus dem Snapshot).
#[derive(Debug, Clone, Copy)]
pub struct BudgetTarget<'a> {
    /// Ebene.
    pub level: BudgetLevel,
    /// ID des Ziels.
    pub target: Id,
    /// Budget.
    pub budget: &'a Budget,
}

/// Ablehnung wegen erreichter harter Grenze.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetExceeded {
    /// Ebene des erschöpften Budgets.
    pub level: BudgetLevel,
    /// Ziel.
    pub target: Id,
    /// Zeitraum.
    pub period: BudgetPeriod,
    /// Betrag je Zeitraum.
    pub amount: Amount,
    /// Bekannter Verbrauch im Zeitraum.
    pub used: Amount,
    /// Ende des Zeitraums (ms seit Epoche); `None` bei `total`.
    pub resets_at_ms: Option<i64>,
}

impl BudgetExceeded {
    /// Zeit bis zum Beginn des nächsten Zeitraums (für `Retry-After`).
    pub fn retry_after(&self, now_ms: i64) -> Option<Duration> {
        let end = self.resets_at_ms?;
        let ms = u64::try_from(end.saturating_sub(now_ms)).unwrap_or(0);
        Some(Duration::from_millis(ms.max(1)))
    }
}

impl fmt::Display for BudgetExceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Budget ({}, {}) erschöpft: {} von {} verbraucht",
            self.level,
            crate::calendar::period_code(self.period),
            self.used,
            self.amount
        )
    }
}

/// Art eines Budget-Ereignisses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetEventKind {
    /// Warnschwelle (Prozent des Betrags) erreicht.
    Warning {
        /// Schwelle in Prozent.
        percent: u8,
    },
    /// Betrag erreicht (100 %); bei harter Grenze werden weitere Anfragen abgelehnt.
    Exhausted,
}

/// Ereignis an Log/Webhook (je Zeitraum, Schwelle und Instanz einmal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetEvent {
    /// Art.
    pub kind: BudgetEventKind,
    /// Budgetstand.
    pub key: BudgetKey,
    /// Betrag je Zeitraum.
    pub amount: Amount,
    /// Verbrauch bei Auslösung.
    pub used: Amount,
    /// Harte Grenze aktiv.
    pub hard_limit: bool,
}

/// Empfänger von Budget-Ereignissen. Darf nicht blockieren (wird im Hot Path bzw. im
/// Abgleich aufgerufen); ein Webhook-Empfänger muss intern puffern.
pub trait BudgetEventSink: Send + Sync + fmt::Debug {
    /// Ein Ereignis.
    fn on_event(&self, event: &BudgetEvent);
}

/// Standard-Empfänger: schreibt Ereignisse ins Log (`warn`).
#[derive(Debug, Clone, Copy, Default)]
pub struct LogEvents;

impl BudgetEventSink for LogEvents {
    fn on_event(&self, ev: &BudgetEvent) {
        let level = ev.key.level.as_str();
        let target = ev.key.target.to_string();
        let period = crate::calendar::period_code(ev.key.period);
        match ev.kind {
            BudgetEventKind::Warning { percent } => tracing::warn!(
                level,
                target,
                period,
                percent,
                used = %ev.used,
                amount = %ev.amount,
                "Budget-Warnschwelle erreicht"
            ),
            BudgetEventKind::Exhausted => tracing::warn!(
                level,
                target,
                period,
                used = %ev.used,
                amount = %ev.amount,
                hard_limit = ev.hard_limit,
                "Budget erschöpft"
            ),
        }
    }
}

/// Fehler eines Ledgers.
#[derive(Debug, thiserror::Error)]
#[error("Budget-Ledger: {0}")]
pub struct LedgerError(pub String);

/// Persistenter, instanzübergreifender Budgetstand.
#[async_trait::async_trait]
pub trait BudgetLedger: Send + Sync + fmt::Debug {
    /// Addiert `deltas` (Nano-Einheiten, je Schlüssel höchstens einmal) und liefert danach
    /// die Summen aller Instanzen für `read` (gleiche Reihenfolge; unbekannt = 0) – möglichst
    /// in einer Transaktion.
    async fn sync(
        &self,
        deltas: &[(BudgetKey, i64)],
        read: &[BudgetKey],
        now_ms: i64,
    ) -> Result<Vec<i64>, LedgerError>;
}

/// Ledger im RAM (Tests; mehrere [`Budgets`] können sich eines teilen wie Instanzen eine
/// Datenbank).
#[derive(Debug, Default)]
pub struct MemoryLedger {
    rows: Mutex<HashMap<BudgetKey, i64>>,
    /// Simulierter Ausfall.
    failing: AtomicBool,
}

impl MemoryLedger {
    /// Stand eines Schlüssels.
    pub fn get(&self, key: &BudgetKey) -> i64 {
        lock(&self.rows).get(key).copied().unwrap_or(0)
    }

    /// Schaltet einen simulierten Ausfall ein/aus.
    pub fn set_failing(&self, failing: bool) {
        self.failing.store(failing, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl BudgetLedger for MemoryLedger {
    async fn sync(
        &self,
        deltas: &[(BudgetKey, i64)],
        read: &[BudgetKey],
        _now_ms: i64,
    ) -> Result<Vec<i64>, LedgerError> {
        if self.failing.load(Ordering::SeqCst) {
            return Err(LedgerError("simulierter Ausfall".into()));
        }
        let mut rows = lock(&self.rows);
        for (k, d) in deltas {
            let v = rows.entry(*k).or_insert(0);
            *v = v.saturating_add(*d);
        }
        Ok(read
            .iter()
            .map(|k| rows.get(k).copied().unwrap_or(0))
            .collect())
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

#[derive(Debug, Default)]
struct EntryState {
    /// Summe aller Instanzen laut letztem Abgleich (inkl. eigener geschriebener Deltas).
    synced: i64,
    /// Gerade im Abgleich befindliche eigene Deltas.
    in_flight: i64,
    /// Eigene, noch nicht geschriebene Deltas.
    pending: i64,
    /// Zuletzt gesehene Budget-Einstellungen (für Ereignisse im Abgleich).
    amount: i64,
    warn_at: Vec<u8>,
    hard: bool,
    /// Bereits gemeldete Schwellen (Prozent) bzw. Erschöpfung.
    warned: Vec<u8>,
    exhausted: bool,
}

impl EntryState {
    fn used(&self) -> i64 {
        self.synced
            .saturating_add(self.in_flight)
            .saturating_add(self.pending)
    }

    fn configure(&mut self, b: &Budget) {
        self.amount = b.amount.nanos();
        self.hard = b.hard_limit;
        if self.warn_at != b.warn_at_percent {
            self.warn_at.clone_from(&b.warn_at_percent);
        }
    }

    /// Neu erreichte Schwellen.
    fn events(&mut self, key: BudgetKey, out: &mut Vec<BudgetEvent>) {
        if self.amount <= 0 && self.warn_at.is_empty() {
            return;
        }
        let used_nanos = self.used();
        let (used, amount) = (i128::from(used_nanos), i128::from(self.amount));
        let (amount_nanos, hard) = (self.amount, self.hard);
        let ev = |kind| BudgetEvent {
            kind,
            key,
            amount: Amount::from_nanos(amount_nanos),
            used: Amount::from_nanos(used_nanos),
            hard_limit: hard,
        };
        for &p in &self.warn_at {
            if !self.warned.contains(&p) && used * 100 >= amount * i128::from(p) {
                self.warned.push(p);
                out.push(ev(BudgetEventKind::Warning { percent: p }));
            }
        }
        if !self.exhausted && used >= amount {
            self.exhausted = true;
            out.push(ev(BudgetEventKind::Exhausted));
        }
    }
}

#[derive(Debug)]
struct Entry {
    loaded: OnceCell<()>,
    end_ms: Option<i64>,
    st: Mutex<EntryState>,
}

/// Budgetstände im RAM mit Abgleich gegen ein [`BudgetLedger`].
#[derive(Debug)]
pub struct Budgets {
    calendar: Calendar,
    ledger: Option<Arc<dyn BudgetLedger>>,
    events: Arc<dyn BudgetEventSink>,
    entries: DashMap<BudgetKey, Arc<Entry>>,
    load_timeout: Duration,
    ledger_down: AtomicBool,
}

impl Budgets {
    /// Neue Budgetstände; ohne Ledger nur im RAM (je Instanz, nicht persistent).
    pub fn new(calendar: Calendar, ledger: Option<Arc<dyn BudgetLedger>>) -> Self {
        Self {
            calendar,
            ledger,
            events: Arc::new(LogEvents),
            entries: DashMap::new(),
            load_timeout: Duration::from_secs(2),
            ledger_down: AtomicBool::new(false),
        }
    }

    /// Anderer Empfänger für Ereignisse.
    #[must_use]
    pub fn with_events(mut self, events: Arc<dyn BudgetEventSink>) -> Self {
        self.events = events;
        self
    }

    /// Kalender (Zeitzone der Zeiträume).
    pub fn calendar(&self) -> &Calendar {
        &self.calendar
    }

    /// Zeitraum eines Budgets zum Zeitpunkt `at_ms`.
    pub fn period(&self, period: BudgetPeriod, at_ms: i64) -> PeriodRange {
        self.calendar.period(period, at_ms)
    }

    /// Schlüssel des Budgetstands von `t` zum Zeitpunkt `at_ms`.
    pub fn key(&self, t: &BudgetTarget<'_>, at_ms: i64) -> (BudgetKey, PeriodRange) {
        let range = self.calendar.period(t.budget.period, at_ms);
        (
            BudgetKey {
                level: t.level,
                target: t.target,
                period: t.budget.period,
                period_start_ms: range.start_ms,
            },
            range,
        )
    }

    fn entry(&self, key: BudgetKey, range: PeriodRange) -> Arc<Entry> {
        if let Some(e) = self.entries.get(&key) {
            return Arc::clone(e.value());
        }
        Arc::clone(
            self.entries
                .entry(key)
                .or_insert_with(|| {
                    Arc::new(Entry {
                        loaded: OnceCell::new(),
                        end_ms: range.end_ms,
                        st: Mutex::new(EntryState::default()),
                    })
                })
                .value(),
        )
    }

    /// Liest den Stand eines Budgets beim ersten Zugriff (je Zeitraum und Instanz einmal).
    async fn ensure_loaded(&self, key: BudgetKey, entry: &Entry) {
        if entry.loaded.initialized() {
            return;
        }
        entry
            .loaded
            .get_or_init(|| async {
                let Some(ledger) = &self.ledger else {
                    return;
                };
                let read =
                    tokio::time::timeout(self.load_timeout, ledger.sync(&[], &[key], now_ms()))
                        .await;
                match read {
                    Ok(Ok(v)) => {
                        let total = v.first().copied().unwrap_or(0);
                        let mut st = lock(&entry.st);
                        st.synced = st.synced.max(total);
                    }
                    Ok(Err(e)) => self.mark_down(&e.to_string()),
                    Err(_) => self.mark_down("Zeitüberschreitung beim Lesen"),
                }
            })
            .await;
    }

    fn mark_down(&self, reason: &str) {
        if self.ledger_down.swap(true, Ordering::AcqRel) {
            tracing::debug!(reason, "Budget-Ledger weiterhin nicht erreichbar");
        } else {
            tracing::warn!(
                reason,
                "Budget-Ledger nicht erreichbar – Budgets zählen lokal weiter"
            );
        }
    }

    fn mark_up(&self) {
        if self.ledger_down.swap(false, Ordering::AcqRel) {
            tracing::info!("Budget-Ledger wieder erreichbar – Stände abgeglichen");
        }
    }

    /// `true`, solange der letzte Zugriff auf das Ledger fehlschlug.
    pub fn is_degraded(&self) -> bool {
        self.ledger_down.load(Ordering::Acquire)
    }

    /// Prüft die harten Grenzen aller `targets` zum Zeitpunkt `now_ms` (vor dem
    /// Upstream-Aufruf). Budgets ohne harte Grenze werden nicht geprüft.
    pub async fn check(
        &self,
        targets: &[BudgetTarget<'_>],
        now_ms: i64,
    ) -> Result<(), BudgetExceeded> {
        for t in targets.iter().filter(|t| t.budget.hard_limit) {
            let (key, range) = self.key(t, now_ms);
            let entry = self.entry(key, range);
            self.ensure_loaded(key, &entry).await;
            let used = {
                let mut st = lock(&entry.st);
                st.configure(t.budget);
                st.used()
            };
            if used >= t.budget.amount.nanos() {
                return Err(BudgetExceeded {
                    level: t.level,
                    target: t.target,
                    period: t.budget.period,
                    amount: t.budget.amount,
                    used: Amount::from_nanos(used),
                    resets_at_ms: range.end_ms,
                });
            }
        }
        Ok(())
    }

    /// Bucht `cost` auf alle `targets` im Zeitraum von `at_ms` (synchron, ohne I/O) und meldet
    /// neu erreichte Warnschwellen.
    pub fn charge(&self, targets: &[BudgetTarget<'_>], cost: Amount, at_ms: i64) {
        if cost.nanos() == 0 {
            return;
        }
        let mut events = Vec::new();
        for t in targets {
            let (key, range) = self.key(t, at_ms);
            let entry = self.entry(key, range);
            let mut st = lock(&entry.st);
            st.configure(t.budget);
            st.pending = st.pending.saturating_add(cost.nanos());
            if entry.loaded.initialized() {
                st.events(key, &mut events);
            }
        }
        for ev in &events {
            self.events.on_event(ev);
        }
    }

    /// Bekannter Verbrauch eines Budgetstands (RAM; `None`, wenn nicht geführt).
    pub fn used(&self, key: &BudgetKey) -> Option<Amount> {
        let e = self.entries.get(key)?;
        let used = lock(&e.st).used();
        Some(Amount::from_nanos(used))
    }

    /// Ein Abgleich mit dem Ledger: Deltas schreiben, Summen aller Instanzen lesen,
    /// abgelaufene Zeiträume verwerfen. Ohne Ledger nur die Bereinigung.
    pub async fn flush(&self) -> Result<(), LedgerError> {
        let now = now_ms();
        let mut taken: Vec<(BudgetKey, Arc<Entry>, i64)> = Vec::with_capacity(self.entries.len());
        for item in &self.entries {
            let e = Arc::clone(item.value());
            let d = {
                let mut st = lock(&e.st);
                let d = std::mem::take(&mut st.pending);
                st.in_flight = st.in_flight.saturating_add(d);
                d
            };
            taken.push((*item.key(), e, d));
        }
        let mut result = Ok(());
        if let Some(ledger) = &self.ledger
            && !taken.is_empty()
        {
            let deltas: Vec<(BudgetKey, i64)> = taken
                .iter()
                .filter(|(_, _, d)| *d != 0)
                .map(|(k, _, d)| (*k, *d))
                .collect();
            let keys: Vec<BudgetKey> = taken.iter().map(|(k, _, _)| *k).collect();
            match ledger.sync(&deltas, &keys, now).await {
                Ok(totals) => {
                    self.mark_up();
                    let mut events = Vec::new();
                    for ((key, e, d), total) in taken.iter().zip(totals) {
                        {
                            let mut st = lock(&e.st);
                            st.in_flight = st.in_flight.saturating_sub(*d);
                            st.synced = total;
                            st.events(*key, &mut events);
                        }
                        let _ = e.loaded.set(());
                    }
                    for ev in &events {
                        self.events.on_event(ev);
                    }
                }
                Err(e) => {
                    self.mark_down(&e.0);
                    for (_, entry, d) in &taken {
                        let mut st = lock(&entry.st);
                        st.in_flight = st.in_flight.saturating_sub(*d);
                        st.pending = st.pending.saturating_add(*d);
                    }
                    result = Err(e);
                }
            }
        } else {
            // Ohne Ledger gilt der lokale Stand.
            for (_, e, d) in &taken {
                let mut st = lock(&e.st);
                st.in_flight = st.in_flight.saturating_sub(*d);
                st.synced = st.synced.saturating_add(*d);
            }
        }
        // Abgelaufene Zeiträume ohne offene Deltas verwerfen.
        self.entries.retain(|_, e| {
            e.end_ms.is_none_or(|end| end > now) || {
                let st = lock(&e.st);
                st.pending != 0 || st.in_flight != 0
            }
        });
        result
    }

    /// Anzahl geführter Budgetstände.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true`, wenn kein Budgetstand geführt wird.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Startet den periodischen Abgleich (Muss innerhalb einer Tokio-Laufzeit aufgerufen
    /// werden).
    pub fn spawn_sync(self: &Arc<Self>, interval: Duration) -> BudgetSyncTask {
        let weak = Arc::downgrade(self);
        let interval = interval.max(Duration::from_millis(50));
        let handle = tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await;
            loop {
                tick.tick().await;
                let Some(b) = weak.upgrade() else {
                    break;
                };
                let _ = b.flush().await;
            }
        });
        BudgetSyncTask {
            budgets: Arc::clone(self),
            handle: Some(handle),
        }
    }
}

/// Laufender Abgleich; beim Verwerfen wird er beendet (ohne letzten Abgleich, dafür
/// [`BudgetSyncTask::shutdown`]).
#[derive(Debug)]
pub struct BudgetSyncTask {
    budgets: Arc<Budgets>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl BudgetSyncTask {
    /// Beendet den Abgleich und schreibt die restlichen Deltas.
    pub async fn shutdown(mut self) {
        if let Some(h) = self.handle.take() {
            h.abort();
            let _ = h.await;
        }
        if let Err(e) = self.budgets.flush().await {
            tracing::warn!(error = %e, "Budgets beim Herunterfahren nicht vollständig geschrieben");
        }
    }
}

impl Drop for BudgetSyncTask {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use aigw_core::resource::new_id;

    use super::*;

    #[derive(Debug, Default)]
    struct Collect(Mutex<Vec<BudgetEvent>>);

    impl BudgetEventSink for Collect {
        fn on_event(&self, ev: &BudgetEvent) {
            lock(&self.0).push(ev.clone());
        }
    }

    fn budget(amount: &str, period: BudgetPeriod, warn: &[u8], hard: bool) -> Budget {
        Budget {
            amount: amount.parse().unwrap(),
            period,
            warn_at_percent: warn.to_vec(),
            hard_limit: hard,
        }
    }

    fn a(s: &str) -> Amount {
        s.parse().unwrap()
    }

    #[tokio::test]
    async fn hard_limit_and_warnings() {
        let events = Arc::new(Collect::default());
        let b = Budgets::new(Calendar::utc(), None).with_events(events.clone());
        let id = new_id();
        let spec = budget("1", BudgetPeriod::Day, &[50, 80], true);
        let t = [BudgetTarget {
            level: BudgetLevel::ApiKey,
            target: id,
            budget: &spec,
        }];
        let now = now_ms();
        b.check(&t, now).await.unwrap();
        b.charge(&t, a("0.4"), now);
        assert!(lock(&events.0).is_empty());
        b.charge(&t, a("0.2"), now);
        assert_eq!(
            lock(&events.0)[0].kind,
            BudgetEventKind::Warning { percent: 50 }
        );
        b.check(&t, now).await.unwrap();
        b.charge(&t, a("0.5"), now); // 1,1 > 1
        let kinds: Vec<_> = lock(&events.0).iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            [
                BudgetEventKind::Warning { percent: 50 },
                BudgetEventKind::Warning { percent: 80 },
                BudgetEventKind::Exhausted
            ]
        );
        let err = b.check(&t, now).await.unwrap_err();
        assert_eq!(err.used, a("1.1"));
        assert_eq!(err.level, BudgetLevel::ApiKey);
        assert!(err.retry_after(now).unwrap() <= Duration::from_secs(86_400));
        // Weitere Buchungen lösen keine doppelten Ereignisse aus.
        b.charge(&t, a("0.1"), now);
        assert_eq!(lock(&events.0).len(), 3);
        // Ohne harte Grenze nie abgelehnt.
        let soft = budget("1", BudgetPeriod::Day, &[], false);
        let ts = [BudgetTarget {
            level: BudgetLevel::ApiKey,
            target: id,
            budget: &soft,
        }];
        b.check(&ts, now).await.unwrap();
        // Nächster Tag: neuer Zeitraum.
        b.check(&t, now + 86_400_000).await.unwrap();
    }

    #[tokio::test]
    async fn two_instances_share_ledger() {
        let ledger = Arc::new(MemoryLedger::default());
        let i1 = Budgets::new(Calendar::utc(), Some(ledger.clone()));
        let i2 = Budgets::new(Calendar::utc(), Some(ledger.clone()));
        let spec = budget("1", BudgetPeriod::Month, &[], true);
        let t = [BudgetTarget {
            level: BudgetLevel::Team,
            target: new_id(),
            budget: &spec,
        }];
        let now = now_ms();
        i1.check(&t, now).await.unwrap();
        i2.check(&t, now).await.unwrap();
        i1.charge(&t, a("0.6"), now);
        i2.charge(&t, a("0.3"), now);
        // Vor dem Abgleich kennt jede Instanz nur ihren Anteil.
        i2.check(&t, now).await.unwrap();
        i1.flush().await.unwrap();
        i2.flush().await.unwrap();
        i1.flush().await.unwrap();
        let (key, _) = i1.key(&t[0], now);
        assert_eq!(ledger.get(&key), a("0.9").nanos());
        assert_eq!(i1.used(&key), Some(a("0.9")));
        assert_eq!(i2.used(&key), Some(a("0.9")));
        i2.charge(&t, a("0.1"), now);
        i2.flush().await.unwrap();
        i1.flush().await.unwrap();
        assert!(i1.check(&t, now).await.is_err());

        // Eine dritte, neue Instanz liest den Stand beim ersten Zugriff.
        let i3 = Budgets::new(Calendar::utc(), Some(ledger.clone()));
        assert!(i3.check(&t, now).await.is_err());
    }

    #[tokio::test]
    async fn ledger_outage_keeps_deltas() {
        let ledger = Arc::new(MemoryLedger::default());
        let b = Budgets::new(Calendar::utc(), Some(ledger.clone()));
        let spec = budget("10", BudgetPeriod::Total, &[], true);
        let t = [BudgetTarget {
            level: BudgetLevel::Organization,
            target: new_id(),
            budget: &spec,
        }];
        let now = now_ms();
        b.check(&t, now).await.unwrap();
        ledger.set_failing(true);
        b.charge(&t, a("2"), now);
        assert!(b.flush().await.is_err());
        assert!(b.is_degraded());
        let (key, range) = b.key(&t[0], now);
        assert_eq!(range.end_ms, None);
        assert_eq!(b.used(&key), Some(a("2")));
        assert_eq!(ledger.get(&key), 0);
        b.charge(&t, a("1"), now);
        ledger.set_failing(false);
        b.flush().await.unwrap();
        assert!(!b.is_degraded());
        assert_eq!(ledger.get(&key), a("3").nanos());
        assert_eq!(b.used(&key), Some(a("3")));
    }

    #[tokio::test]
    async fn expired_periods_are_pruned() {
        let b = Budgets::new(Calendar::utc(), None);
        let spec = budget("1", BudgetPeriod::Day, &[], true);
        let t = [BudgetTarget {
            level: BudgetLevel::ApiKey,
            target: new_id(),
            budget: &spec,
        }];
        let yesterday = now_ms() - 86_400_000;
        b.check(&t, yesterday).await.unwrap();
        b.charge(&t, a("0.5"), yesterday);
        assert_eq!(b.len(), 1);
        b.flush().await.unwrap();
        assert!(b.is_empty());
    }

    #[test]
    fn level_codes_roundtrip() {
        for l in [
            BudgetLevel::Organization,
            BudgetLevel::Team,
            BudgetLevel::ApiKey,
        ] {
            assert_eq!(BudgetLevel::from_code(l.as_str()), Some(l));
        }
        assert_eq!(BudgetLevel::from_code("user"), None);
    }
}
