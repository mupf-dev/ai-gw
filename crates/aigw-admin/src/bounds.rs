// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Grenzen Key ≤ Team ≤ Organisation in der Admin-API (AIGW-67).
//!
//! Regeln: [`aigw_core::validate::bounds`]. Hier: wer darf was, und was passiert mit
//! bestehenden Objekten unterhalb einer geänderten Grenze.
//!
//! - Verstößt das geschriebene Objekt selbst gegen eine Grenze → `422` mit `violations`.
//! - Liegen bestehende Keys/Teams nach der Änderung über den neuen Grenzen → `409` mit
//!   `violations` je betroffenem Objekt; mit `?cascade=clamp` werden sie in derselben
//!   Transaktion auf die neuen Grenzen gekürzt (Audit `clamp` je Objekt).
//! - `?force=true` (nur globaler `admin`) lässt beides zu; ein Audit-Eintrag `force` hält die
//!   Verstöße fest.

use std::collections::HashMap;

use aigw_core::resource::{ApiKeySpec, ModelGroupSpec, Object, OrganizationSpec, TeamSpec};
use aigw_core::validate::bounds::{self, Clamp, ModelNames, Node, Violation};
use aigw_store::{Store, WriteExtras};
use serde::Deserialize;
use serde_json::json;
use utoipa::{IntoParams, ToSchema};

use crate::auth::Principal;
use crate::problem::Problem;
use crate::rbac;

/// Umgang mit bestehenden Objekten über einer gesenkten Grenze.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Cascade {
    /// Ablehnen (`409` mit Liste der betroffenen Objekte).
    #[default]
    Reject,
    /// Betroffene Teams/Keys auf die neuen Grenzen kürzen (Audit `clamp` je Objekt).
    Clamp,
}

/// Optionen beim Schreiben von Teams und Organisationen.
#[derive(Debug, Clone, Copy, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct BoundsQuery {
    /// Grenzverstöße bewusst zulassen (nur globaler `admin`; Audit-Eintrag `force`).
    pub force: Option<bool>,
    /// Bestehende Keys/Teams über den neuen Grenzen: `reject` (Standard, `409`) oder `clamp`.
    pub cascade: Option<Cascade>,
}

/// Optionen beim Schreiben von API-Keys.
#[derive(Debug, Clone, Copy, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct KeyBoundsQuery {
    /// Grenzverstöße bewusst zulassen (nur globaler `admin`; Audit-Eintrag `force`).
    pub force: Option<bool>,
}

/// `force` ist nur für globale Admins erlaubt.
fn force_of(p: &Principal, force: Option<bool>) -> Result<bool, Problem> {
    let force = force.unwrap_or(false);
    if force && !rbac::is_global_admin(&p.roles) {
        return Err(Problem::forbidden(
            "force=true ist nur für globale Admins erlaubt",
        ));
    }
    Ok(force)
}

async fn model_names(store: &Store) -> Result<ModelNames, Problem> {
    let groups: Vec<Object<ModelGroupSpec>> = store.list().await?;
    Ok(ModelNames::new(
        groups.iter().map(|g| (g.metadata.name.as_str(), &g.spec)),
    ))
}

async fn org_of(store: &Store, name: &str) -> Result<Option<Object<OrganizationSpec>>, Problem> {
    match store.get::<OrganizationSpec>(name).await {
        Ok(o) => Ok(Some(o)),
        // Unbekannte Organisation meldet die Verweisprüfung beim Schreiben (`422`).
        Err(aigw_store::StoreError::NotFound { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Nur Verstöße, die es mit den bisherigen Grenzen nicht gab (bereits bestehende Verstöße,
/// z. B. aus der Zeit vor AIGW-67 oder per `force`, blockieren andere Änderungen nicht).
fn new_only(new: Vec<Violation>, old: &[Violation]) -> Vec<Violation> {
    new.into_iter()
        .filter(|v| {
            !old.iter()
                .any(|o| o.level == v.level && o.name == v.name && o.field == v.field)
        })
        .collect()
}

/// Ergebnis: eigene Verstöße (`422` bzw. `force`) und Verstöße darunter (`409`, `clamp`
/// bzw. `force`).
fn conclude(
    spec: &impl serde::Serialize,
    own: Vec<Violation>,
    below: Vec<Violation>,
    unclampable: Vec<Violation>,
    force: bool,
    mut extras: WriteExtras,
) -> Result<WriteExtras, Problem> {
    if force {
        if !own.is_empty() || !below.is_empty() {
            // `spec` ordnet den Eintrag Organisation/Team zu (Sichtbarkeit im Audit-Log).
            extras.force_note = Some(json!({
                "force": true,
                "spec": spec,
                "violations": own,
                "affected": below,
            }));
        }
        return Ok(extras);
    }
    if !own.is_empty() {
        return Err(Problem::invalid(format!(
            "Grenzen der übergeordneten Ebene überschritten: {}",
            bounds::summary(&own)
        ))
        .with_violations(own));
    }
    if !unclampable.is_empty() {
        return Err(Problem::conflict(format!(
            "Betroffene Objekte lassen sich nicht automatisch anpassen: {}",
            bounds::summary(&unclampable)
        ))
        .with_violations(unclampable));
    }
    if !below.is_empty() {
        return Err(Problem::conflict(format!(
            "{} bestehende(s) Objekt(e) lägen über den neuen Grenzen – anpassen oder \
             ?cascade=clamp verwenden: {}",
            below.len(),
            bounds::summary(&below)
        ))
        .with_violations(below));
    }
    Ok(extras)
}

/// Prüft einen API-Key gegen sein Team und dessen Organisation.
pub(crate) async fn api_key(
    store: &Store,
    p: &Principal,
    name: &str,
    spec: &ApiKeySpec,
    q: &KeyBoundsQuery,
) -> Result<WriteExtras, Problem> {
    let force = force_of(p, q.force)?;
    let Some(team_name) = &spec.team else {
        return Ok(WriteExtras::default());
    };
    // Unbekanntes Team wurde bereits bei der Rollenprüfung abgelehnt.
    let team: Object<TeamSpec> = store.get(team_name).await?;
    let org = org_of(store, &team.spec.organization).await?;
    let mut parents = vec![Node::team(team_name, &team.spec)];
    if let Some(o) = &org {
        parents.push(Node::organization(&o.metadata.name, &o.spec));
    }
    let models = model_names(store).await?;
    let own = bounds::check(&Node::api_key(name, spec), &parents, &models);
    conclude(spec, own, vec![], vec![], force, WriteExtras::default())
}

/// Kürzt einen Key auf seine Eltern (für `cascade=clamp`); `Err` = nicht möglich.
fn clamp_key(
    key: &aigw_core::resource::ApiKey,
    parents: &[Node<'_>],
    models: &ModelNames,
) -> Result<Option<ApiKeySpec>, Violation> {
    let mut spec = key.spec.clone();
    match bounds::clamp(
        &mut spec.limits,
        &mut spec.budget,
        &mut spec.allowed_models,
        parents,
        models,
    ) {
        Clamp::Unchanged => Ok(None),
        Clamp::Changed => Ok(Some(spec)),
        Clamp::Impossible(why) => Err(Violation {
            level: bounds::Level::ApiKey,
            name: key.metadata.name.clone(),
            field: "allowedModels".into(),
            message: why,
        }),
    }
}

/// Prüft ein Team gegen seine Organisation und seine bestehenden Keys gegen die neuen
/// Team-Grenzen.
pub(crate) async fn team(
    store: &Store,
    p: &Principal,
    name: &str,
    spec: &TeamSpec,
    current: Option<&TeamSpec>,
    q: &BoundsQuery,
) -> Result<WriteExtras, Problem> {
    let force = force_of(p, q.force)?;
    let clamp = q.cascade == Some(Cascade::Clamp);
    let models = model_names(store).await?;
    let org = org_of(store, &spec.organization).await?;
    let org_node = org
        .as_ref()
        .map(|o| Node::organization(&o.metadata.name, &o.spec));
    let own = bounds::check(&Node::team(name, spec), org_node.as_slice(), &models);
    let mut parents = vec![Node::team(name, spec)];
    parents.extend(org_node);
    // Bisherige Grenzen (für „nur neue Verstöße“).
    let old_org = match current {
        Some(c) if c.organization != spec.organization => org_of(store, &c.organization).await?,
        _ => org.clone(),
    };
    let mut old_parents = Vec::new();
    if let Some(c) = current {
        old_parents.push(Node::team(name, c));
        old_parents.extend(
            old_org
                .as_ref()
                .map(|o| Node::organization(&o.metadata.name, &o.spec)),
        );
    }
    let mut below = Vec::new();
    let mut unclampable = Vec::new();
    let mut extras = WriteExtras::default();
    for key in store.list_api_keys().await? {
        if key.spec.team.as_deref() != Some(name) {
            continue;
        }
        let node = Node::api_key(&key.metadata.name, &key.spec);
        let v = bounds::check(&node, &parents, &models);
        if v.is_empty() {
            continue;
        }
        if clamp {
            match clamp_key(&key, &parents, &models) {
                Ok(Some(new)) => extras.clamp_api_keys.push((key, new)),
                Ok(None) => {}
                Err(e) => unclampable.push(e),
            }
        } else {
            let old = if current.is_some() {
                bounds::check(&node, &old_parents, &models)
            } else {
                Vec::new()
            };
            below.extend(new_only(v, &old));
        }
    }
    conclude(spec, own, below, unclampable, force, extras)
}

/// Prüft die Teams und Keys einer Organisation gegen die neuen Organisationsgrenzen.
pub(crate) async fn organization(
    store: &Store,
    p: &Principal,
    name: &str,
    spec: &OrganizationSpec,
    current: Option<&OrganizationSpec>,
    q: &BoundsQuery,
) -> Result<WriteExtras, Problem> {
    let force = force_of(p, q.force)?;
    let clamp = q.cascade == Some(Cascade::Clamp);
    let models = model_names(store).await?;
    let org_node = Node::organization(name, spec);
    let old_org: Vec<Node<'_>> = current
        .map(|c| Node::organization(name, c))
        .into_iter()
        .collect();
    let teams: Vec<Object<TeamSpec>> = store
        .list::<TeamSpec>()
        .await?
        .into_iter()
        .filter(|t| t.spec.organization == name)
        .collect();
    let mut below = Vec::new();
    let mut unclampable = Vec::new();
    let mut extras = WriteExtras::default();
    // Wirksamer Stand der Teams (bei `clamp` gekürzt) für die Prüfung ihrer Keys.
    let mut effective: HashMap<String, TeamSpec> = HashMap::new();
    for t in teams {
        let tname = t.metadata.name.clone();
        let node = Node::team(&tname, &t.spec);
        let v = bounds::check(&node, &[org_node], &models);
        if !v.is_empty() && clamp {
            let mut new = t.spec.clone();
            // Teams haben keine Modelleinschränkung durch die Organisation → immer möglich.
            if bounds::clamp(
                &mut new.limits,
                &mut new.budget,
                &mut new.allowed_models,
                &[org_node],
                &models,
            ) == Clamp::Changed
            {
                effective.insert(tname.clone(), new.clone());
                extras.clamp_teams.push((t, new));
                continue;
            }
        } else if !v.is_empty() {
            below.extend(new_only(v, &bounds::check(&node, &old_org, &models)));
        }
        effective.insert(tname, t.spec);
    }
    for key in store.list_api_keys().await? {
        let Some(team) = key
            .spec
            .team
            .as_deref()
            .and_then(|t| effective.get_key_value(t))
        else {
            continue;
        };
        let parents = [Node::team(team.0, team.1), org_node];
        let node = Node::api_key(&key.metadata.name, &key.spec);
        let v = bounds::check(&node, &parents, &models);
        if v.is_empty() {
            continue;
        }
        if clamp {
            match clamp_key(&key, &parents, &models) {
                Ok(Some(new)) => extras.clamp_api_keys.push((key, new)),
                Ok(None) => {}
                Err(e) => unclampable.push(e),
            }
        } else {
            // Bisher: Team unverändert, alte Organisationsgrenzen.
            let mut old_parents = vec![Node::team(team.0, team.1)];
            old_parents.extend(old_org.iter().copied());
            below.extend(new_only(v, &bounds::check(&node, &old_parents, &models)));
        }
    }
    conclude(spec, vec![], below, unclampable, force, extras)
}
