// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Grenzen entlang der Mandantenhierarchie (AIGW-67): **Key ≤ Team ≤ Organisation**.
//!
//! Gemeinsam für Admin-API und GitOps. Regeln (Details: `docs/konzept/11-limits-und-budgets.md`
//! §6a):
//!
//! - **Limits:** Ein Limit eines Kindes darf kein vergleichbares Limit einer übergeordneten
//!   Ebene überschreiten. Vergleichbar heißt: gleiche Metrik, gleiches Fenster (bei
//!   `concurrency` ohne Fenster) und das obere Limit gilt für die Modellgruppe des unteren
//!   (gleiche Gruppe oder oben ohne Gruppe). Verschiedene Fenster werden **nicht** umgerechnet.
//!   Fehlt oben ein vergleichbares Limit, gibt es keine Grenze; fehlt unten eins, gilt zur
//!   Laufzeit ohnehin das obere.
//! - **Budgets:** nur bei gleichem Zeitraum vergleichbar (`day`, `week`, `month`, `total`);
//!   Betrag unten ≤ Betrag oben. Es gibt eine Abrechnungswährung, Beträge sind direkt
//!   vergleichbar.
//! - **Modelle:** Schränkt eine übergeordnete Ebene `allowedModels` ein, muss jede Freigabe
//!   des Kindes darin enthalten sein (Namen und Aliasse werden auf den kanonischen
//!   Gruppennamen abgebildet). Eine leere Liste unten heißt „alles, was oben erlaubt ist“.
//!
//! Die Funktionen sind rein; Eltern und Modellnamen übergibt der Aufrufer.

use std::collections::HashMap;
use std::fmt;

use serde::Serialize;
use utoipa::ToSchema;

use crate::resource::{
    ApiKeySpec, Budget, BudgetPeriod, LimitMetric, LimitSpec, LimitWindow, ModelGroupSpec,
    OrganizationSpec, TeamSpec,
};

/// Ebene der Mandantenhierarchie.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Level {
    /// Organisation.
    Organization,
    /// Team.
    Team,
    /// API-Key.
    ApiKey,
}

impl Level {
    /// Deutscher Anzeigename.
    pub fn label(self) -> &'static str {
        match self {
            Self::Organization => "Organisation",
            Self::Team => "Team",
            Self::ApiKey => "Key",
        }
    }
}

/// Limits, Budget und Modellfreigaben eines Objekts.
#[derive(Debug, Clone, Copy)]
pub struct Node<'a> {
    /// Ebene.
    pub level: Level,
    /// Name.
    pub name: &'a str,
    /// Limits.
    pub limits: &'a [LimitSpec],
    /// Budget.
    pub budget: Option<&'a Budget>,
    /// Modellfreigaben (leer = keine Einschränkung durch diese Ebene).
    pub allowed_models: &'a [String],
}

impl<'a> Node<'a> {
    /// Organisation.
    pub fn organization(name: &'a str, spec: &'a OrganizationSpec) -> Self {
        Self {
            level: Level::Organization,
            name,
            limits: &spec.limits,
            budget: spec.budget.as_ref(),
            allowed_models: &[],
        }
    }

    /// Team.
    pub fn team(name: &'a str, spec: &'a TeamSpec) -> Self {
        Self {
            level: Level::Team,
            name,
            limits: &spec.limits,
            budget: spec.budget.as_ref(),
            allowed_models: &spec.allowed_models,
        }
    }

    /// API-Key.
    pub fn api_key(name: &'a str, spec: &'a ApiKeySpec) -> Self {
        Self {
            level: Level::ApiKey,
            name,
            limits: &spec.limits,
            budget: spec.budget.as_ref(),
            allowed_models: &spec.allowed_models,
        }
    }

    fn describe(&self) -> String {
        format!("{} {:?}", self.level.label(), self.name)
    }
}

/// Abbildung Modellname/Alias → kanonischer Gruppenname.
#[derive(Debug, Clone, Default)]
pub struct ModelNames(HashMap<String, String>);

impl ModelNames {
    /// Aus allen Modellgruppen (Name, Spezifikation).
    pub fn new<'a>(groups: impl IntoIterator<Item = (&'a str, &'a ModelGroupSpec)>) -> Self {
        let mut map = HashMap::new();
        for (name, g) in groups {
            for a in &g.aliases {
                map.insert(a.clone(), name.to_owned());
            }
            map.insert(name.to_owned(), name.to_owned());
        }
        Self(map)
    }

    /// Kanonischer Name (Unbekanntes bleibt, wie es ist).
    pub fn canonical<'s>(&'s self, model: &'s str) -> &'s str {
        self.0.get(model).map_or(model, String::as_str)
    }
}

/// Ein Verstoß gegen eine Grenze.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Violation {
    /// Ebene des betroffenen Objekts.
    pub level: Level,
    /// Name des betroffenen Objekts.
    pub name: String,
    /// Feld, z. B. `limits[0]`, `budget`, `allowedModels`.
    pub field: String,
    /// Verständliche Meldung.
    pub message: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {:?}: {}: {}",
            self.level.label(),
            self.name,
            self.field,
            self.message
        )
    }
}

/// Meldungen als eine Zeile (`; `-getrennt), z. B. für `Problem.detail`.
pub fn summary(violations: &[Violation]) -> String {
    violations
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

fn metric_str(m: LimitMetric) -> &'static str {
    match m {
        LimitMetric::Requests => "requests",
        LimitMetric::Tokens => "tokens",
        LimitMetric::InputTokens => "input-tokens",
        LimitMetric::OutputTokens => "output-tokens",
        LimitMetric::Concurrency => "concurrency",
    }
}

fn window_str(w: LimitWindow) -> &'static str {
    w.as_str()
}

/// Hinweis zu einem Limit, das mit dem gewählten Zähler-Backend nur ungenau wirkt (F29).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LimitWarning {
    /// Art des Objekts (`organization`, `team`, `api-key`, `model-group`).
    pub kind: String,
    /// Name des Objekts.
    pub name: String,
    /// Feld, z. B. `limits[0]`.
    pub field: String,
    /// Kennung des Hinweises (`second-limit-postgres-sync`).
    pub code: String,
    /// Verständliche Meldung.
    pub message: String,
}

/// Hinweise zu Sekundenlimits bei `counters.backend: postgres-sync` (F29): Die Instanzen
/// gleichen nur im Abstand `sync_interval_ms` ab, im Cluster kann ein Sekundenlimit daher
/// mehrfach ausgeschöpft werden. `objects`: (Art, Name, Limits).
pub fn limit_warnings<'a>(
    backend: crate::config::CounterBackendKind,
    objects: impl IntoIterator<Item = (&'a str, &'a str, &'a [LimitSpec])>,
) -> Vec<LimitWarning> {
    if backend != crate::config::CounterBackendKind::PostgresSync {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (kind, name, limits) in objects {
        for (i, l) in limits.iter().enumerate() {
            if l.window == LimitWindow::Second && l.metric != LimitMetric::Concurrency {
                out.push(LimitWarning {
                    kind: kind.to_owned(),
                    name: name.to_owned(),
                    field: format!("limits[{i}]"),
                    code: "second-limit-postgres-sync".to_owned(),
                    message: format!(
                        "{} je Sekunde mit counters.backend = postgres-sync: Instanzen gleichen \
                         nur alle sync_interval_ms ab – im Cluster kann das Limit um ein \
                         Mehrfaches überschritten werden (redis verwenden oder ein längeres \
                         Fenster wählen)",
                        metric_str(l.metric)
                    ),
                });
            }
        }
    }
    out
}

fn period_str(p: BudgetPeriod) -> &'static str {
    match p {
        BudgetPeriod::Day => "day",
        BudgetPeriod::Week => "week",
        BudgetPeriod::Month => "month",
        BudgetPeriod::Total => "total",
    }
}

/// Kurzbeschreibung eines Limits, z. B. `tokens/minute (Modellgruppe "gpt-4o")`.
fn limit_label(l: &LimitSpec) -> String {
    let mut s = metric_str(l.metric).to_owned();
    if l.metric != LimitMetric::Concurrency {
        s.push('/');
        s.push_str(window_str(l.window));
    }
    if let Some(g) = &l.model_group {
        s.push_str(&format!(" (Modellgruppe {g:?})"));
    }
    s
}

/// `true`, wenn das obere Limit `upper` das untere `lower` begrenzt.
fn bounds(upper: &LimitSpec, lower: &LimitSpec, models: &ModelNames) -> bool {
    upper.metric == lower.metric
        && (upper.metric == LimitMetric::Concurrency || upper.window == lower.window)
        && match (&upper.model_group, &lower.model_group) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(u), Some(l)) => models.canonical(u) == models.canonical(l),
        }
}

/// Engstes vergleichbares Limit aller Eltern: (Wert, Elternteil).
fn tightest_limit<'a>(
    lower: &LimitSpec,
    parents: &[Node<'a>],
    models: &ModelNames,
) -> Option<(u64, Node<'a>)> {
    parents
        .iter()
        .flat_map(|p| p.limits.iter().map(move |l| (l, p)))
        .filter(|(u, _)| bounds(u, lower, models))
        .map(|(u, p)| (u.limit, *p))
        .min_by_key(|(v, _)| *v)
}

/// Engstes Budget gleichen Zeitraums aller Eltern.
fn tightest_budget<'a>(lower: &Budget, parents: &[Node<'a>]) -> Option<(&'a Budget, Node<'a>)> {
    parents
        .iter()
        .filter_map(|p| p.budget.map(|b| (b, *p)))
        .filter(|(b, _)| b.period == lower.period)
        .min_by_key(|(b, _)| b.amount)
}

/// Erster Elternteil mit Modelleinschränkung, der `model` nicht erlaubt.
fn forbidding_parent<'a>(
    model: &str,
    parents: &[Node<'a>],
    models: &ModelNames,
) -> Option<Node<'a>> {
    let m = models.canonical(model);
    parents.iter().copied().find(|p| {
        !p.allowed_models.is_empty() && !p.allowed_models.iter().any(|a| models.canonical(a) == m)
    })
}

/// Prüft `node` gegen alle übergeordneten Ebenen `parents` (z. B. Key gegen Team und
/// Organisation) und liefert alle Verstöße.
pub fn check(node: &Node<'_>, parents: &[Node<'_>], models: &ModelNames) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut push = |field: String, message: String| {
        out.push(Violation {
            level: node.level,
            name: node.name.to_owned(),
            field,
            message,
        });
    };
    for (i, l) in node.limits.iter().enumerate() {
        if let Some((max, p)) = tightest_limit(l, parents, models)
            && l.limit > max
        {
            push(
                format!("limits[{i}]"),
                format!(
                    "{} = {} überschreitet das Limit von {} ({max})",
                    limit_label(l),
                    l.limit,
                    p.describe()
                ),
            );
        }
    }
    if let Some(b) = node.budget
        && let Some((max, p)) = tightest_budget(b, parents)
        && b.amount > max.amount
    {
        push(
            "budget".into(),
            format!(
                "{} je {} überschreitet das Budget von {} ({} je {})",
                b.amount,
                period_str(b.period),
                p.describe(),
                max.amount,
                period_str(max.period)
            ),
        );
    }
    for (i, m) in node.allowed_models.iter().enumerate() {
        if let Some(p) = forbidding_parent(m, parents, models) {
            push(
                format!("allowedModels[{i}]"),
                format!("Modell {m:?} ist für {} nicht freigegeben", p.describe()),
            );
        }
    }
    out
}

/// Ergebnis von [`clamp`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Clamp {
    /// Nichts zu tun.
    Unchanged,
    /// Angepasst.
    Changed,
    /// Nicht automatisch anpassbar (Grund), z. B. weil die Modellliste leer würde und
    /// „leer“ „alles, was das Team erlaubt“ bedeutet – das wäre eine Erweiterung.
    Impossible(String),
}

/// Setzt Limits und Budget von `limits`/`budget`/`allowed_models` (eines Objekts der Ebene
/// unterhalb von `parents`) auf die Grenzen der Eltern herab und entfernt nicht freigegebene
/// Modelle. Ändert bei [`Clamp::Impossible`] nichts.
pub fn clamp(
    limits: &mut [LimitSpec],
    budget: &mut Option<Budget>,
    allowed_models: &mut Vec<String>,
    parents: &[Node<'_>],
    models: &ModelNames,
) -> Clamp {
    let kept: Vec<String> = allowed_models
        .iter()
        .filter(|m| forbidding_parent(m, parents, models).is_none())
        .cloned()
        .collect();
    if kept.is_empty() && !allowed_models.is_empty() {
        return Clamp::Impossible(
            "keine der Modellfreigaben ist oben erlaubt (leere Liste hieße „alle“)".into(),
        );
    }
    let mut changed = kept.len() != allowed_models.len();
    *allowed_models = kept;
    for l in limits.iter_mut() {
        if let Some((max, _)) = tightest_limit(l, parents, models)
            && l.limit > max
        {
            l.limit = max;
            changed = true;
        }
    }
    if let Some(b) = budget.as_mut()
        && let Some((max, _)) = tightest_budget(b, parents)
        && b.amount > max.amount
    {
        b.amount = max.amount;
        changed = true;
    }
    if changed {
        Clamp::Changed
    } else {
        Clamp::Unchanged
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::Amount;

    #[test]
    fn second_limits_warn_only_with_postgres_sync() {
        use crate::config::CounterBackendKind as K;
        let limits = [
            lim(Requests, 5, LimitWindow::Second, None),
            lim(Tokens, 500, Minute, None),
            lim(Concurrency, 2, LimitWindow::Second, None),
        ];
        let objs = || [("team", "t1", &limits[..])];
        assert!(limit_warnings(K::Local, objs()).is_empty());
        assert!(limit_warnings(K::Redis, objs()).is_empty());
        let w = limit_warnings(K::PostgresSync, objs());
        assert_eq!(w.len(), 1);
        assert_eq!(
            (w[0].kind.as_str(), w[0].name.as_str(), w[0].field.as_str()),
            ("team", "t1", "limits[0]")
        );
        assert_eq!(w[0].code, "second-limit-postgres-sync");
        assert!(w[0].message.contains("redis"));
    }

    #[test]
    fn limit_window_names() {
        for w in LimitWindow::ALL {
            let json = serde_json::to_value(w).unwrap();
            assert_eq!(json, w.as_str());
            let back: LimitWindow = serde_json::from_value(json).unwrap();
            assert_eq!(back, w);
        }
        assert!(LimitWindow::CalendarWeek.is_calendar() && !LimitWindow::Days30.is_calendar());
        assert!(serde_json::from_value::<LimitWindow>(serde_json::json!("month")).is_err());
    }

    fn lim(metric: LimitMetric, limit: u64, window: LimitWindow, group: Option<&str>) -> LimitSpec {
        LimitSpec {
            metric,
            limit,
            window,
            model_group: group.map(str::to_owned),
        }
    }

    fn budget(amount: &str, period: BudgetPeriod) -> Budget {
        Budget {
            amount: amount.parse().unwrap_or(Amount::ZERO),
            period,
            warn_at_percent: vec![],
            hard_limit: true,
            enforcement: Default::default(),
        }
    }

    fn models() -> ModelNames {
        let g = ModelGroupSpec {
            aliases: vec!["gpt-4o-latest".into()],
            ..ModelGroupSpec::default()
        };
        let c = ModelGroupSpec::default();
        ModelNames::new([("gpt-4o", &g), ("claude", &c)])
    }

    use LimitMetric::{Concurrency, InputTokens, Requests, Tokens};
    use LimitWindow::{Day, Hour, Minute};

    #[test]
    fn limits_same_metric_and_window() {
        let m = models();
        let org = OrganizationSpec {
            limits: vec![lim(Requests, 1000, Minute, None)],
            ..OrganizationSpec::default()
        };
        let team = TeamSpec {
            organization: "acme".into(),
            limits: vec![
                lim(Tokens, 5000, Minute, None),
                lim(Tokens, 100, Minute, Some("gpt-4o")),
            ],
            ..TeamSpec::default()
        };
        let parents = [Node::team("t", &team), Node::organization("acme", &org)];
        let key = ApiKeySpec {
            limits: vec![
                lim(Tokens, 5000, Minute, None),                 // gleich: ok
                lim(Tokens, 6000, Minute, None),                 // > Team
                lim(Tokens, 6000, Hour, None),                   // anderes Fenster: kein Vergleich
                lim(InputTokens, 9999, Minute, None),            // andere Metrik
                lim(Tokens, 200, Minute, Some("gpt-4o-latest")), // Alias → Gruppenlimit 100
                lim(Tokens, 4000, Minute, Some("claude")),       // Team gesamt 5000: ok
                lim(Requests, 2000, Minute, None),               // > Org
                lim(Concurrency, 5, Day, None),                  // oben kein Limit
            ],
            ..ApiKeySpec::default()
        };
        let v = check(&Node::api_key("k", &key), &parents, &m);
        let fields: Vec<&str> = v.iter().map(|v| v.field.as_str()).collect();
        assert_eq!(fields, ["limits[1]", "limits[4]", "limits[6]"], "{v:?}");
        assert!(
            v[0].message.contains("Team \"t\" (5000)"),
            "{}",
            v[0].message
        );
        assert!(v[1].message.contains("(100)"), "{}", v[1].message);
        assert!(v[2].message.contains("Organisation \"acme\" (1000)"));
        assert!(
            v[0].to_string()
                .starts_with("Key \"k\": limits[1]: tokens/minute = 6000")
        );
    }

    #[test]
    fn group_limit_below_does_not_bound_global_limit() {
        let m = models();
        let team = TeamSpec {
            limits: vec![lim(Tokens, 100, Minute, Some("gpt-4o"))],
            ..TeamSpec::default()
        };
        let key = ApiKeySpec {
            limits: vec![lim(Tokens, 1000, Minute, None)],
            ..ApiKeySpec::default()
        };
        assert!(check(&Node::api_key("k", &key), &[Node::team("t", &team)], &m).is_empty());
    }

    #[test]
    fn concurrency_ignores_window() {
        let m = models();
        let team = TeamSpec {
            limits: vec![lim(Concurrency, 10, Minute, None)],
            ..TeamSpec::default()
        };
        let key = ApiKeySpec {
            limits: vec![lim(Concurrency, 11, Day, None)],
            ..ApiKeySpec::default()
        };
        assert_eq!(
            check(&Node::api_key("k", &key), &[Node::team("t", &team)], &m).len(),
            1
        );
    }

    #[test]
    fn budgets_only_same_period() {
        let m = models();
        let org = OrganizationSpec {
            budget: Some(budget("1000", BudgetPeriod::Month)),
            ..OrganizationSpec::default()
        };
        let team = TeamSpec {
            budget: Some(budget("50", BudgetPeriod::Day)),
            ..TeamSpec::default()
        };
        let parents = [Node::team("t", &team), Node::organization("o", &org)];
        let mut key = ApiKeySpec {
            budget: Some(budget("1000", BudgetPeriod::Month)),
            ..ApiKeySpec::default()
        };
        assert!(check(&Node::api_key("k", &key), &parents, &m).is_empty());
        key.budget = Some(budget("1000.000000001", BudgetPeriod::Month));
        let v = check(&Node::api_key("k", &key), &parents, &m);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].field, "budget");
        assert!(
            v[0].message.contains("Organisation \"o\" (1000 je month)"),
            "{}",
            v[0].message
        );
        // Woche gegen Monat/Tag: nicht vergleichbar
        key.budget = Some(budget("99999", BudgetPeriod::Week));
        assert!(check(&Node::api_key("k", &key), &parents, &m).is_empty());
        key.budget = Some(budget("51", BudgetPeriod::Day));
        assert_eq!(check(&Node::api_key("k", &key), &parents, &m).len(), 1);
    }

    #[test]
    fn models_subset_with_aliases() {
        let m = models();
        let team = TeamSpec {
            allowed_models: vec!["gpt-4o-latest".into()],
            ..TeamSpec::default()
        };
        let parents = [Node::team("t", &team)];
        let mut key = ApiKeySpec::default();
        assert!(check(&Node::api_key("k", &key), &parents, &m).is_empty());
        key.allowed_models = vec!["gpt-4o".into()];
        assert!(check(&Node::api_key("k", &key), &parents, &m).is_empty());
        key.allowed_models = vec!["gpt-4o".into(), "claude".into()];
        let v = check(&Node::api_key("k", &key), &parents, &m);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].field, "allowedModels[1]");
        // Team ohne Einschränkung
        let open = TeamSpec::default();
        assert!(check(&Node::api_key("k", &key), &[Node::team("t", &open)], &m).is_empty());
    }

    #[test]
    fn clamp_to_parents() {
        let m = models();
        let team = TeamSpec {
            allowed_models: vec!["gpt-4o".into()],
            limits: vec![lim(Tokens, 100, Minute, None)],
            budget: Some(budget("10", BudgetPeriod::Month)),
            ..TeamSpec::default()
        };
        let parents = [Node::team("t", &team)];
        let mut key = ApiKeySpec {
            allowed_models: vec!["gpt-4o-latest".into(), "claude".into()],
            limits: vec![lim(Tokens, 500, Minute, None), lim(Tokens, 50, Day, None)],
            budget: Some(budget("20", BudgetPeriod::Month)),
            ..ApiKeySpec::default()
        };
        let r = clamp(
            &mut key.limits,
            &mut key.budget,
            &mut key.allowed_models,
            &parents,
            &m,
        );
        assert_eq!(r, Clamp::Changed);
        assert_eq!(key.allowed_models, ["gpt-4o-latest"]);
        assert_eq!(key.limits[0].limit, 100);
        assert_eq!(key.limits[1].limit, 50);
        assert_eq!(
            key.budget.as_ref().map(|b| b.amount.to_string()).as_deref(),
            Some("10")
        );
        assert!(check(&Node::api_key("k", &key), &parents, &m).is_empty());
        let r = clamp(
            &mut key.limits,
            &mut key.budget,
            &mut key.allowed_models,
            &parents,
            &m,
        );
        assert_eq!(r, Clamp::Unchanged);
        // Nur nicht freigegebene Modelle → nicht automatisch anpassbar, nichts geändert
        key.allowed_models = vec!["claude".into()];
        key.limits[0].limit = 999;
        let r = clamp(
            &mut key.limits,
            &mut key.budget,
            &mut key.allowed_models,
            &parents,
            &m,
        );
        assert!(matches!(r, Clamp::Impossible(_)));
        assert_eq!(key.allowed_models, ["claude"]);
        assert_eq!(key.limits[0].limit, 999);
    }
}
