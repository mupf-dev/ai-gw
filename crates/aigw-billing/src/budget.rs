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
//! - **Reservierung (F106, AIGW-71):** Bei `enforcement: strict` (Standard) reserviert
//!   [`Budgets::reserve`] die geschätzten Kosten einer Anfrage vor dem Upstream-Aufruf; der
//!   [`BudgetHold`] wird mit [`Budgets::settle`] gegen die tatsächlichen Kosten getauscht.
//!   Abgelehnt wird, wenn Verbrauch + laufende Reservierungen + Schätzung den Betrag
//!   überschreiten würden. `soft` lehnt wie bisher erst ab, wenn der Verbrauch den Betrag
//!   erreicht hat.
//! - **Genauigkeit:** je Instanz exakt (strict: keine Überschreitung, solange die tatsächlichen
//!   Kosten die Schätzung nicht übersteigen). Im Cluster sieht jede Instanz fremden Verbrauch
//!   mit bis zu zwei Abgleichintervallen Verzögerung (fremdes Schreiben + eigenes Lesen) –
//!   strikte Budgets überschreiten höchstens um die in dieser Zeit angefallenen Kosten der
//!   anderen Instanzen. Mit einem geteilten Zähler ([`SharedBudgets`],
//!   in Betrieb Redis) sind strikte Budgets auch im Cluster exakt.
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

use aigw_core::resource::{Amount, Budget, BudgetEnforcement, BudgetPeriod, Id};
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

/// Grund einer Budget-Ablehnung.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExceededKind {
    /// Der Verbrauch hat den Betrag erreicht.
    Exhausted,
    /// Laufende Reservierungen anderer Anfragen belegen den Rest (strict) – kurz warten hilft.
    Reserved,
    /// Die geschätzten Kosten dieser Anfrage passen nicht in den Rest (strict) – kleineres
    /// `max_tokens` oder neuer Zeitraum.
    Estimate,
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
    /// Laufende Reservierungen (strict; bei geteiltem Zähler: Verbrauch + Reservierungen im
    /// Cluster abzüglich `used`).
    pub reserved: Amount,
    /// Geschätzte Kosten dieser Anfrage.
    pub requested: Amount,
    /// Grund.
    pub kind: ExceededKind,
    /// Ende des Zeitraums (ms seit Epoche); `None` bei `total`.
    pub resets_at_ms: Option<i64>,
}

impl BudgetExceeded {
    /// Wartezeit für `Retry-After`: bis zum Beginn des nächsten Zeitraums; bei Ablehnung
    /// wegen laufender Reservierungen 1 s.
    pub fn retry_after(&self, now_ms: i64) -> Option<Duration> {
        if self.kind == ExceededKind::Reserved {
            return Some(Duration::from_secs(1));
        }
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

/// Ein Eintrag einer geteilten Reservierung.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SharedItem {
    /// Budgetstand.
    pub key: BudgetKey,
    /// Startwert, falls der Zähler noch nicht existiert (bekannter Verbrauch dieser Instanz,
    /// Nano-Einheiten).
    pub base: i64,
    /// Betrag des Budgets (Nano-Einheiten).
    pub limit: i64,
    /// Zu reservierende Kosten (Nano-Einheiten, ≥ 0).
    pub amount: i64,
    /// Ablauf des Zählers (ms seit Epoche; `None` = ohne Ablauf).
    pub expire_at_ms: Option<i64>,
}

/// Ergebnis einer geteilten Reservierung.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedReserve {
    /// Für alle Einträge gebucht.
    Reserved,
    /// Eintrag `index` hätte den Betrag überschritten (Stand `current`); nichts gebucht.
    Rejected {
        /// Index in den übergebenen Einträgen.
        index: usize,
        /// Stand (Verbrauch + Reservierungen im Cluster, Nano-Einheiten).
        current: i64,
    },
}

/// Instanzübergreifender, atomarer Zähler für strikte Budgets (Verbrauch + laufende
/// Reservierungen aller Instanzen). In Betrieb Redis (`counters.backend: redis`); ohne ihn
/// gleichen sich Instanzen über das [`BudgetLedger`] im Abgleich-Takt ab.
#[async_trait::async_trait]
pub trait SharedBudgets: Send + Sync + fmt::Debug {
    /// Atomar über alle `items`: Fehlende Zähler werden mit `base` angelegt; würde bei einem
    /// Eintrag `stand ≥ limit` oder `stand + amount > limit` gelten, wird nichts gebucht,
    /// sonst wird jedem Zähler `amount` hinzugefügt.
    async fn reserve(&self, items: &[SharedItem]) -> Result<SharedReserve, LedgerError>;

    /// Korrigiert Zähler (Abgleich mit den tatsächlichen Kosten, Freigabe). Nicht vorhandene
    /// Zähler werden nicht angelegt.
    async fn adjust(&self, deltas: &[(BudgetKey, i64)]) -> Result<(), LedgerError>;
}

/// Geteilter Zähler im RAM (Tests: mehrere [`Budgets`] teilen sich einen wie Instanzen
/// einen Redis).
#[derive(Debug, Default)]
pub struct MemoryShared {
    rows: Mutex<HashMap<BudgetKey, i64>>,
}

impl MemoryShared {
    /// Stand eines Zählers.
    pub fn get(&self, key: &BudgetKey) -> Option<i64> {
        lock(&self.rows).get(key).copied()
    }
}

#[async_trait::async_trait]
impl SharedBudgets for MemoryShared {
    async fn reserve(&self, items: &[SharedItem]) -> Result<SharedReserve, LedgerError> {
        let mut rows = lock(&self.rows);
        for (index, it) in items.iter().enumerate() {
            let cur = *rows.entry(it.key).or_insert(it.base);
            if cur >= it.limit || cur.saturating_add(it.amount) > it.limit {
                return Ok(SharedReserve::Rejected {
                    index,
                    current: cur,
                });
            }
        }
        for it in items {
            if let Some(v) = rows.get_mut(&it.key) {
                *v = v.saturating_add(it.amount);
            }
        }
        Ok(SharedReserve::Reserved)
    }

    async fn adjust(&self, deltas: &[(BudgetKey, i64)]) -> Result<(), LedgerError> {
        let mut rows = lock(&self.rows);
        for (k, d) in deltas {
            if let Some(v) = rows.get_mut(k) {
                *v = v.saturating_add(*d);
            }
        }
        Ok(())
    }
}

/// Reservierte Kosten einer Anfrage (F106). Mit [`Budgets::settle`] abschließen; wird sie
/// verworfen, werden die Reservierungen freigegeben (ohne Buchung).
#[must_use = "Budget-Reservierungen müssen mit Budgets::settle abgeschlossen werden"]
#[derive(Debug, Default)]
pub struct BudgetHold {
    local: Vec<(Arc<Entry>, i64)>,
    shared: Option<SharedHold>,
}

/// Geteilter Zähler samt reservierten Beträgen je Budgetstand.
type SharedHold = (Arc<dyn SharedBudgets>, Vec<(BudgetKey, i64)>);

impl BudgetHold {
    /// `true`, wenn nichts reserviert ist.
    pub fn is_empty(&self) -> bool {
        self.local.is_empty() && self.shared.is_none()
    }

    /// Lokal reservierter Betrag je Budget (Nano-Einheiten; gleich für alle Budgets).
    pub fn reserved(&self) -> Amount {
        Amount::from_nanos(self.local.first().map_or(0, |(_, n)| *n))
    }

    fn release_local(&mut self) {
        for (e, n) in self.local.drain(..) {
            let mut st = lock(&e.st);
            st.reserved = st.reserved.saturating_sub(n);
        }
    }
}

impl Drop for BudgetHold {
    fn drop(&mut self) {
        self.release_local();
        if let Some((shared, keys)) = self.shared.take() {
            let deltas: Vec<(BudgetKey, i64)> = keys.into_iter().map(|(k, n)| (k, -n)).collect();
            spawn_adjust(shared, deltas);
        }
    }
}

/// Korrigiert den geteilten Zähler im Hintergrund (der Aufrufer wartet nie).
fn spawn_adjust(shared: Arc<dyn SharedBudgets>, deltas: Vec<(BudgetKey, i64)>) {
    let deltas: Vec<(BudgetKey, i64)> = deltas.into_iter().filter(|(_, d)| *d != 0).collect();
    if deltas.is_empty() {
        return;
    }
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    rt.spawn(async move {
        if let Err(e) = shared.adjust(&deltas).await {
            tracing::debug!(error = %e, "geteilter Budgetzähler: Abgleich verloren");
        }
    });
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
    /// Reservierte geschätzte Kosten laufender Anfragen dieser Instanz (strict, F106).
    reserved: i64,
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
    shared: Option<Arc<dyn SharedBudgets>>,
    shared_down: AtomicBool,
}

/// Ablauf eines geteilten Zählers: Ende des Zeitraums + 1 Tag (`total`: ohne Ablauf).
fn shared_expiry(end_ms: Option<i64>) -> Option<i64> {
    end_ms.map(|e| e.saturating_add(86_400_000))
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
            shared: None,
            shared_down: AtomicBool::new(false),
        }
    }

    /// Geteilter Zähler für strikte Budgets (Redis): Reservierungen sind dann im Cluster
    /// exakt statt im Abgleich-Takt.
    #[must_use]
    pub fn with_shared(mut self, shared: Arc<dyn SharedBudgets>) -> Self {
        self.shared = Some(shared);
        self
    }

    /// Ist ein geteilter Zähler konfiguriert?
    pub fn has_shared(&self) -> bool {
        self.shared.is_some()
    }

    fn shared_result(&self, ok: Result<(), &str>) {
        match ok {
            Ok(()) => {
                if self.shared_down.swap(false, Ordering::AcqRel) {
                    tracing::info!("geteilter Budgetzähler wieder erreichbar");
                }
            }
            Err(reason) => {
                if self.shared_down.swap(true, Ordering::AcqRel) {
                    tracing::debug!(reason, "geteilter Budgetzähler weiterhin nicht erreichbar");
                } else {
                    tracing::warn!(
                        reason,
                        "geteilter Budgetzähler nicht erreichbar – strikte Budgets nur lokal \
                         (Abgleich-Takt)"
                    );
                }
            }
        }
    }

    /// Prüft die harten Grenzen aller `targets` und reserviert bei `strict` die geschätzten
    /// Kosten `estimate` (vor dem Upstream-Aufruf, F106).
    ///
    /// - `strict`: abgelehnt, wenn Verbrauch + Reservierungen ≥ Betrag oder Verbrauch +
    ///   Reservierungen + Schätzung > Betrag. Mit geteiltem Zähler zusätzlich atomar gegen den
    ///   Clusterstand.
    /// - `soft`: abgelehnt, wenn der Verbrauch den Betrag erreicht hat (keine Reservierung).
    ///
    /// Alle oder keine: Lehnt ein Budget ab, werden bereits gemachte Reservierungen
    /// freigegeben.
    pub async fn reserve(
        &self,
        targets: &[BudgetTarget<'_>],
        estimate: Amount,
        now_ms: i64,
    ) -> Result<BudgetHold, BudgetExceeded> {
        let est = estimate.nanos().max(0);
        let mut hold = BudgetHold::default();
        // (Eintrag, Index in targets, lokaler Verbrauch, Ende des Zeitraums)
        let mut shared_items: Vec<(SharedItem, usize, i64, Option<i64>)> = Vec::new();
        for (ti, t) in targets
            .iter()
            .enumerate()
            .filter(|(_, t)| t.budget.hard_limit)
        {
            let (key, range) = self.key(t, now_ms);
            let entry = self.entry(key, range);
            self.ensure_loaded(key, &entry).await;
            let amount = t.budget.amount.nanos();
            let strict = t.budget.enforcement == BudgetEnforcement::Strict;
            let mut st = lock(&entry.st);
            st.configure(t.budget);
            let used = st.used();
            let reserved = if strict { st.reserved } else { 0 };
            let busy = used.saturating_add(reserved);
            let kind = if used >= amount {
                Some(ExceededKind::Exhausted)
            } else if !strict {
                None
            } else if used.saturating_add(est) > amount {
                Some(ExceededKind::Estimate)
            } else if busy >= amount || busy.saturating_add(est) > amount {
                Some(ExceededKind::Reserved)
            } else {
                None
            };
            if let Some(kind) = kind {
                drop(st);
                return Err(BudgetExceeded {
                    level: t.level,
                    target: t.target,
                    period: t.budget.period,
                    amount: t.budget.amount,
                    used: Amount::from_nanos(used),
                    reserved: Amount::from_nanos(reserved),
                    requested: Amount::from_nanos(est),
                    kind,
                    resets_at_ms: range.end_ms,
                });
            }
            if strict {
                if est > 0 {
                    st.reserved = st.reserved.saturating_add(est);
                    drop(st);
                    hold.local.push((Arc::clone(&entry), est));
                }
                shared_items.push((
                    SharedItem {
                        key,
                        base: used,
                        limit: amount,
                        amount: est,
                        expire_at_ms: shared_expiry(range.end_ms),
                    },
                    ti,
                    used,
                    range.end_ms,
                ));
            }
        }
        if let (Some(shared), false) = (&self.shared, shared_items.is_empty()) {
            let items: Vec<SharedItem> = shared_items.iter().map(|(i, ..)| *i).collect();
            match shared.reserve(&items).await {
                Ok(SharedReserve::Reserved) => {
                    self.shared_result(Ok(()));
                    if est > 0 {
                        hold.shared = Some((
                            Arc::clone(shared),
                            items.iter().map(|i| (i.key, est)).collect(),
                        ));
                    }
                }
                Ok(SharedReserve::Rejected { index, current }) => {
                    self.shared_result(Ok(()));
                    if let Some(&(item, ti, used, end_ms)) = shared_items.get(index)
                        && let Some(t) = targets.get(ti)
                    {
                        let kind = if used >= item.limit {
                            ExceededKind::Exhausted
                        } else if used.saturating_add(est) > item.limit {
                            ExceededKind::Estimate
                        } else {
                            ExceededKind::Reserved
                        };
                        return Err(BudgetExceeded {
                            level: t.level,
                            target: t.target,
                            period: t.budget.period,
                            amount: t.budget.amount,
                            used: Amount::from_nanos(used),
                            reserved: Amount::from_nanos(current.saturating_sub(used).max(0)),
                            requested: Amount::from_nanos(est),
                            kind,
                            resets_at_ms: end_ms,
                        });
                    }
                }
                Err(e) => self.shared_result(Err(&e.0)),
            }
        }
        Ok(hold)
    }

    /// Schließt eine Reservierung ab: gibt sie frei und bucht die tatsächlichen Kosten `cost`
    /// (synchron, ohne I/O; der geteilte Zähler wird im Hintergrund korrigiert).
    pub fn settle(
        &self,
        mut hold: BudgetHold,
        targets: &[BudgetTarget<'_>],
        cost: Amount,
        at_ms: i64,
    ) {
        hold.release_local();
        let held = hold.shared.take();
        self.charge_local(targets, cost, at_ms);
        // Geteilter Zähler: Reservierung gegen die tatsächlichen Kosten tauschen.
        let shared = held
            .as_ref()
            .map(|(s, _)| Arc::clone(s))
            .or_else(|| self.shared.clone());
        if let Some(shared) = shared {
            let mut deltas = self.shared_charges(targets, cost, at_ms);
            if let Some((_, keys)) = &held {
                for (k, n) in keys {
                    match deltas.iter_mut().find(|(dk, _)| dk == k) {
                        Some((_, d)) => *d = d.saturating_sub(*n),
                        None => deltas.push((*k, -*n)),
                    }
                }
            }
            spawn_adjust(shared, deltas);
        }
    }

    /// Kosten je strikt durchgesetztem Budget (für den geteilten Zähler).
    fn shared_charges(
        &self,
        targets: &[BudgetTarget<'_>],
        cost: Amount,
        at_ms: i64,
    ) -> Vec<(BudgetKey, i64)> {
        if cost.nanos() == 0 {
            return Vec::new();
        }
        targets
            .iter()
            .filter(|t| t.budget.hard_limit && t.budget.enforcement == BudgetEnforcement::Strict)
            .map(|t| (self.key(t, at_ms).0, cost.nanos()))
            .collect()
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
                    reserved: Amount::ZERO,
                    requested: Amount::ZERO,
                    kind: ExceededKind::Exhausted,
                    resets_at_ms: range.end_ms,
                });
            }
        }
        Ok(())
    }

    /// Bucht `cost` auf alle `targets` im Zeitraum von `at_ms` (synchron, ohne I/O) und meldet
    /// neu erreichte Warnschwellen. Für Kosten ohne vorherige Reservierung (z. B. Embeddings
    /// des semantischen Caches, F169); strikte Budgets im geteilten Zähler werden im
    /// Hintergrund mitgebucht.
    pub fn charge(&self, targets: &[BudgetTarget<'_>], cost: Amount, at_ms: i64) {
        self.charge_local(targets, cost, at_ms);
        if let Some(shared) = &self.shared {
            spawn_adjust(
                Arc::clone(shared),
                self.shared_charges(targets, cost, at_ms),
            );
        }
    }

    fn charge_local(&self, targets: &[BudgetTarget<'_>], cost: Amount, at_ms: i64) {
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
            enforcement: BudgetEnforcement::Strict,
        }
    }

    /// Viele parallele Anfragen gegen ein Budget: je Anfrage `reserve(est)`, kurze Laufzeit,
    /// `settle(cost)`. Liefert (Anzahl zugelassen, Anzahl abgelehnt).
    async fn hammer(
        b: Arc<Budgets>,
        spec: Arc<Budget>,
        target: Id,
        workers: usize,
        per_worker: usize,
        est: Amount,
        cost: Amount,
    ) -> (usize, usize) {
        let mut tasks = Vec::new();
        for _ in 0..workers {
            let (b, spec) = (Arc::clone(&b), Arc::clone(&spec));
            tasks.push(tokio::spawn(async move {
                let (mut ok, mut rej) = (0, 0);
                for _ in 0..per_worker {
                    let t = [BudgetTarget {
                        level: BudgetLevel::Team,
                        target,
                        budget: &spec,
                    }];
                    match b.reserve(&t, est, now_ms()).await {
                        Ok(hold) => {
                            tokio::time::sleep(Duration::from_millis(1)).await;
                            b.settle(hold, &t, cost, now_ms());
                            ok += 1;
                        }
                        Err(_) => {
                            rej += 1;
                            tokio::time::sleep(Duration::from_millis(1)).await;
                        }
                    }
                }
                (ok, rej)
            }));
        }
        let mut sum = (0, 0);
        for t in tasks {
            let (o, r) = t.await.unwrap();
            sum = (sum.0 + o, sum.1 + r);
        }
        sum
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn strict_parallel_never_exceeds_locally() {
        let b = Arc::new(Budgets::new(Calendar::utc(), None));
        let spec = Arc::new(budget("1", BudgetPeriod::Total, &[], true));
        let id = new_id();
        // Schätzung = tatsächliche Kosten (ungünstigster Fall): 0,03 je Anfrage.
        let (ok, rej) = hammer(
            Arc::clone(&b),
            Arc::clone(&spec),
            id,
            32,
            10,
            a("0.03"),
            a("0.03"),
        )
        .await;
        let t = BudgetTarget {
            level: BudgetLevel::Team,
            target: id,
            budget: &spec,
        };
        let used = b.used(&b.key(&t, now_ms()).0).unwrap();
        assert!(used <= a("1"), "{used}");
        assert_eq!(used, a("0.99"), "{ok} zugelassen, {rej} abgelehnt");
        assert_eq!(ok, 33);
        // Alle Reservierungen freigegeben.
        let hold = b.reserve(&[t], a("0.01"), now_ms()).await.unwrap();
        assert_eq!(hold.reserved(), a("0.01"));
        drop(hold);
        assert!(b.reserve(&[t], a("0.02"), now_ms()).await.is_err());
    }

    #[tokio::test]
    async fn strict_reasons_and_soft() {
        let b = Budgets::new(Calendar::utc(), None);
        let id = new_id();
        let spec = budget("1", BudgetPeriod::Day, &[], true);
        let t = [BudgetTarget {
            level: BudgetLevel::ApiKey,
            target: id,
            budget: &spec,
        }];
        let now = now_ms();
        // Schätzung allein zu groß.
        let e = b.reserve(&t, a("1.5"), now).await.unwrap_err();
        assert_eq!(e.kind, ExceededKind::Estimate);
        assert!(e.retry_after(now).unwrap() > Duration::from_secs(1));
        // Laufende Reservierung belegt den Rest → kurz warten.
        let h1 = b.reserve(&t, a("0.7"), now).await.unwrap();
        let e = b.reserve(&t, a("0.4"), now).await.unwrap_err();
        assert_eq!(e.kind, ExceededKind::Reserved);
        assert_eq!(e.reserved, a("0.7"));
        assert_eq!(e.retry_after(now), Some(Duration::from_secs(1)));
        // Abgleich: tatsächlich nur 0,1 → Rest wieder frei.
        b.settle(h1, &t, a("0.1"), now);
        let h2 = b.reserve(&t, a("0.9"), now).await.unwrap();
        b.settle(h2, &t, a("0.9"), now);
        let e = b.reserve(&t, Amount::ZERO, now).await.unwrap_err();
        assert_eq!(e.kind, ExceededKind::Exhausted);

        // soft: Schätzung und Reservierungen zählen nicht, erst der Verbrauch.
        let mut soft = budget("1", BudgetPeriod::Day, &[], true);
        soft.enforcement = BudgetEnforcement::Soft;
        let ts = [BudgetTarget {
            level: BudgetLevel::Team,
            target: new_id(),
            budget: &soft,
        }];
        let h = b.reserve(&ts, a("5"), now).await.unwrap();
        assert!(h.is_empty());
        b.settle(h, &ts, a("1.2"), now);
        assert_eq!(
            b.reserve(&ts, a("0.01"), now).await.unwrap_err().kind,
            ExceededKind::Exhausted
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn strict_with_shared_counter_exact_across_instances() {
        let ledger = Arc::new(MemoryLedger::default());
        let shared = Arc::new(MemoryShared::default());
        let mk = || {
            Arc::new(
                Budgets::new(Calendar::utc(), Some(ledger.clone()))
                    .with_shared(shared.clone() as Arc<dyn SharedBudgets>),
            )
        };
        let (i1, i2) = (mk(), mk());
        let spec = Arc::new(budget("1", BudgetPeriod::Month, &[], true));
        let id = new_id();
        let (r1, r2) = tokio::join!(
            hammer(i1.clone(), spec.clone(), id, 16, 10, a("0.03"), a("0.03")),
            hammer(i2.clone(), spec.clone(), id, 16, 10, a("0.03"), a("0.03")),
        );
        assert_eq!(r1.0 + r2.0, 33, "{r1:?} {r2:?}");
        // Abgleich der Korrekturen im Hintergrund abwarten, dann Ledger schreiben.
        tokio::time::sleep(Duration::from_millis(50)).await;
        i1.flush().await.unwrap();
        i2.flush().await.unwrap();
        let t = BudgetTarget {
            level: BudgetLevel::Team,
            target: id,
            budget: &spec,
        };
        let key = i1.key(&t, now_ms()).0;
        assert_eq!(ledger.get(&key), a("0.99").nanos());
        assert_eq!(shared.get(&key), Some(a("0.99").nanos()));
    }

    /// Zeitpunkte der gebuchten Anfragen einer Instanz (4 Arbeiter, je Anfrage ≈ 1 ms).
    async fn load_with_times(
        b: Arc<Budgets>,
        spec: Arc<Budget>,
        target: Id,
        cost: Amount,
    ) -> Vec<std::time::Instant> {
        let times = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let (b, spec, times) = (Arc::clone(&b), Arc::clone(&spec), Arc::clone(&times));
            tasks.push(tokio::spawn(async move {
                for _ in 0..80 {
                    let t = [BudgetTarget {
                        level: BudgetLevel::Team,
                        target,
                        budget: &spec,
                    }];
                    if let Ok(hold) = b.reserve(&t, cost, now_ms()).await {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                        b.settle(hold, &t, cost, now_ms());
                        lock(&times).push(std::time::Instant::now());
                    } else {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                }
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        let mut v = lock(&times).clone();
        v.sort();
        v
    }

    /// Höchste Anzahl Buchungen in einem Fenster der Länge `w`.
    fn max_in_window(times: &[std::time::Instant], w: Duration) -> usize {
        let mut best = 0;
        let mut j = 0;
        for i in 0..times.len() {
            while times[i].duration_since(times[j]) > w {
                j += 1;
            }
            best = best.max(i + 1 - j);
        }
        best
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn strict_without_shared_overshoot_bounded_by_sync_period() {
        // Zwei Instanzen, nur Ledger (local/postgres-sync im Cluster): Eine Instanz sieht fremde
        // Buchungen erst, wenn die andere sie geschrieben (≤ 1 Intervall) und sie selbst sie
        // zurückgelesen hat (≤ 1 Intervall). Die Überschreitung ist daher höchstens die
        // Summe der Buchungen jeder Instanz in einem Fenster von zwei Abgleichintervallen.
        let ledger = Arc::new(MemoryLedger::default());
        let i1 = Arc::new(Budgets::new(Calendar::utc(), Some(ledger.clone())));
        let i2 = Arc::new(Budgets::new(Calendar::utc(), Some(ledger.clone())));
        let interval = Duration::from_millis(50);
        let s1 = i1.spawn_sync(interval);
        let s2 = i2.spawn_sync(interval);
        let spec = Arc::new(budget("1", BudgetPeriod::Month, &[], true));
        let id = new_id();
        let c = a("0.01");
        let (t1, t2) = tokio::join!(
            load_with_times(i1.clone(), spec.clone(), id, c),
            load_with_times(i2.clone(), spec.clone(), id, c),
        );
        s1.shutdown().await;
        s2.shutdown().await;
        let t = BudgetTarget {
            level: BudgetLevel::Team,
            target: id,
            budget: &spec,
        };
        let total = ledger.get(&i1.key(&t, now_ms()).0);
        let n = i64::try_from(t1.len() + t2.len()).unwrap();
        assert_eq!(total, n * c.nanos());
        assert!(total >= a("1").nanos() - c.nanos(), "{total}");
        // Zwei Intervalle plus Spielraum für Taktverschiebung/Planung.
        let w = interval * 2 + Duration::from_millis(30);
        let slack = i64::try_from(max_in_window(&t1, w) + max_in_window(&t2, w)).unwrap();
        let bound = a("1").nanos() + slack * c.nanos();
        assert!(total <= bound, "{total} > {bound}");
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
