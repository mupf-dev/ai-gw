// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Standard-Guardrail-Profile als Vorschläge (AIGW-86, `docs/konzept/13-guardrails.md` §10).
//!
//! - **Profile** „Basis“, „Streng“, „Offen“ liegen versioniert als YAML im Binary
//!   (`crates/aigw-admin/profiles/*.yaml`). Jedes Profil besteht aus Plugin-Bindungs-Specs
//!   (Platzhalter-Scope `global`), einem Erklärungstext und optional einer Obergrenze für das
//!   Payload-Logging.
//! - `GET /admin/api/v1/guardrail-profiles` – Liste mit Inhalt (Lesen: wie der
//!   Plugin-Katalog).
//! - `POST /admin/api/v1/guardrail-profiles/{name}:apply` – wendet ein Profil auf einen Scope
//!   an (`global`, `organization`, `team`, `api-key`, `model-group`). Mit `dryRun: true` nur
//!   der Diff. Bindungen heißen `<profil>-<plugin>-<ebene>[-<ziel>]` und tragen die Labels
//!   `aigw.io/guardrail-profile` und `aigw.io/guardrail-profile-version`. **Idempotent:**
//!   unveränderte Bindungen werden nicht geschrieben; Bindungen eines (anderen) Profils auf
//!   demselben Scope, die nicht mehr dazugehören, werden entfernt (Profilwechsel). Von Hand
//!   angelegte Bindungen ohne Profil-Label bleiben unberührt.
//! - **Rechte und Prüfung wie bei Plugin-Bindungen** (F183, AIGW-58): Schreibrecht auf das
//!   Ziel jeder Bindung, Registry-Prüfung (`422`), Verweise auf Secrets/Zugangsdaten
//!   (`content-safety`) nur durch einen globalen `admin`, `locked` von oben. Jede geschriebene
//!   Bindung bekommt einen eigenen Audit-Eintrag (inkrementeller Snapshot).
//! - **Payload-Logging** (`logging.payloadModeMax`): auf Organisation/Team als
//!   `payloadModeMax` (nur absenken, nie anheben; `payloadMode` darüber wird mit abgesenkt).
//!   Ohne Recht dazu bzw. auf anderen Ebenen: Hinweis statt Änderung.

#![allow(unused_qualifications)]

use std::collections::BTreeMap;
use std::sync::LazyLock;

use aigw_core::observe::PayloadMode;
use aigw_core::resource::{
    JsonObject, Object, ObjectMeta, OrganizationSpec, PluginBindingSpec, ResourceKind, Scope,
    ScopeLevel, TeamSpec, validate_name,
};
use aigw_store::{Expect, WriteExtras};
use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use crate::AdminState;
use crate::auth::{Permission, Principal, Target};
use crate::bounds::BoundsQuery;
use crate::problem::Problem;
use crate::resources::{ApiResult, St, json_with};

/// Label: Name des Profils, aus dem eine Bindung stammt.
pub const PROFILE_LABEL: &str = "aigw.io/guardrail-profile";
/// Label: angewendete Version des Profils.
pub const VERSION_LABEL: &str = "aigw.io/guardrail-profile-version";
/// Parameter-Schlüssel für `content-safety` beim Anwenden.
const CONTENT_SAFETY_KEYS: [&str; 12] = [
    "provider",
    "endpoint",
    "secret",
    "credential",
    "auth",
    "maxChecks",
    "model",
    "apiVersion",
    "modelGroup",
    "thresholds",
    "categories",
    "promptTemplate",
];

const SOURCES: [&str; 3] = [
    include_str!("../profiles/basis.yaml"),
    include_str!("../profiles/streng.yaml"),
    include_str!("../profiles/offen.yaml"),
];

/// Eine Bindung eines Profils.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GuardrailProfileBinding {
    /// Erklärung der Bindung.
    pub note: String,
    /// Benötigte Angabe beim Anwenden (`contentSafety`); fehlt sie, wird die Bindung
    /// ausgelassen (Hinweis in `warnings`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires: Option<String>,
    /// Spezifikation der Bindung (Scope ist ein Platzhalter; beim Anwenden ersetzt).
    pub spec: PluginBindingSpec,
}

/// Payload-Logging-Empfehlung eines Profils.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GuardrailProfileLogging {
    /// Obergrenze für `payloadMode` (Organisation/Team: `payloadModeMax`).
    pub payload_mode_max: PayloadMode,
}

/// Standard-Guardrail-Profil (eingebettet, versioniert).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GuardrailProfile {
    /// Name (Präfix der Bindungsnamen), z. B. `basis`.
    pub name: String,
    /// Version des Profilinhalts.
    pub version: u32,
    /// Anzeigename.
    pub title: String,
    /// Kurzbeschreibung (eine Zeile).
    pub summary: String,
    /// Erklärungstext.
    pub description: String,
    /// Payload-Logging-Empfehlung.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging: Option<GuardrailProfileLogging>,
    /// Bindungen des Profils.
    pub bindings: Vec<GuardrailProfileBinding>,
}

/// Antwort von `GET /guardrail-profiles`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct GuardrailProfileList {
    /// Profile (Reihenfolge: Basis, Streng, Offen).
    pub items: Vec<GuardrailProfile>,
}

static PROFILES: LazyLock<Vec<GuardrailProfile>> = LazyLock::new(|| {
    SOURCES
        .iter()
        .map(|s| {
            serde_saphyr::from_str(s)
                .unwrap_or_else(|e| unreachable!("eingebettetes Profil ungültig: {e}"))
        })
        .collect()
});

/// Alle eingebetteten Profile.
pub fn profiles() -> &'static [GuardrailProfile] {
    &PROFILES
}

/// Anfrage an `POST /guardrail-profiles/{name}:apply`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GuardrailProfileApply {
    /// Ziel-Scope (`global` ohne `target`).
    pub scope: Scope,
    /// Nur Diff berechnen, nichts schreiben.
    #[serde(default)]
    pub dry_run: bool,
    /// Angaben für `content-safety` (Profil „Streng“): `provider`, `endpoint`, `secret` bzw.
    /// `credential`, `auth` (`entraId` mit `credential` = ProviderCredential mit `spec.entra`,
    /// AIGW-89), `maxChecks`, `modelGroup`, `apiVersion`, `model`, `thresholds`, `categories`,
    /// `promptTemplate`. Ohne Angabe wird die Bindung ausgelassen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub content_safety: Option<JsonObject>,
    /// Payload-Logging-Empfehlung anwenden (Standard `true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging: Option<bool>,
}

/// Art einer Änderung.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ProfileChangeAction {
    /// Wird angelegt.
    Create,
    /// Wird geändert.
    Update,
    /// Wird gelöscht (Bindung eines Profils, die nicht mehr dazugehört).
    Delete,
    /// Schon aktuell.
    Unchanged,
}

/// Eine Änderung im Diff.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProfileChange {
    /// Art des Objekts: `plugin-binding`, `organization` oder `team`.
    pub kind: String,
    /// Name des Objekts.
    pub name: String,
    /// Änderung.
    pub action: ProfileChangeAction,
    /// Plugin (nur Bindungen).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    /// Bisheriger Stand (Bindung: Spec; Organisation/Team: Payload-Felder).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub before: Option<Value>,
    /// Neuer Stand.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub after: Option<Value>,
}

/// Ergebnis von `POST /guardrail-profiles/{name}:apply`.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GuardrailProfileApplyResult {
    /// Profil.
    pub profile: String,
    /// Angewendete Version.
    pub version: u32,
    /// Ziel-Scope.
    pub scope: Scope,
    /// Nur Diff (nichts geschrieben)?
    pub dry_run: bool,
    /// Änderungen (auch unveränderte Bindungen).
    pub changes: Vec<ProfileChange>,
    /// Hinweise (ausgelassene Bindungen, Logging nicht anwendbar …).
    pub warnings: Vec<String>,
}

/// Liste der Standard-Guardrail-Profile mit ihren Bindungen.
#[utoipa::path(
    get, path = "/admin/api/v1/guardrail-profiles", tag = "guardrail-profiles",
    responses(
        (status = 200, description = "Profile", body = GuardrailProfileList),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub(crate) async fn list_guardrail_profiles(Extension(p): Extension<Principal>) -> ApiResult {
    p.require_on(Permission::Read, &Target::Catalog)?;
    Ok(json_with(
        StatusCode::OK,
        None,
        &GuardrailProfileList {
            items: profiles().to_vec(),
        },
    ))
}

/// Wendet ein Profil auf einen Scope an (bzw. zeigt mit `dryRun` den Diff).
#[utoipa::path(
    post, path = "/admin/api/v1/guardrail-profiles/{name}:apply", tag = "guardrail-profiles",
    params(("name" = String, Path, description = "Profil (`basis`, `streng`, `offen`)")),
    request_body = GuardrailProfileApply,
    responses(
        (status = 200, description = "Diff bzw. angewendete Änderungen", body = GuardrailProfileApplyResult),
        (status = 400, description = "Ungültige Anfrage", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Profil unbekannt", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Ungültiger Scope oder ungültige Bindung", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub(crate) async fn apply_guardrail_profile(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(action): Path<String>,
    body: Result<Json<GuardrailProfileApply>, JsonRejection>,
) -> ApiResult {
    let Some(name) = action.strip_suffix(":apply") else {
        return Err(Problem::not_found(format!(
            "unbekannte Aktion {action:?} (erwartet <profil>:apply)"
        )));
    };
    let profile = profiles()
        .iter()
        .find(|x| x.name == name)
        .ok_or_else(|| Problem::not_found(format!("unbekanntes Guardrail-Profil {name:?}")))?;
    let Json(req) = body?;
    let scope = normalize_scope(&req.scope)?;
    let mut warnings = Vec::new();
    let desired = desired_bindings(profile, &scope, req.content_safety.as_ref(), &mut warnings)?;
    let logging = if req.logging.unwrap_or(true) {
        logging_plan(&st, &p, profile, &scope, &mut warnings).await?
    } else {
        Vec::new()
    };
    let changes = binding_plan(&st, &p, &scope, &desired).await?;
    if req.dry_run {
        let mut all: Vec<ProfileChange> = logging.iter().map(|l| l.change.clone()).collect();
        all.extend(changes.into_iter().map(|c| c.change));
        return Ok(result(profile, scope, true, all, warnings));
    }
    // Erst das Logging (eigene Prüfungen über die Ressourcen-Abläufe) – schlägt es fehl, ist
    // noch keine Bindung geschrieben.
    for l in &logging {
        if l.change.action != ProfileChangeAction::Unchanged {
            l.write(&st, &p).await?;
        }
    }
    let _guard = st.write_lock.lock().await;
    // Unter der Sperre neu planen (zwischenzeitliche Änderungen).
    let changes = binding_plan(&st, &p, &scope, &desired).await?;
    for c in &changes {
        match c.change.action {
            ProfileChangeAction::Create => {
                let meta = c.meta.clone().unwrap_or_default();
                let spec = c.spec.clone().ok_or_else(Problem::internal)?;
                st.store
                    .create_with(p.actor(), meta, spec, &WriteExtras::default())
                    .await?;
            }
            ProfileChangeAction::Update => {
                let meta = c.meta.clone().unwrap_or_default();
                let spec = c.spec.clone().ok_or_else(Problem::internal)?;
                st.store
                    .update_with(p.actor(), meta, spec, &WriteExtras::default())
                    .await?;
            }
            ProfileChangeAction::Delete => {
                st.store
                    .delete_with(
                        p.actor(),
                        ResourceKind::PluginBinding,
                        &c.change.name,
                        Expect::Any,
                        &WriteExtras::default(),
                    )
                    .await?;
            }
            ProfileChangeAction::Unchanged => {}
        }
    }
    let mut all: Vec<ProfileChange> = logging.into_iter().map(|l| l.change).collect();
    all.extend(changes.into_iter().map(|c| c.change));
    Ok(result(profile, scope, false, all, warnings))
}

fn result(
    profile: &GuardrailProfile,
    scope: Scope,
    dry_run: bool,
    changes: Vec<ProfileChange>,
    warnings: Vec<String>,
) -> axum::response::Response {
    json_with(
        StatusCode::OK,
        None,
        &GuardrailProfileApplyResult {
            profile: profile.name.clone(),
            version: profile.version,
            scope,
            dry_run,
            changes,
            warnings,
        },
    )
}

/// `global` ohne Ziel, sonst Ziel Pflicht.
fn normalize_scope(s: &Scope) -> Result<Scope, Problem> {
    let target = s.target.as_deref().map(str::trim).filter(|t| !t.is_empty());
    match (s.level, target) {
        (ScopeLevel::Global, None) => Ok(Scope::global()),
        (ScopeLevel::Global, Some(_)) => Err(Problem::invalid(
            "scope.target: auf Ebene global kein Ziel angeben",
        )),
        (_, None) => Err(Problem::invalid("scope.target: Pflicht auf dieser Ebene")),
        (level, Some(t)) => Ok(Scope {
            level,
            target: Some(t.to_owned()),
        }),
    }
}

/// Name einer Profil-Bindung: `<profil>-<plugin>-<ebene>[-<ziel>]`.
pub fn binding_name(profile: &str, plugin: &str, scope: &Scope) -> String {
    let mut n = format!("{profile}-{plugin}-{}", scope.level.as_str());
    if let Some(t) = &scope.target {
        n.push('-');
        n.push_str(t);
    }
    n
}

/// Soll-Bindungen eines Profils auf `scope` (Name → Objekt-Metadaten + Spec).
fn desired_bindings(
    profile: &GuardrailProfile,
    scope: &Scope,
    content_safety: Option<&JsonObject>,
    warnings: &mut Vec<String>,
) -> Result<Vec<(ObjectMeta, PluginBindingSpec)>, Problem> {
    if let Some(cs) = content_safety
        && let Some(k) = cs
            .keys()
            .find(|k| !CONTENT_SAFETY_KEYS.contains(&k.as_str()))
    {
        return Err(Problem::invalid(format!(
            "contentSafety.{k}: unbekannter Schlüssel (erlaubt: {})",
            CONTENT_SAFETY_KEYS.join(", ")
        )));
    }
    let mut used_cs = false;
    let mut out = Vec::new();
    for b in &profile.bindings {
        let mut spec = b.spec.clone();
        spec.scope = scope.clone();
        match b.requires.as_deref() {
            Some("contentSafety") => match content_safety {
                Some(cs) if !cs.is_empty() => {
                    used_cs = true;
                    for (k, v) in cs {
                        spec.config.insert(k.clone(), v.clone());
                    }
                }
                _ => {
                    warnings.push(format!(
                        "{} ausgelassen: für die externe Inhaltsprüfung contentSafety angeben \
                         (provider + endpoint + secret bzw. provider: model + modelGroup)",
                        spec.plugin
                    ));
                    continue;
                }
            },
            Some(other) => {
                return Err(Problem::internal().at(format!("requires {other}")));
            }
            None => {}
        }
        let name = binding_name(&profile.name, &spec.plugin, scope);
        validate_name(&name).map_err(|e| Problem::invalid(format!("scope.target: {e}")))?;
        let mut meta = ObjectMeta::named(name);
        meta.labels
            .insert(PROFILE_LABEL.into(), profile.name.clone());
        meta.labels
            .insert(VERSION_LABEL.into(), profile.version.to_string());
        out.push((meta, spec));
    }
    if content_safety.is_some_and(|c| !c.is_empty()) && !used_cs {
        warnings.push(format!(
            "contentSafety ignoriert: Profil {:?} enthält keine content-safety-Bindung",
            profile.name
        ));
    }
    Ok(out)
}

/// Geplante Änderung einer Bindung.
struct BindingChange {
    change: ProfileChange,
    meta: Option<ObjectMeta>,
    spec: Option<PluginBindingSpec>,
}

fn spec_json(s: &PluginBindingSpec) -> Option<Value> {
    serde_json::to_value(s).ok()
}

/// Vergleicht Soll und Ist, prüft Rechte und Konfiguration jeder Änderung.
async fn binding_plan(
    st: &AdminState,
    p: &Principal,
    scope: &Scope,
    desired: &[(ObjectMeta, PluginBindingSpec)],
) -> Result<Vec<BindingChange>, Problem> {
    let existing: BTreeMap<String, Object<PluginBindingSpec>> = st
        .store
        .list::<PluginBindingSpec>()
        .await?
        .into_iter()
        .map(|o| (o.metadata.name.clone(), o))
        .collect();
    let mut out = Vec::new();
    for (meta, spec) in desired {
        let current = existing.get(&meta.name);
        let action = match current {
            None => ProfileChangeAction::Create,
            Some(c) if c.spec == *spec && c.metadata.labels == meta.labels => {
                ProfileChangeAction::Unchanged
            }
            Some(_) => ProfileChangeAction::Update,
        };
        if action != ProfileChangeAction::Unchanged {
            crate::plugin_bindings::authorize_write(st, p, spec).await?;
            if let Some(c) = current {
                p.require_on(
                    Permission::Write,
                    &crate::plugin_bindings::binding_target(st, &c.spec).await?,
                )?;
            }
            crate::plugin_bindings::check(&st.upstreams, spec).map_err(Problem::invalid)?;
        }
        out.push(BindingChange {
            change: ProfileChange {
                kind: "plugin-binding".into(),
                name: meta.name.clone(),
                action,
                plugin: Some(spec.plugin.clone()),
                before: current.and_then(|c| spec_json(&c.spec)),
                after: spec_json(spec),
            },
            meta: Some(meta.clone()),
            spec: Some(spec.clone()),
        });
    }
    // Bindungen eines Profils auf demselben Scope, die nicht (mehr) dazugehören.
    for (name, o) in &existing {
        if o.spec.scope != *scope
            || !o.metadata.labels.contains_key(PROFILE_LABEL)
            || desired.iter().any(|(m, _)| m.name == *name)
        {
            continue;
        }
        p.require_on(
            Permission::Write,
            &crate::plugin_bindings::binding_target(st, &o.spec)
                .await
                .unwrap_or(Target::Global),
        )?;
        out.push(BindingChange {
            change: ProfileChange {
                kind: "plugin-binding".into(),
                name: name.clone(),
                action: ProfileChangeAction::Delete,
                plugin: Some(o.spec.plugin.clone()),
                before: spec_json(&o.spec),
                after: None,
            },
            meta: None,
            spec: None,
        });
    }
    Ok(out)
}

/// Payload-Felder einer Organisation bzw. eines Teams.
fn payload_json(mode: Option<PayloadMode>, max: Option<PayloadMode>) -> Value {
    serde_json::json!({"payloadMode": mode, "payloadModeMax": max})
}

/// Geplante Logging-Änderung (Organisation oder Team).
struct LoggingChange {
    change: ProfileChange,
    org: Option<(Object<OrganizationSpec>, OrganizationSpec)>,
    team: Option<(Object<TeamSpec>, TeamSpec)>,
}

impl LoggingChange {
    async fn write(&self, st: &AdminState, p: &Principal) -> Result<(), Problem> {
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::IF_MATCH, HeaderValue::from_static("*"));
        let q = BoundsQuery::default();
        let res = if let Some((cur, spec)) = &self.org {
            crate::resources::put_impl(
                st,
                p,
                &cur.metadata.name,
                &headers,
                cur.metadata.clone(),
                spec.clone(),
                &q,
            )
            .await?
        } else if let Some((cur, spec)) = &self.team {
            crate::resources::put_impl(
                st,
                p,
                &cur.metadata.name,
                &headers,
                cur.metadata.clone(),
                spec.clone(),
                &q,
            )
            .await?
        } else {
            return Ok(());
        };
        if res.status().is_success() {
            Ok(())
        } else {
            Err(Problem::internal())
        }
    }
}

/// Neue Payload-Felder: Obergrenze nur absenken, `payloadMode` darüber mit absenken.
fn capped(
    mode: Option<PayloadMode>,
    max: Option<PayloadMode>,
    cap: PayloadMode,
) -> (Option<PayloadMode>, Option<PayloadMode>) {
    let new_max = Some(max.map_or(cap, |m| m.min(cap)));
    let new_mode = mode.map(|m| m.min(cap));
    (new_mode, new_max)
}

async fn logging_plan(
    st: &AdminState,
    p: &Principal,
    profile: &GuardrailProfile,
    scope: &Scope,
    warnings: &mut Vec<String>,
) -> Result<Vec<LoggingChange>, Problem> {
    let Some(logging) = profile.logging else {
        return Ok(Vec::new());
    };
    let cap = logging.payload_mode_max;
    let target = scope.target.clone().unwrap_or_default();
    let hint = |what: &str| format!("Payload-Logging höchstens {:?}: {what}", cap.as_str());
    match scope.level {
        ScopeLevel::Organization => {
            let cur: Object<OrganizationSpec> = match st.store.get(&target).await {
                Ok(o) => o,
                Err(aigw_store::StoreError::NotFound { .. }) => {
                    return Err(Problem::invalid(format!(
                        "scope.target: unbekannte Organisation {target:?}"
                    )));
                }
                Err(e) => return Err(e.into()),
            };
            if !p.can(Permission::Write, &Target::Organization(target.clone())) {
                warnings.push(hint(
                    "keine Berechtigung, die Organisation zu ändern – bitte von einem admin setzen lassen",
                ));
                return Ok(Vec::new());
            }
            let (mode, max) = capped(cur.spec.payload_mode, cur.spec.payload_mode_max, cap);
            let mut spec = cur.spec.clone();
            spec.payload_mode = mode;
            spec.payload_mode_max = max;
            let action = if spec == cur.spec {
                ProfileChangeAction::Unchanged
            } else {
                ProfileChangeAction::Update
            };
            Ok(vec![LoggingChange {
                change: ProfileChange {
                    kind: "organization".into(),
                    name: target,
                    action,
                    plugin: None,
                    before: Some(payload_json(
                        cur.spec.payload_mode,
                        cur.spec.payload_mode_max,
                    )),
                    after: Some(payload_json(mode, max)),
                },
                org: Some((cur, spec)),
                team: None,
            }])
        }
        ScopeLevel::Team => {
            let cur: Object<TeamSpec> = match st.store.get(&target).await {
                Ok(o) => o,
                Err(aigw_store::StoreError::NotFound { .. }) => {
                    return Err(Problem::invalid(format!(
                        "scope.target: unbekanntes Team {target:?}"
                    )));
                }
                Err(e) => return Err(e.into()),
            };
            let org = cur.spec.organization.clone();
            // S-09: payloadModeMax eines Teams setzen nur admin/org-admin.
            if !p.can(Permission::Write, &Target::InOrganization(org))
                || !p.can(
                    Permission::Write,
                    &Target::Team {
                        organization: cur.spec.organization.clone(),
                        team: target.clone(),
                    },
                )
            {
                warnings.push(hint(
                    "nur admin oder org-admin dürfen payloadModeMax eines Teams setzen – bitte dort setzen lassen",
                ));
                return Ok(Vec::new());
            }
            let (mode, max) = capped(cur.spec.payload_mode, cur.spec.payload_mode_max, cap);
            let mut spec = cur.spec.clone();
            spec.payload_mode = mode;
            spec.payload_mode_max = max;
            let action = if spec == cur.spec {
                ProfileChangeAction::Unchanged
            } else {
                ProfileChangeAction::Update
            };
            Ok(vec![LoggingChange {
                change: ProfileChange {
                    kind: "team".into(),
                    name: target,
                    action,
                    plugin: None,
                    before: Some(payload_json(
                        cur.spec.payload_mode,
                        cur.spec.payload_mode_max,
                    )),
                    after: Some(payload_json(mode, max)),
                },
                org: None,
                team: Some((cur, spec)),
            }])
        }
        ScopeLevel::Global => {
            warnings.push(hint(
                "global über observe.payload in der Start-Konfiguration bzw. payloadModeMax je Organisation setzen",
            ));
            Ok(Vec::new())
        }
        ScopeLevel::ApiKey | ScopeLevel::ModelGroup => {
            warnings.push(hint(
                "auf dieser Ebene nicht als Obergrenze setzbar – payloadModeMax an Organisation bzw. Team setzen",
            ));
            Ok(Vec::new())
        }
    }
}

/// Routen der Guardrail-Profile.
pub(crate) fn router() -> axum::Router<AdminState> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route(
            "/admin/api/v1/guardrail-profiles",
            get(list_guardrail_profiles),
        )
        .route(
            "/admin/api/v1/guardrail-profiles/{action}",
            post(apply_guardrail_profile),
        )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use aigw_core::upstream::UpstreamPolicy;
    use serde_json::json;

    #[test]
    fn embedded_profiles_parse_and_validate() {
        let names: Vec<&str> = profiles().iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["basis", "streng", "offen"]);
        let policy = UpstreamPolicy::default();
        for p in profiles() {
            assert!(p.version >= 1);
            assert!(!p.description.trim().is_empty() && !p.summary.is_empty());
            for b in &p.bindings {
                assert_eq!(b.spec.scope, Scope::global(), "{}", p.name);
                if b.requires.is_some() {
                    // Mit Beispielangaben gültig.
                    let mut spec = b.spec.clone();
                    spec.config.insert("provider".into(), json!("model"));
                    spec.config.insert("modelGroup".into(), json!("guard"));
                    crate::plugin_bindings::check(&policy, &spec).unwrap();
                } else {
                    crate::plugin_bindings::check(&policy, &b.spec)
                        .unwrap_or_else(|e| panic!("{}: {e}", p.name));
                }
            }
        }
    }

    #[test]
    fn profile_contents_match_intent() {
        let get = |n: &str| profiles().iter().find(|p| p.name == n).unwrap();
        let plugins = |n: &str| -> Vec<String> {
            get(n)
                .bindings
                .iter()
                .map(|b| b.spec.plugin.clone())
                .collect()
        };
        assert_eq!(plugins("basis"), ["prompt-filter", "pii"]);
        assert_eq!(
            plugins("streng"),
            ["prompt-filter", "pii", "link-guard", "prompt-leak", "content-safety"]
        );
        assert_eq!(plugins("offen"), ["prompt-filter"]);
        assert!(get("basis").logging.is_none());
        assert_eq!(
            get("streng").logging.unwrap().payload_mode_max,
            PayloadMode::Masked
        );
        assert_eq!(
            get("offen").logging.unwrap().payload_mode_max,
            PayloadMode::Metadata
        );
        let streng_pii = &get("streng").bindings[1].spec.config;
        assert_eq!(streng_pii["categories"]["creditCard"], "reject");
        assert_eq!(streng_pii["categories"]["apiKey"], "reject");
        let offen = &get("offen").bindings[0].spec.config;
        assert!(offen.get("injection").is_none() && offen.get("limits").is_some());
    }

    #[test]
    fn names_and_scope() {
        assert_eq!(
            binding_name("basis", "pii", &Scope::global()),
            "basis-pii-global"
        );
        let t = Scope {
            level: ScopeLevel::Team,
            target: Some("support".into()),
        };
        assert_eq!(binding_name("streng", "pii", &t), "streng-pii-team-support");
        assert!(
            normalize_scope(&Scope {
                level: ScopeLevel::Team,
                target: None
            })
            .is_err()
        );
        assert!(
            normalize_scope(&Scope {
                level: ScopeLevel::Global,
                target: Some("x".into())
            })
            .is_err()
        );
    }

    #[test]
    fn content_safety_is_optional_and_whitelisted() {
        let streng = profiles().iter().find(|p| p.name == "streng").unwrap();
        let mut w = Vec::new();
        let d = desired_bindings(streng, &Scope::global(), None, &mut w).unwrap();
        assert_eq!(d.len(), 4);
        assert_eq!(w.len(), 1, "{w:?}");
        let mut w = Vec::new();
        let cs = json!({"provider": "model", "modelGroup": "guard"});
        let d = desired_bindings(streng, &Scope::global(), cs.as_object(), &mut w).unwrap();
        assert_eq!(d.len(), 5);
        assert_eq!(d[4].1.config["modelGroup"], "guard");
        assert_eq!(d[4].1.config["direction"], "both");
        assert_eq!(d[4].0.labels[PROFILE_LABEL], "streng");
        assert!(w.is_empty());
        let bad = json!({"action": "flag"});
        assert!(desired_bindings(streng, &Scope::global(), bad.as_object(), &mut w).is_err());
    }

    #[test]
    fn capping_only_lowers() {
        use PayloadMode::*;
        assert_eq!(capped(None, None, Masked), (None, Some(Masked)));
        assert_eq!(
            capped(Some(Full), Some(Full), Masked),
            (Some(Masked), Some(Masked))
        );
        assert_eq!(
            capped(Some(Off), Some(Metadata), Masked),
            (Some(Off), Some(Metadata))
        );
    }
}
