// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Zu prüfende Limits einer Anfrage aus dem Snapshot.
//!
//! Zählerschlüssel `<ebene>:<id>:<metrik>[:<modellgruppe>]` (05 §1), z. B.
//! `team:0193…:tokens` oder `key:0193…:requests:gpt-4o` für ein Key-Limit, das nur für die
//! Modellgruppe `gpt-4o` gilt (kanonischer Name, auch wenn das Limit einen Alias nennt). Das
//! gleitende Fenster gehört nicht in den Schlüssel; dasselbe Limit in zwei Fenstern (RPM und
//! RPD) sind zwei Zähler. Kalenderzeiträume (F25) tragen Art und Beginn im Schlüssel, z. B.
//! `key:0193…:tokens@calendar-month:1761951600000` – mit dem nächsten Zeitraum entsteht ein
//! neuer Zähler.
//!
//! Gleichzeitigkeits-Limits (`concurrency`, S-12) haben kein Fenster: Schlüssel
//! `<ebene>:<id>:concurrency[:<modellgruppe>]`, geplant mit [`plan_slots`].

use std::fmt::Write as _;
use std::sync::RwLock;

use aigw_billing::Calendar;
use aigw_core::resource::{BudgetPeriod, Id, LimitMetric, LimitSpec, LimitWindow};
use aigw_core::snapshot::{KeyCtx, ModelGroupCtx};
use aigw_counters::Window;

/// Gemessene Größe (ohne `concurrency`, das kein Zeitfenster hat).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Metric {
    /// Anfragen (RPM/RPD); nie korrigiert.
    Requests,
    /// Tokens gesamt (TPM/TPD).
    Tokens,
    /// Eingabe-Tokens.
    InputTokens,
    /// Ausgabe-Tokens.
    OutputTokens,
}

impl Metric {
    fn from_spec(m: LimitMetric) -> Option<Self> {
        Some(match m {
            LimitMetric::Requests => Self::Requests,
            LimitMetric::Tokens => Self::Tokens,
            LimitMetric::InputTokens => Self::InputTokens,
            LimitMetric::OutputTokens => Self::OutputTokens,
            LimitMetric::Concurrency => return None,
        })
    }

    /// Kürzel im Zählerschlüssel.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Requests => "requests",
            Self::Tokens => "tokens",
            Self::InputTokens => "input-tokens",
            Self::OutputTokens => "output-tokens",
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Requests => "Anfragen",
            Self::Tokens => "Tokens",
            Self::InputTokens => "Eingabe-Tokens",
            Self::OutputTokens => "Ausgabe-Tokens",
        }
    }
}

/// Ebene eines Limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Level {
    Key,
    Team,
    Organization,
    ModelGroup,
}

impl Level {
    const fn code(self) -> &'static str {
        match self {
            Self::Key => "key",
            Self::Team => "team",
            Self::Organization => "org",
            Self::ModelGroup => "group",
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Key => "API-Key",
            Self::Team => "Team",
            Self::Organization => "Organisation",
            Self::ModelGroup => "Modellgruppe",
        }
    }
}

pub(crate) fn window_label(w: LimitWindow) -> &'static str {
    match w {
        LimitWindow::Second => "Sekunde",
        LimitWindow::Minute => "Minute",
        LimitWindow::Hour => "Stunde",
        LimitWindow::Day => "Tag (24 h)",
        LimitWindow::Days30 => "30 Tage",
        LimitWindow::CalendarDay => "Kalendertag",
        LimitWindow::CalendarWeek => "Kalenderwoche",
        LimitWindow::CalendarMonth => "Kalendermonat",
    }
}

/// Kalenderzeiträume der Limits (F25) in der Zeitzone `billing.timezone`, mit Zwischenspeicher
/// des laufenden Zeitraums je Art (die Umrechnung läuft nur beim Wechsel).
#[derive(Debug)]
pub(crate) struct Periods {
    calendar: Calendar,
    cache: RwLock<[Option<(u64, u64)>; 3]>,
}

impl Default for Periods {
    fn default() -> Self {
        Self::new(Calendar::utc())
    }
}

impl Periods {
    pub(crate) fn new(calendar: Calendar) -> Self {
        Self {
            calendar,
            cache: RwLock::new([None; 3]),
        }
    }

    pub(crate) fn calendar(&self) -> &Calendar {
        &self.calendar
    }

    /// Fenster eines Limits zum Zeitpunkt `now_ms` (gleitend oder fester Zeitraum).
    pub(crate) fn window(&self, w: LimitWindow, now_ms: u64) -> Window {
        if let Some(s) = Window::sliding(w) {
            return s;
        }
        let (slot, period) = match w {
            LimitWindow::CalendarDay => (0, BudgetPeriod::Day),
            LimitWindow::CalendarWeek => (1, BudgetPeriod::Week),
            _ => (2, BudgetPeriod::Month),
        };
        if let Ok(c) = self.cache.read()
            && let Some((start, end)) = c[slot]
            && start <= now_ms
            && now_ms < end
        {
            return Window::period(start, end);
        }
        let r = self
            .calendar
            .period(period, i64::try_from(now_ms).unwrap_or(i64::MAX));
        let start = u64::try_from(r.start_ms).unwrap_or(0);
        let end = r
            .end_ms
            .and_then(|e| u64::try_from(e).ok())
            .unwrap_or(u64::MAX);
        if let Ok(mut c) = self.cache.write() {
            c[slot] = Some((start, end));
        }
        Window::period(start, end)
    }
}

/// Ein geplantes Limit.
#[derive(Debug, Clone)]
pub(crate) struct Planned {
    pub(crate) key: String,
    pub(crate) window: Window,
    /// Fenster laut Spezifikation (Meldungen).
    pub(crate) spec_window: LimitWindow,
    pub(crate) limit: u64,
    pub(crate) metric: Metric,
    pub(crate) level: Level,
}

/// Hängt bei Kalenderzeiträumen Art und Beginn an den Zählerschlüssel an.
fn period_suffix(key: &mut String, spec: LimitWindow, w: Window) {
    if let Window::Period { start_ms, .. } = w {
        let _ = write!(key, "@{}:{start_ms}", spec.as_str());
    }
}

fn push(
    out: &mut Vec<Planned>,
    level: Level,
    id: Id,
    limits: &[LimitSpec],
    group: Option<&ModelGroupCtx>,
    periods: &Periods,
    now_ms: u64,
) {
    for l in limits {
        let Some(metric) = Metric::from_spec(l.metric) else {
            continue; // `concurrency`: siehe [`plan_slots`]
        };
        let scoped = match (&l.model_group, group) {
            (None, _) => None,
            (Some(name), Some(g)) if g.answers_to(name) => Some(g.group.metadata.name.as_str()),
            // Limit für eine andere (oder ohne Snapshot unbekannte) Modellgruppe.
            (Some(_), _) => continue,
        };
        let mut key = String::with_capacity(64);
        let _ = write!(key, "{}:{id}:{}", level.code(), metric.code());
        if let Some(g) = scoped {
            key.push(':');
            key.push_str(g);
        }
        let window = periods.window(l.window, now_ms);
        period_suffix(&mut key, l.window, window);
        out.push(Planned {
            key,
            window,
            spec_window: l.window,
            limit: l.limit,
            metric,
            level,
        });
    }
}

/// Alle Limits für Key (samt Team und Organisation) und Modellgruppe, nach Metrik sortiert
/// (Anfragen, Tokens, Eingabe, Ausgabe; innerhalb stabil: Key, Team, Org, Modellgruppe).
pub(crate) fn plan(
    key: Option<&KeyCtx>,
    group: Option<&ModelGroupCtx>,
    periods: &Periods,
    now_ms: u64,
    out: &mut Vec<Planned>,
) {
    if let Some(k) = key {
        let p = (periods, now_ms);
        push(
            out,
            Level::Key,
            k.key.id,
            &k.key.spec.limits,
            group,
            p.0,
            p.1,
        );
        if let Some(t) = &k.team {
            push(out, Level::Team, t.id, &t.spec.limits, group, p.0, p.1);
        }
        if let Some(o) = &k.organization {
            push(
                out,
                Level::Organization,
                o.id,
                &o.spec.limits,
                group,
                p.0,
                p.1,
            );
        }
    }
    if let Some(g) = group {
        // Globale Limits der Gruppe; `modelGroup` ist hier bedeutungslos.
        for l in &g.group.spec.limits {
            let Some(metric) = Metric::from_spec(l.metric) else {
                continue;
            };
            let window = periods.window(l.window, now_ms);
            let mut key = format!("group:{}:{}", g.group.id, metric.code());
            period_suffix(&mut key, l.window, window);
            out.push(Planned {
                key,
                window,
                spec_window: l.window,
                limit: l.limit,
                metric,
                level: Level::ModelGroup,
            });
        }
    }
    out.sort_by_key(|p| p.metric);
}

/// Ein geplantes Gleichzeitigkeits-Limit.
#[derive(Debug, Clone)]
pub(crate) struct PlannedSlot {
    pub(crate) key: String,
    pub(crate) limit: u64,
    pub(crate) level: Level,
}

fn push_slots(
    out: &mut Vec<PlannedSlot>,
    level: Level,
    id: Id,
    limits: &[LimitSpec],
    group: Option<&ModelGroupCtx>,
    group_scoped: bool,
) {
    for l in limits
        .iter()
        .filter(|l| l.metric == LimitMetric::Concurrency)
    {
        let scoped = match (&l.model_group, group) {
            _ if !group_scoped => None,
            (None, _) => None,
            (Some(name), Some(g)) if g.answers_to(name) => Some(g.group.metadata.name.as_str()),
            (Some(_), _) => continue,
        };
        let mut key = String::with_capacity(64);
        let _ = write!(key, "{}:{id}:concurrency", level.code());
        if let Some(g) = scoped {
            key.push(':');
            key.push_str(g);
        }
        out.push(PlannedSlot {
            key,
            limit: l.limit,
            level,
        });
    }
}

/// Alle Gleichzeitigkeits-Limits für Key (samt Team und Organisation) und Modellgruppe
/// (Reihenfolge Key, Team, Org, Modellgruppe).
pub(crate) fn plan_slots(
    key: Option<&KeyCtx>,
    group: Option<&ModelGroupCtx>,
    out: &mut Vec<PlannedSlot>,
) {
    if let Some(k) = key {
        push_slots(out, Level::Key, k.key.id, &k.key.spec.limits, group, true);
        if let Some(t) = &k.team {
            push_slots(out, Level::Team, t.id, &t.spec.limits, group, true);
        }
        if let Some(o) = &k.organization {
            push_slots(out, Level::Organization, o.id, &o.spec.limits, group, true);
        }
    }
    if let Some(g) = group {
        // Globale Limits der Gruppe; `modelGroup` ist hier bedeutungslos.
        push_slots(
            out,
            Level::ModelGroup,
            g.group.id,
            &g.group.spec.limits,
            None,
            false,
        );
    }
}
