// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Zu prüfende Limits einer Anfrage aus dem Snapshot.
//!
//! Zählerschlüssel `<ebene>:<id>:<metrik>[:<modellgruppe>]` (05 §1), z. B.
//! `team:0193…:tokens` oder `key:0193…:requests:gpt-4o` für ein Key-Limit, das nur für die
//! Modellgruppe `gpt-4o` gilt (kanonischer Name, auch wenn das Limit einen Alias nennt). Das
//! Fenster gehört nicht in den Schlüssel; dasselbe Limit in zwei Fenstern (RPM und RPD) sind
//! zwei Zähler.

use std::fmt::Write as _;

use aigw_core::resource::{Id, LimitMetric, LimitSpec};
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

pub(crate) fn window_label(w: Window) -> &'static str {
    match w {
        Window::Second => "Sekunde",
        Window::Minute => "Minute",
        Window::Hour => "Stunde",
        Window::Day => "Tag",
        Window::Month => "30 Tage",
    }
}

/// Ein geplantes Limit.
#[derive(Debug, Clone)]
pub(crate) struct Planned {
    pub(crate) key: String,
    pub(crate) window: Window,
    pub(crate) limit: u64,
    pub(crate) metric: Metric,
    pub(crate) level: Level,
}

fn push(
    out: &mut Vec<Planned>,
    level: Level,
    id: Id,
    limits: &[LimitSpec],
    group: Option<&ModelGroupCtx>,
) {
    for l in limits {
        let Some(metric) = Metric::from_spec(l.metric) else {
            continue; // `concurrency`: noch nicht unterstützt (siehe 11 §7)
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
        out.push(Planned {
            key,
            window: Window::from(l.window),
            limit: l.limit,
            metric,
            level,
        });
    }
}

/// Alle Limits für Key (samt Team und Organisation) und Modellgruppe, nach Metrik sortiert
/// (Anfragen, Tokens, Eingabe, Ausgabe; innerhalb stabil: Key, Team, Org, Modellgruppe).
pub(crate) fn plan(key: Option<&KeyCtx>, group: Option<&ModelGroupCtx>, out: &mut Vec<Planned>) {
    if let Some(k) = key {
        push(out, Level::Key, k.key.id, &k.key.spec.limits, group);
        if let Some(t) = &k.team {
            push(out, Level::Team, t.id, &t.spec.limits, group);
        }
        if let Some(o) = &k.organization {
            push(out, Level::Organization, o.id, &o.spec.limits, group);
        }
    }
    if let Some(g) = group {
        // Globale Limits der Gruppe; `modelGroup` ist hier bedeutungslos.
        for l in &g.group.spec.limits {
            let Some(metric) = Metric::from_spec(l.metric) else {
                continue;
            };
            out.push(Planned {
                key: format!("group:{}:{}", g.group.id, metric.code()),
                window: Window::from(l.window),
                limit: l.limit,
                metric,
                level: Level::ModelGroup,
            });
        }
    }
    out.sort_by_key(|p| p.metric);
}
