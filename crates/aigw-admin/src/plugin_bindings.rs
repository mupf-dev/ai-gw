// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Plugin-Bindungen (AIGW-69, `docs/konzept/12-plugins.md` §4).
//!
//! - **Prüfung beim Schreiben (F118/F165):** `config` wird mit der Registry der Plugin-Kette
//!   geprüft – Kettensteuerung (`inherit`, `locked`, `failMode`, `timeoutMs`,
//!   `outputRejectMode`) und Plugin-Konfiguration (`deny_unknown_fields`, Wertebereiche,
//!   SSRF-Regeln für `webhook`/`content-safety` aus `admin.allow_*_upstreams`). Ungültig →
//!   `422` mit der Meldung des Plugins. Dieselbe Prüfung läuft im GitOps-Plan
//!   ([`check`] als `ApplyOptions::plugin_bindings`).
//! - **Wirksame Steuerung (F148):** Die gelesene Darstellung enthält `effective` – den
//!   wirksamen Fehlermodus (mit Herkunft: Bindung oder Standard der Plugin-Art), das
//!   Zeitlimit und `outputRejectMode`, damit die UI sie anzeigen kann.
//! - **Rechte (F183):** lesen darf jede wirksame Rolle. Schreiben je Ebene: `global` und
//!   `model-group` nur globaler `admin` (wie der Modellkatalog); `organization` O: `admin` im
//!   Bereich und `org-admin` von O; `team` T: zusätzlich `team-admin` von T; `api-key`: wie
//!   der Key selbst (Team bzw. Organisation des Keys, F193).
//! - **`locked` von oben:** Eingeschränkte Rollen dürfen eine von außen gesperrte Bindung
//!   (`locked: true` auf einer äußeren Ebene desselben Plugins) weder mit `inherit: override`
//!   noch `disable` aushebeln (`403`); sie wäre ohnehin wirkungslos.
//! - **Geheimnis-Verweise (AIGW-58):** `config.secret`/`config.credential` verweisen auf
//!   globale Secrets/Zugangsdaten; solche Bindungen schreibt nur ein globaler `admin` (`403`).

// `#[utoipa::path(params(…))]` erzeugt für IntoParams-Typen qualifizierte Pfade.
#![allow(unused_qualifications)]

use std::sync::LazyLock;

use aigw_core::resource::{Id, Object, ObjectMeta, PluginBindingSpec, ScopeLevel, TeamSpec};
use aigw_core::upstream::UpstreamPolicy;
use aigw_proxy::PluginRegistry as Registry;
use aigw_store::{ListFilter, WriteExtras};
use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::AdminState;
use crate::auth::{Permission, Principal, Target};
use crate::bounds::BoundsQuery;
use crate::problem::Problem;
use crate::resources::{
    AdminResource, ApiResult, ListArgs, St, create_impl, delete_impl, get_impl, json_with,
    list_impl, plain_resource, put_impl,
};

/// Prüft eine Bindung mit der Registry der Plugin-Kette und den SSRF-Regeln `policy`
/// (`admin.*`). Auch als `ApplyOptions::plugin_bindings` für GitOps
/// (`aigw config validate|diff|apply`, `config:apply`).
pub fn check(policy: &UpstreamPolicy, spec: &PluginBindingSpec) -> Result<(), String> {
    Registry::for_validation(policy)
        .validate_spec(spec)
        .map_err(|e| format!("Plugin-Bindung ({}): {e}", spec.plugin))
}

/// Standardwerte der Plugin-Arten (unabhängig von Host und SSRF-Regeln).
static DEFAULTS: LazyLock<Registry> = LazyLock::new(Registry::with_builtin);

/// Herkunft eines wirksamen Werts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ValueSource {
    /// In der Bindung gesetzt.
    Binding,
    /// Standard der Plugin-Art.
    PluginDefault,
}

/// Wirksame Kettensteuerung einer Bindung (F148), für die Anzeige in der UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct EffectivePluginControl {
    /// Fehlermodus bei Laufzeitfehlern und Zeitüberschreitung: `open` (überspringen) oder
    /// `closed` (ablehnen). Standard je Plugin-Art: `content-safety` closed, `webhook` open,
    /// sonst closed.
    #[schema(value_type = String, example = "closed")]
    pub fail_mode: String,
    /// Woher der Fehlermodus kommt.
    pub fail_mode_source: ValueSource,
    /// Zeitlimit asynchroner Haken (Millisekunden).
    pub timeout_ms: u64,
    /// Ausgabe-Ablehnung eines Guardrails: `finish` (200 mit `content_filter`/`refusal`) oder
    /// `error` (400 bzw. Fehlerereignis im Stream).
    #[schema(value_type = String, example = "finish")]
    pub output_reject_mode: String,
}

/// Wirksame Steuerung (`None` bei unbekanntem Plugin oder ungültiger Steuerung – z. B. vor
/// F118 per GitOps gespeicherte Bindungen).
fn effective(o: &Object<PluginBindingSpec>) -> Option<EffectivePluginControl> {
    let e = DEFAULTS.effective_spec(&o.spec).ok()?;
    Some(EffectivePluginControl {
        fail_mode: e.fail_mode.as_str().to_owned(),
        fail_mode_source: if e.fail_mode_default {
            ValueSource::PluginDefault
        } else {
            ValueSource::Binding
        },
        timeout_ms: u64::try_from(e.timeout.as_millis()).unwrap_or(u64::MAX),
        output_reject_mode: e.output_reject.as_str().to_owned(),
    })
}

/// Filter für Plugin-Bindungen.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct PluginBindingQuery {
    /// Seitengröße (1–500, Standard 50).
    pub limit: Option<u32>,
    /// Fortsetzung (`nextCursor` der vorigen Seite).
    pub cursor: Option<String>,
    /// Teilstring im Namen.
    pub q: Option<String>,
    /// Label-Selektor `k=v,k2`.
    pub label: Option<String>,
    /// Nur dieses Plugin (z. B. `pii`).
    pub plugin: Option<String>,
    /// Nur diese Ebene (`global`, `organization`, `team`, `api-key`, `model-group`).
    pub level: Option<String>,
    /// Nur dieses Ziel der Ebene (Name).
    pub target: Option<String>,
}

plain_resource!(
    spec = PluginBindingSpec,
    res = PluginBinding,
    input = PluginBindingInput,
    list = PluginBindingList,
    query = PluginBindingQuery,
    tag = "plugin-bindings",
    path = "/admin/api/v1/plugin-bindings",
    item_path = "/admin/api/v1/plugin-bindings/{name}",
    what = "Plugin-Bindung",
    handlers = [
        list_plugin_bindings,
        create_plugin_binding,
        get_plugin_binding,
        put_plugin_binding,
        delete_plugin_binding
    ],
    bounds = BoundsQuery,
    res_extra = [
        /// Wirksame Kettensteuerung (F148; fehlt bei ungültiger Bindung).
        #[serde(skip_serializing_if = "Option::is_none")]
        effective: Option<EffectivePluginControl> = effective
    ]
);

impl AdminResource for PluginBindingSpec {
    type Res = PluginBinding;
    type Query = PluginBindingQuery;
    const COLLECTION: &'static str = "plugin-bindings";

    fn target(_name: &str, _spec: &Self) -> Target {
        Target::Catalog
    }

    async fn target_for(st: &AdminState, _name: &str, spec: &Self) -> Result<Target, Problem> {
        binding_target(st, spec).await
    }

    fn list_args(q: &Self::Query) -> ListArgs<'_> {
        ListArgs {
            limit: q.limit,
            cursor: q.cursor.as_deref(),
            q: q.q.as_deref(),
            label: q.label.as_deref(),
        }
    }

    fn list_filters(q: &Self::Query) -> Result<Vec<ListFilter>, Problem> {
        let mut f = Vec::new();
        f.extend(q.plugin.iter().map(|p| ListFilter::Plugin(p.clone())));
        f.extend(q.level.iter().map(|l| ListFilter::Level(l.clone())));
        f.extend(q.target.iter().map(|t| ListFilter::Target(t.clone())));
        Ok(f)
    }

    async fn bounds(
        st: &AdminState,
        p: &Principal,
        _name: &str,
        spec: &Self,
        _current: Option<&Object<Self>>,
        q: &BoundsQuery,
    ) -> Result<WriteExtras, Problem> {
        if q.force.is_some() || q.cascade.is_some() {
            return Err(Problem::bad_request(
                "force/cascade gibt es bei Plugin-Bindungen nicht",
            ));
        }
        respect_secret_refs(p, spec)?;
        respect_locked(st, p, spec).await?;
        Ok(WriteExtras::default())
    }

    async fn check(
        st: &AdminState,
        _name: &str,
        spec: &Self,
        _current: Option<&Object<Self>>,
    ) -> Result<(), Problem> {
        check(&st.upstreams, spec).map_err(Problem::invalid)?;
        crate::wasm_modules::check_binding(st, spec).await
    }
}

/// Konfigurationsschlüssel, mit denen ein Plugin globale Geheimnisse referenziert
/// (`content-safety`: `secret`/`credential`; `webhook`: `secret` mit `secret`/`credential`/
/// `env`/`file`).
const SECRET_REF_KEYS: [&str; 2] = ["secret", "credential"];

/// AIGW-58 (Sicherheitsreview): Secrets und Provider-Credentials sind global (ohne
/// Org/Team-Bereich). Eingeschränkte Rollen (org-/team-admin) dürfen sie deshalb in
/// Bindungen nicht referenzieren – sonst könnten sie einen globalen Schlüssel per eigenem
/// `endpoint` an einen beliebigen Server schicken lassen (`403`).
fn respect_secret_refs(p: &Principal, spec: &PluginBindingSpec) -> Result<(), Problem> {
    if p.can(Permission::Write, &Target::Catalog) {
        return Ok(());
    }
    if let Some(k) = SECRET_REF_KEYS
        .iter()
        .find(|k| spec.config.get(**k).is_some_and(|v| !v.is_null()))
    {
        return Err(Problem::forbidden(format!(
            "config.{k}: Secrets und Zugangsdaten sind global; Bindungen mit Verweis darauf \
             darf nur ein globaler admin schreiben"
        )));
    }
    Ok(())
}

/// Schreibrechte für eine Bindung wie bei `POST/PUT /plugin-bindings`: Ziel der Ebene (F183),
/// Secret-Verweise (AIGW-58), `locked` von oben. Auch für Guardrail-Profile (AIGW-86).
pub(crate) async fn authorize_write(
    st: &AdminState,
    p: &Principal,
    spec: &PluginBindingSpec,
) -> Result<(), Problem> {
    p.require_on(Permission::Write, &binding_target(st, spec).await?)?;
    respect_secret_refs(p, spec)?;
    respect_locked(st, p, spec).await
}

/// RBAC-Ziel einer Bindung je Ebene (F183). Unbekanntes Ziel → `422`.
pub(crate) async fn binding_target(
    st: &AdminState,
    spec: &PluginBindingSpec,
) -> Result<Target, Problem> {
    let name = || {
        spec.scope
            .target
            .clone()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| Problem::invalid("scope.target: Pflicht auf dieser Ebene"))
    };
    match spec.scope.level {
        ScopeLevel::Global | ScopeLevel::ModelGroup => Ok(Target::Catalog),
        ScopeLevel::Organization => Ok(Target::InOrganization(name()?)),
        ScopeLevel::Team => {
            let t = name()?;
            match st.store.get::<TeamSpec>(&t).await {
                Ok(team) => Ok(Target::in_team(team.spec.organization, t)),
                Err(aigw_store::StoreError::NotFound { .. }) => Err(Problem::invalid(format!(
                    "scope.target: unbekanntes Team {t:?}"
                ))),
                Err(e) => Err(e.into()),
            }
        }
        ScopeLevel::ApiKey => {
            let k = name()?;
            match st.store.get_api_key(&k).await {
                Ok(key) => crate::resources::key_target(&st.store, &key.spec).await,
                Err(aigw_store::StoreError::NotFound { .. }) => Err(Problem::invalid(format!(
                    "scope.target: unbekannter API-Key {k:?}"
                ))),
                Err(e) => Err(e.into()),
            }
        }
    }
}

/// `inherit` einer Bindung (`append` Standard; Spec-Feld seit F117).
fn inherit_of(spec: &PluginBindingSpec) -> &'static str {
    spec.inherit.unwrap_or_default().as_str()
}

/// F183: Eingeschränkte Rollen dürfen eine auf einer äußeren Ebene gesperrte Bindung desselben
/// Plugins nicht per `override`/`disable` aushebeln. Äußere Ebenen: global, Organisation,
/// Team (je nach Ziel der Bindung).
async fn respect_locked(
    st: &AdminState,
    p: &Principal,
    spec: &PluginBindingSpec,
) -> Result<(), Problem> {
    if p.can(Permission::Write, &Target::Catalog)
        || !matches!(inherit_of(spec), "override" | "disable")
    {
        return Ok(());
    }
    // Äußere Ebenen des Ziels (Ebene, Name).
    let mut outer: Vec<(ScopeLevel, Option<String>)> = vec![(ScopeLevel::Global, None)];
    match binding_target(st, spec).await? {
        Target::InOrganization(o) if spec.scope.level == ScopeLevel::ApiKey => {
            outer.push((ScopeLevel::Organization, Some(o)));
        }
        Target::InTeam { organization, team } => {
            outer.push((ScopeLevel::Organization, Some(organization)));
            if spec.scope.level == ScopeLevel::ApiKey {
                outer.push((ScopeLevel::Team, Some(team)));
            }
        }
        _ => {}
    }
    let locked: Vec<String> = st
        .store
        .list::<PluginBindingSpec>()
        .await?
        .into_iter()
        .filter(|b| {
            b.spec.plugin == spec.plugin
                && b.spec.enabled
                && b.spec.locked
                && outer.iter().any(|(l, t)| {
                    b.spec.scope.level == *l && (t.is_none() || b.spec.scope.target == *t)
                })
        })
        .map(|b| b.metadata.name)
        .collect();
    if locked.is_empty() {
        Ok(())
    } else {
        Err(Problem::forbidden(format!(
            "Plugin {:?} ist auf einer äußeren Ebene gesperrt (locked: {locked:?}) – \
             inherit: {} ist hier nicht erlaubt",
            spec.plugin,
            inherit_of(spec)
        )))
    }
}

/// Routen der Plugin-Bindungen (ohne Authentifizierungsschicht).
pub(crate) fn router() -> axum::Router<AdminState> {
    use axum::routing::get;
    axum::Router::new()
        .route(
            "/admin/api/v1/plugin-bindings",
            get(list_plugin_bindings).post(create_plugin_binding),
        )
        .route(
            "/admin/api/v1/plugin-bindings/{name}",
            get(get_plugin_binding)
                .put(put_plugin_binding)
                .delete(delete_plugin_binding),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigw_core::resource::JsonObject;
    use serde_json::json;

    fn spec(plugin: &str, v: serde_json::Value) -> PluginBindingSpec {
        PluginBindingSpec {
            scope: aigw_core::resource::Scope::global(),
            plugin: plugin.into(),
            order: 0,
            enabled: true,
            inherit: None,
            locked: false,
            fail_mode: None,
            timeout_ms: None,
            output_reject_mode: None,
            config: match v {
                serde_json::Value::Object(m) => m,
                _ => JsonObject::new(),
            },
        }
    }

    #[test]
    fn check_reports_plugin_errors() {
        let strict = UpstreamPolicy::strict();
        assert!(check(&strict, &spec("pii", json!({"action": "mask"}))).is_ok());
        let e = check(&strict, &spec("pii", json!({"acton": "mask"}))).unwrap_err();
        assert!(e.starts_with("Plugin-Bindung (pii): "), "{e}");
        assert!(e.contains("acton"), "{e}");
        assert!(check(&strict, &spec("gibt-es-nicht", json!({}))).is_err());
        // Alter Steuerschlüssel mit ungültigem Wert (nicht übernehmbar) bleibt ein Fehler.
        assert!(check(&strict, &spec("pii", json!({"failMode": "maybe"}))).is_err());
        // Steuerfelder (F117) werden mitgeprüft.
        let mut s = spec("pii", json!({}));
        s.timeout_ms = Some(0);
        assert!(check(&strict, &s).is_err());
        s.timeout_ms = Some(500);
        s.fail_mode = Some(aigw_core::resource::PluginFailMode::Open);
        assert!(check(&strict, &s).is_ok());
        let e = check(
            &strict,
            &spec("webhook", json!({"url": "https://169.254.169.254/latest"})),
        )
        .unwrap_err();
        assert!(e.contains("url"), "{e}");
    }
}
