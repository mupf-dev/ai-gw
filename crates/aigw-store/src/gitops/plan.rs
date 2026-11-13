// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Validierung und Soll/Ist-Vergleich.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use aigw_core::resource::{
    LimitSpec, Manifest, ModelGroupSpec, ResourceKind, ResourceSpec, Scope, ScopeLevel,
    SecretString,
};
use aigw_core::types::ProviderKind;
use aigw_core::validate;
use serde::Serialize;
use serde_json::Value;
use utoipa::ToSchema;

use super::document::Document;
use super::secrets::{SecretSource, resolve};
use super::state::{Current, Extra, State};
use super::{ApiKeyMode, ApplyOptions, GitopsError, Issue};
use crate::keys::{hash_api_key, parse_prefix};
use crate::repo::level_kind;

/// Art einer geplanten Änderung.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ChangeAction {
    /// Anlegen (nur im Manifest).
    Create,
    /// Ändern (Manifest weicht ab).
    Update,
    /// Löschen (nicht im Manifest, nur mit `prune`).
    Delete,
    /// Unverändert.
    Unchanged,
    /// Nicht im Manifest, bleibt bestehen (ohne `prune`).
    Retain,
    /// Im Manifest, wird aber nicht angewendet (z. B. neuer API-Key ohne `spec.secret`).
    Skip,
}

impl ChangeAction {
    fn symbol(self) -> &'static str {
        match self {
            Self::Create => "+",
            Self::Update => "~",
            Self::Delete => "-",
            Self::Unchanged => "=",
            Self::Retain => "·",
            Self::Skip => "!",
        }
    }

    fn verb(self) -> &'static str {
        match self {
            Self::Create => "anlegen",
            Self::Update => "ändern",
            Self::Delete => "löschen",
            Self::Unchanged => "unverändert",
            Self::Retain => "nicht im Manifest, bleibt",
            Self::Skip => "übersprungen",
        }
    }
}

/// Geändertes Feld (Pfad in Punktnotation, z. B. `spec.budget.amount`).
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FieldChange {
    /// Pfad.
    pub path: String,
    /// Wert vorher (fehlt bei neuem Feld oder Geheimnis).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub before: Option<Value>,
    /// Wert nachher (fehlt bei entferntem Feld oder Geheimnis).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub after: Option<Value>,
    /// Geheimer Wert: nur „geändert“, nie der Inhalt.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub secret: bool,
}

/// Eine Ressource im Plan.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    /// Art.
    pub kind: ResourceKind,
    /// Name.
    pub name: String,
    /// Aktion.
    pub action: ChangeAction,
    /// Geänderte Felder (bei `update`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<FieldChange>,
    /// Hinweis (z. B. Grund für `skip`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Anzahl je Aktion.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, ToSchema)]
pub struct Summary {
    /// Anzulegen.
    pub create: usize,
    /// Zu ändern.
    pub update: usize,
    /// Zu löschen.
    pub delete: usize,
    /// Unverändert.
    pub unchanged: usize,
    /// Nicht im Manifest, bleiben.
    pub retain: usize,
    /// Übersprungen.
    pub skip: usize,
}

/// Ergebnis des Soll/Ist-Vergleichs.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    /// `config_version`, gegen die geplant wurde.
    pub base_version: i64,
    /// Mit `prune` geplant.
    pub prune: bool,
    /// Umgang mit API-Keys.
    pub api_keys: ApiKeyMode,
    /// Alle Ressourcen (Abhängigkeitsreihenfolge, je Art nach Name).
    pub changes: Vec<Change>,
    /// Anzahl je Aktion.
    pub summary: Summary,
    /// Warnungen (z. B. übersprungene Keys).
    pub warnings: Vec<String>,
}

impl Plan {
    /// Gibt es etwas zu schreiben?
    pub fn has_changes(&self) -> bool {
        self.summary.create + self.summary.update + self.summary.delete > 0
    }

    /// Lesbare Textform (`verbose`: auch unveränderte und behaltene Ressourcen einzeln).
    pub fn to_text(&self, verbose: bool) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "Plan gegen config_version {} (prune: {}, API-Keys: {})",
            self.base_version,
            if self.prune { "ja" } else { "nein" },
            match self.api_keys {
                ApiKeyMode::Include => "include",
                ApiKeyMode::Ignore => "ignore",
            }
        );
        for c in &self.changes {
            let quiet = matches!(c.action, ChangeAction::Unchanged | ChangeAction::Retain);
            if quiet && !verbose {
                continue;
            }
            let _ = write!(
                out,
                "  {} {} {:?} {}",
                c.action.symbol(),
                c.kind,
                c.name,
                c.action.verb()
            );
            if let Some(n) = &c.note {
                let _ = write!(out, ": {n}");
            }
            out.push('\n');
            for f in &c.fields {
                let v = |x: &Option<Value>| {
                    x.as_ref().map_or_else(|| "–".to_owned(), |v| v.to_string())
                };
                if f.secret {
                    let _ = writeln!(out, "      {}: (Geheimnis geändert)", f.path);
                } else {
                    let _ = writeln!(out, "      {}: {} → {}", f.path, v(&f.before), v(&f.after));
                }
            }
        }
        for w in &self.warnings {
            let _ = writeln!(out, "Warnung: {w}");
        }
        let s = &self.summary;
        let _ = write!(
            out,
            "Zusammenfassung: {} anlegen, {} ändern, {} löschen, {} unverändert",
            s.create, s.update, s.delete, s.unchanged
        );
        if s.retain > 0 {
            let _ = write!(
                out,
                ", {} nicht im Manifest (bleiben; --prune löscht sie)",
                s.retain
            );
        }
        if s.skip > 0 {
            let _ = write!(out, ", {} übersprungen", s.skip);
        }
        out.push('\n');
        out
    }
}

/// Aufgelöste Geheimnisse eines Dokuments.
#[derive(Debug, Clone)]
pub(crate) enum Resolved {
    None,
    Credential(BTreeMap<String, SecretString>),
    /// Vorgegebener Key-Klartext (falls `spec.secret`).
    ApiKey(Option<SecretString>),
}

/// Ein Schreibschritt.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum Step {
    Upsert {
        manifest: Manifest,
        existing: Option<Current>,
        resolved: Resolved,
    },
    Delete {
        kind: ResourceKind,
        name: String,
        current: Current,
    },
}

/// Plan plus Schreibschritte in Ausführungsreihenfolge.
#[derive(Debug)]
pub(crate) struct Prepared {
    pub(crate) plan: Plan,
    pub(crate) steps: Vec<Step>,
}

/// Endzustand nach dem Anwenden (für die Verweisprüfung).
struct Final<'a> {
    objects: BTreeMap<ResourceKind, BTreeMap<String, (Option<usize>, &'a Manifest)>>,
    /// Namen und Aliasse aller Modellgruppen.
    models: BTreeSet<String>,
}

impl Final<'_> {
    fn has(&self, kind: ResourceKind, name: &str) -> bool {
        self.objects
            .get(&kind)
            .is_some_and(|m| m.contains_key(name))
    }
}

fn retains(kind: ResourceKind, opts: &ApplyOptions) -> bool {
    !opts.prune || (kind == ResourceKind::ApiKey && opts.api_keys == ApiKeyMode::Ignore)
}

/// Prüft alles (Verweise, Geheimnisse, Eindeutigkeit) und berechnet Plan und Schritte.
pub(crate) fn prepare(
    state: &State,
    docs: &[Document],
    opts: &ApplyOptions,
    src: &dyn SecretSource,
) -> Result<Prepared, GitopsError> {
    let mut issues = Vec::new();
    let mut warnings = Vec::new();

    // Soll je Art (doppelte Einträge sind bereits beim Parsen abgelehnt; hier zur Sicherheit).
    let mut desired: BTreeMap<ResourceKind, BTreeMap<String, &Document>> = BTreeMap::new();
    for d in docs {
        if desired
            .entry(d.kind())
            .or_default()
            .insert(d.name().to_owned(), d)
            .is_some()
        {
            issues.push(Issue::at(Some(d.index), d.kind(), d.name(), "doppelt"));
        }
    }

    // Geheimnisse auflösen, übersprungene Keys bestimmen.
    let mut resolved: BTreeMap<(ResourceKind, String), Resolved> = BTreeMap::new();
    let mut skipped: BTreeMap<(ResourceKind, String), String> = BTreeMap::new();
    for d in docs {
        let key = (d.kind(), d.name().to_owned());
        let issue = |m: String| Issue::at(Some(d.index), d.kind(), d.name(), m);
        match &d.manifest.spec {
            ResourceSpec::ProviderCredential(c) => {
                let mut values = BTreeMap::new();
                for (field, r) in &c.data {
                    match resolve(src, &opts.secrets, r) {
                        Ok(v) => {
                            values.insert(field.clone(), v);
                        }
                        Err(m) => issues.push(issue(format!("Feld {field:?}: {m}"))),
                    }
                }
                resolved.insert(key, Resolved::Credential(values));
            }
            ResourceSpec::ApiKey(_) => {
                if opts.api_keys == ApiKeyMode::Ignore {
                    skipped.insert(key, "API-Keys werden ignoriert (--api-keys ignore)".into());
                    continue;
                }
                match &d.api_key_secret {
                    Some(r) => match resolve(src, &opts.secrets, r) {
                        Ok(v) if parse_prefix(v.expose()).is_some() => {
                            resolved.insert(key, Resolved::ApiKey(Some(v)));
                        }
                        Ok(_) => issues.push(issue(
                            "spec.secret: kein gültiger Key (Format sk-aigw-<12 Zeichen>-<43 Zeichen>)"
                                .into(),
                        )),
                        Err(m) => issues.push(issue(format!("spec.secret: {m}"))),
                    },
                    None if state.get(ResourceKind::ApiKey, d.name()).is_none() => {
                        skipped.insert(
                            key,
                            "neuer Key ohne spec.secret.valueFrom – wird nicht angelegt \
                             (Klartext wäre nirgends abrufbar)"
                                .into(),
                        );
                    }
                    None => {
                        resolved.insert(key, Resolved::ApiKey(None));
                    }
                }
            }
            _ => {
                resolved.insert(key, Resolved::None);
            }
        }
    }
    for ((kind, name), why) in &skipped {
        warnings.push(format!("{kind} {name:?} übersprungen: {why}"));
    }

    // Endzustand.
    let mut fin = Final {
        objects: BTreeMap::new(),
        models: BTreeSet::new(),
    };
    for kind in ResourceKind::ALL {
        let slot = fin.objects.entry(kind).or_default();
        if retains(kind, opts) {
            for (name, cur) in state.of(kind) {
                slot.insert(name.clone(), (None, &cur.manifest));
            }
        }
        for (name, d) in desired.get(&kind).into_iter().flatten() {
            if !skipped.contains_key(&(kind, name.clone())) {
                slot.insert(name.clone(), (Some(d.index), &d.manifest));
            }
        }
    }
    check_unique(&mut fin, state, &resolved, &mut issues);
    for (kind, objs) in &fin.objects {
        for (name, (doc, m)) in objs {
            check_refs((*kind, name, *doc), m, &fin, state, &mut issues);
        }
    }
    check_rules(&fin, state, opts, &resolved, &mut issues);

    if !issues.is_empty() {
        return Err(GitopsError::Invalid(issues));
    }

    // Soll/Ist-Vergleich.
    let mut changes = Vec::new();
    let mut upserts = Vec::new();
    let mut deletes = Vec::new();
    for kind in ResourceKind::ALL {
        let want = desired.get(&kind);
        let names: BTreeSet<&String> = want
            .into_iter()
            .flat_map(|m| m.keys())
            .chain(state.of(kind).map(|(n, _)| n))
            .collect();
        for name in names {
            let cur = state.get(kind, name);
            let doc = want.and_then(|m| m.get(name));
            let mut change = Change {
                kind,
                name: name.clone(),
                action: ChangeAction::Unchanged,
                fields: Vec::new(),
                note: None,
            };
            match (doc, cur) {
                (Some(d), cur) => {
                    let key = (kind, name.clone());
                    if let Some(why) = skipped.get(&key) {
                        // Übersprungen werden nur neue Keys bzw. alle Keys bei `ignore` –
                        // in beiden Fällen bleibt der Ist-Zustand, wie er ist.
                        change.action = ChangeAction::Skip;
                        change.note = Some(why.clone());
                    } else {
                        let res = resolved.remove(&key).unwrap_or(Resolved::None);
                        match cur {
                            None => change.action = ChangeAction::Create,
                            Some(c) => {
                                diff(c, &d.manifest, &res, &mut change.fields)?;
                                if !change.fields.is_empty() {
                                    change.action = ChangeAction::Update;
                                }
                            }
                        }
                        if matches!(change.action, ChangeAction::Create | ChangeAction::Update) {
                            upserts.push(Step::Upsert {
                                manifest: d.manifest.clone(),
                                existing: cur.cloned(),
                                resolved: res,
                            });
                        }
                    }
                }
                (None, Some(c)) => {
                    if retains(kind, opts) {
                        change.action = ChangeAction::Retain;
                    } else {
                        change.action = ChangeAction::Delete;
                    }
                    if change.action == ChangeAction::Delete {
                        deletes.push(Step::Delete {
                            kind,
                            name: name.clone(),
                            current: c.clone(),
                        });
                    }
                }
                (None, None) => continue,
            }
            changes.push(change);
        }
    }
    // Löschen in umgekehrter Abhängigkeitsreihenfolge, nach allen Anlagen/Änderungen.
    deletes.reverse();
    let mut summary = Summary::default();
    for c in &changes {
        match c.action {
            ChangeAction::Create => summary.create += 1,
            ChangeAction::Update => summary.update += 1,
            ChangeAction::Delete => summary.delete += 1,
            ChangeAction::Unchanged => summary.unchanged += 1,
            ChangeAction::Retain => summary.retain += 1,
            ChangeAction::Skip => summary.skip += 1,
        }
    }
    upserts.extend(deletes);
    Ok(Prepared {
        plan: Plan {
            base_version: state.version,
            prune: opts.prune,
            api_keys: opts.api_keys,
            changes,
            summary,
            warnings,
        },
        steps: upserts,
    })
}

/// Rekursiver Vergleich zweier JSON-Werte; Objekte feldweise, alles andere als Ganzes.
fn diff_json(path: &str, a: &Value, b: &Value, out: &mut Vec<FieldChange>) {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let keys: BTreeSet<&String> = x.keys().chain(y.keys()).collect();
            for k in keys {
                diff_json(
                    &format!("{path}.{k}"),
                    x.get(k).unwrap_or(&Value::Null),
                    y.get(k).unwrap_or(&Value::Null),
                    out,
                );
            }
        }
        _ if a == b => {}
        _ => out.push(FieldChange {
            path: path.to_owned(),
            before: (!a.is_null()).then(|| a.clone()),
            after: (!b.is_null()).then(|| b.clone()),
            secret: false,
        }),
    }
}

fn diff(
    cur: &Current,
    want: &Manifest,
    res: &Resolved,
    out: &mut Vec<FieldChange>,
) -> Result<(), crate::StoreError> {
    let a = serde_json::to_value(&cur.manifest)?;
    let b = serde_json::to_value(want)?;
    let empty = Value::Object(serde_json::Map::new());
    let labels = |v: &Value| {
        v["metadata"]
            .get("labels")
            .cloned()
            .unwrap_or(empty.clone())
    };
    diff_json("metadata.labels", &labels(&a), &labels(&b), out);
    let (mut sa, mut sb) = (a["spec"].clone(), b["spec"].clone());
    if let (Some(x), Some(y)) = (sa.as_object_mut(), sb.as_object_mut())
        && matches!(want.spec, ResourceSpec::ProviderCredential(_))
    {
        // Werte werden unten über den Klartext verglichen.
        x.remove("data");
        y.remove("data");
    }
    diff_json("spec", &sa, &sb, out);

    match (&cur.extra, res) {
        (Extra::Credential(old), Resolved::Credential(new)) => {
            let secret = |path: String| FieldChange {
                path,
                before: None,
                after: None,
                secret: true,
            };
            match old {
                Some(old) => {
                    let fields: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
                    for f in fields {
                        if old.get(f).map(SecretString::expose)
                            != new.get(f).map(SecretString::expose)
                        {
                            out.push(secret(format!("spec.data.{f}")));
                        }
                    }
                }
                // Ohne entschlüsselten Ist-Zustand lässt sich nichts vergleichen.
                None => out.push(secret("spec.data".into())),
            }
        }
        (Extra::ApiKey { hash, .. }, Resolved::ApiKey(Some(s)))
            if hash_api_key(s.expose()) != *hash =>
        {
            out.push(FieldChange {
                path: "spec.secret".into(),
                before: None,
                after: None,
                secret: true,
            });
        }
        _ => {}
    }
    Ok(())
}

/// Belegt `key` für `name`; meldet, wenn ein anderes Objekt ihn schon hat.
fn claim<K: Ord>(
    map: &mut BTreeMap<K, String>,
    key: K,
    what: &str,
    at: (ResourceKind, &str, Option<usize>),
    issues: &mut Vec<Issue>,
) {
    let (kind, name, doc) = at;
    if let Some(other) = map.insert(key, name.to_owned())
        && other != name
    {
        issues.push(Issue::at(
            doc,
            kind,
            name,
            format!("{what} bereits von {kind} {other:?} belegt"),
        ));
    }
}

/// Eindeutigkeit im Endzustand: Modellnamen/Aliasse, Preise, OIDC-Identitäten, Keys.
fn check_unique(
    fin: &mut Final<'_>,
    state: &State,
    resolved: &BTreeMap<(ResourceKind, String), Resolved>,
    issues: &mut Vec<Issue>,
) {
    let mut models: BTreeMap<String, String> = BTreeMap::new();
    let mut prices: BTreeMap<(String, String, i128), String> = BTreeMap::new();
    let mut oidc: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut prefixes: BTreeMap<String, String> = BTreeMap::new();
    for (kind, objs) in &fin.objects {
        let kind = *kind;
        for (name, (doc, m)) in objs {
            let at = (kind, name.as_str(), *doc);
            match &m.spec {
                ResourceSpec::ModelGroup(g) => {
                    for n in std::iter::once(name).chain(g.aliases.iter()) {
                        let what = format!("Modellname/Alias {n:?}");
                        claim(&mut models, n.clone(), &what, at, issues);
                    }
                }
                ResourceSpec::Price(p) => {
                    let key = (
                        p.provider
                            .map(|p| p.as_str().to_owned())
                            .unwrap_or_default(),
                        p.model.clone(),
                        p.valid_from.unix_timestamp_nanos(),
                    );
                    let what = "Kombination Anbieter/Modell/validFrom";
                    claim(&mut prices, key, what, at, issues);
                }
                ResourceSpec::User(u) => {
                    if let (Some(i), Some(s)) = (&u.oidc_issuer, &u.oidc_subject) {
                        let key = (i.clone(), s.clone());
                        claim(&mut oidc, key, "OIDC-Identität", at, issues);
                    }
                }
                ResourceSpec::ApiKey(_) => {
                    let prefix = match resolved.get(&(kind, name.clone())) {
                        Some(Resolved::ApiKey(Some(s))) => {
                            parse_prefix(s.expose()).map(str::to_owned)
                        }
                        _ => match state.get(kind, name).map(|c| &c.extra) {
                            Some(Extra::ApiKey { status, .. }) => Some(status.prefix.clone()),
                            _ => None,
                        },
                    };
                    if let Some(p) = prefix {
                        claim(&mut prefixes, p, "Key-Präfix", at, issues);
                    }
                }
                _ => {}
            }
        }
    }
    fin.models = models.into_keys().collect();
}

/// Verweis auf ein Objekt (Feld, Zielart, Zielname).
type Ref = (String, ResourceKind, String);

fn limit_models(limits: &[LimitSpec], models: &mut Vec<(&'static str, String)>) {
    for l in limits {
        if let Some(g) = &l.model_group {
            models.push(("spec.limits.modelGroup", g.clone()));
        }
    }
}

fn scope_ref(field: &str, scope: &Scope, refs: &mut Vec<Ref>, errs: &mut Vec<String>) {
    match (level_kind(scope.level), &scope.target) {
        (None, None) => {}
        (None, Some(_)) => errs.push(format!("{field}: Ebene `global` hat kein Ziel")),
        (Some(_), None) => errs.push(format!("{field}.target fehlt")),
        (Some(k), Some(t)) => refs.push((format!("{field}.target"), k, t.clone())),
    }
}

fn r(field: &str, kind: ResourceKind, target: &str) -> Ref {
    (field.to_owned(), kind, target.to_owned())
}

/// Sammelt Verweise (auf Objekte und Modellnamen) einer Spezifikation.
fn collect_refs(
    spec: &ResourceSpec,
    refs: &mut Vec<Ref>,
    models: &mut Vec<(&'static str, String)>,
    errs: &mut Vec<String>,
) {
    match spec {
        ResourceSpec::Organization(o) => limit_models(&o.limits, models),
        ResourceSpec::Team(t) => {
            refs.push(r(
                "spec.organization",
                ResourceKind::Organization,
                &t.organization,
            ));
            for g in &t.allowed_models {
                models.push(("spec.allowedModels", g.clone()));
            }
            limit_models(&t.limits, models);
        }
        ResourceSpec::User(u) => {
            for b in &u.roles {
                if let Some(o) = &b.organization {
                    refs.push(r("spec.roles.organization", ResourceKind::Organization, o));
                }
                if let Some(t) = &b.team {
                    refs.push(r("spec.roles.team", ResourceKind::Team, t));
                }
            }
        }
        ResourceSpec::ApiKey(k) => {
            if let Some(t) = &k.team {
                refs.push(r("spec.team", ResourceKind::Team, t));
            }
            if let Some(u) = &k.user {
                refs.push(r("spec.user", ResourceKind::User, u));
            }
            for g in &k.allowed_models {
                models.push(("spec.allowedModels", g.clone()));
            }
            limit_models(&k.limits, models);
        }
        ResourceSpec::ProviderCredential(_) | ResourceSpec::Price(_) => {}
        ResourceSpec::ModelGroup(g) => {
            for f in &g.fallbacks {
                models.push(("spec.fallbacks.modelGroup", f.model_group.clone()));
            }
            limit_models(&g.limits, models);
        }
        ResourceSpec::Deployment(d) => {
            refs.push(r(
                "spec.modelGroup",
                ResourceKind::ModelGroup,
                &d.model_group,
            ));
            if let Some(c) = &d.credential {
                refs.push(r("spec.credential", ResourceKind::ProviderCredential, c));
            }
        }
        ResourceSpec::PluginBinding(b) => scope_ref("spec.scope", &b.scope, refs, errs),
        ResourceSpec::McpServer(s) => {
            if let Some(c) = &s.credential {
                refs.push(r("spec.credential", ResourceKind::ProviderCredential, c));
            }
        }
        ResourceSpec::McpGrant(g) => {
            refs.push(r("spec.server", ResourceKind::McpServer, &g.server));
            if matches!(g.subject.level, ScopeLevel::Team | ScopeLevel::ApiKey) {
                scope_ref("spec.subject", &g.subject, refs, errs);
            } else {
                errs.push("spec.subject.level: nur `team` oder `api-key` erlaubt".into());
            }
        }
    }
}

/// Prüft alle Verweise eines Objekts gegen den Endzustand.
/// Inhaltliche Regeln wie in der Admin-API ([`aigw_core::validate`]) für alle Dokumente des
/// Manifests: Aliasse, Fallbacks (Ziele, Zyklen im Endzustand), Deployments (SSRF,
/// Pflichtfelder, Zugangsdaten desselben Anbieters), Feldnamen/Pflichtfelder der Zugangsdaten,
/// Preise (Beträge; gültige Versionen unveränderlich).
fn check_rules(
    fin: &Final<'_>,
    state: &State,
    opts: &ApplyOptions,
    resolved: &BTreeMap<(ResourceKind, String), Resolved>,
    issues: &mut Vec<Issue>,
) {
    let now = time::OffsetDateTime::now_utc();
    let of = |kind: ResourceKind| fin.objects.get(&kind).into_iter().flatten();
    let groups: BTreeMap<&str, &ModelGroupSpec> = of(ResourceKind::ModelGroup)
        .filter_map(|(n, (_, m))| match &m.spec {
            ResourceSpec::ModelGroup(g) => Some((n.as_str(), g)),
            _ => None,
        })
        .collect();
    let creds: BTreeMap<&str, ProviderKind> = of(ResourceKind::ProviderCredential)
        .filter_map(|(n, (_, m))| match &m.spec {
            ResourceSpec::ProviderCredential(c) => Some((n.as_str(), c.provider)),
            _ => None,
        })
        .collect();
    for (kind, objs) in &fin.objects {
        for (name, (doc, m)) in objs {
            // Bestehende Objekte außerhalb des Manifests werden nicht nachträglich geprüft.
            let Some(doc) = doc else { continue };
            let res = match &m.spec {
                ResourceSpec::ModelGroup(g) => validate::aliases(name, g)
                    .and_then(|()| validate::fallbacks(&groups, name, g))
                    .and_then(|()| opts.group_settings.map_or(Ok(()), |f| f(&g.settings))),
                ResourceSpec::Deployment(d) => {
                    validate::deployment(&opts.upstreams, d).and_then(|()| {
                        match d
                            .credential
                            .as_deref()
                            .and_then(|c| Some((c, creds.get(c)?)))
                        {
                            Some((c, p)) => validate::deployment_credential(d, c, *p),
                            None => Ok(()),
                        }
                    })
                }
                ResourceSpec::ProviderCredential(c) => {
                    validate::credential_fields(c.provider, c.data.keys().map(String::as_str))
                        .and_then(|()| match resolved.get(&(*kind, name.clone())) {
                            Some(Resolved::Credential(v)) => v
                                .iter()
                                .try_for_each(|(f, v)| validate::credential_value(f, v.expose())),
                            _ => Ok(()),
                        })
                }
                ResourceSpec::Price(pr) => validate::price(pr).and_then(|()| {
                    match state
                        .get(ResourceKind::Price, name)
                        .map(|c| &c.manifest.spec)
                    {
                        Some(ResourceSpec::Price(cur)) => validate::price_change(cur, pr, now),
                        _ => Ok(()),
                    }
                }),
                _ => Ok(()),
            };
            if let Err(e) = res {
                issues.push(Issue::at(Some(*doc), *kind, name, e));
            }
        }
    }
}

fn check_refs(
    at: (ResourceKind, &str, Option<usize>),
    m: &Manifest,
    fin: &Final<'_>,
    state: &State,
    issues: &mut Vec<Issue>,
) {
    let (kind, name, doc) = at;
    let mut errs: Vec<String> = Vec::new();
    let mut refs: Vec<Ref> = Vec::new();
    let mut models: Vec<(&'static str, String)> = Vec::new();
    collect_refs(&m.spec, &mut refs, &mut models, &mut errs);
    for (field, target, t) in refs {
        if !fin.has(target, &t) {
            let pruned = if state.get(target, &t).is_some() {
                " (würde durch --prune gelöscht)"
            } else {
                ""
            };
            errs.push(format!(
                "{field}: Verweis auf unbekannte Ressource {target} {t:?}{pruned}"
            ));
        }
    }
    for (field, g) in models {
        if !fin.models.contains(&g) {
            errs.push(format!("{field}: unbekannte Modellgruppe/Alias {g:?}"));
        }
    }
    for e in errs {
        let message = if doc.is_none() {
            format!("(bestehend, nicht im Manifest) {e}")
        } else {
            e
        };
        issues.push(Issue::at(doc, kind, name, message));
    }
}
