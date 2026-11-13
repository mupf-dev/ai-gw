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
//! - **Rechte:** Plugin-Bindungen sind sicherheitsrelevant (Guardrails, Webhooks) – lesen darf
//!   jede wirksame Rolle, schreiben nur ein globaler `admin` (wie der Modellkatalog).

// `#[utoipa::path(params(…))]` erzeugt für IntoParams-Typen qualifizierte Pfade.
#![allow(unused_qualifications)]

use std::sync::LazyLock;

use aigw_core::resource::{Id, JsonObject, Object, ObjectMeta, PluginBindingSpec};
use aigw_core::upstream::UpstreamPolicy;
use aigw_proxy::PluginRegistry as Registry;
use aigw_store::ListFilter;
use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::AdminState;
use crate::auth::{Principal, Target};
use crate::problem::Problem;
use crate::resources::{
    AdminResource, ApiResult, ListArgs, St, create_impl, delete_impl, get_impl, json_with,
    list_impl, plain_resource, put_impl,
};

/// Prüft eine Bindung mit der Registry der Plugin-Kette und den SSRF-Regeln `policy`
/// (`admin.*`). Auch als `ApplyOptions::plugin_bindings` für GitOps
/// (`aigw config validate|diff|apply`, `config:apply`).
pub fn check(policy: &UpstreamPolicy, plugin: &str, config: &JsonObject) -> Result<(), String> {
    Registry::for_validation(policy)
        .validate(plugin, config)
        .map_err(|e| format!("Plugin-Bindung ({plugin}): {e}"))
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
    let e = DEFAULTS.effective(&o.spec.plugin, &o.spec.config).ok()?;
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

    async fn check(
        st: &AdminState,
        _name: &str,
        spec: &Self,
        _current: Option<&Object<Self>>,
    ) -> Result<(), Problem> {
        check(&st.upstreams, &spec.plugin, &spec.config).map_err(Problem::invalid)
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
    use serde_json::json;

    fn obj(v: serde_json::Value) -> JsonObject {
        match v {
            serde_json::Value::Object(m) => m,
            _ => JsonObject::new(),
        }
    }

    #[test]
    fn check_reports_plugin_errors() {
        let strict = UpstreamPolicy::strict();
        assert!(check(&strict, "pii", &obj(json!({"action": "mask"}))).is_ok());
        let e = check(&strict, "pii", &obj(json!({"acton": "mask"}))).unwrap_err();
        assert!(e.starts_with("Plugin-Bindung (pii): "), "{e}");
        assert!(e.contains("acton"), "{e}");
        assert!(check(&strict, "gibt-es-nicht", &JsonObject::new()).is_err());
        assert!(check(&strict, "pii", &obj(json!({"failMode": "maybe"}))).is_err());
        let e = check(
            &strict,
            "webhook",
            &obj(json!({"url": "https://169.254.169.254/latest"})),
        )
        .unwrap_err();
        assert!(e.contains("url"), "{e}");
    }
}
