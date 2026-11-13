// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Deployments (AIGW-65): konkrete Anbieter-Modelle hinter einer Modellgruppe.
//!
//! Prüfungen beim Schreiben (gemeinsam mit GitOps in [`aigw_core::validate`], zusätzlich zu
//! Verweisen, die der Store auflöst): Endpunkt gegen die SSRF-Regeln
//! ([`aigw_core::upstream`]), Pflichtfelder je Anbieter (Azure: Deployment-Name und
//! `settings.apiVersion`), Zugangsdaten desselben Anbieters, Gewicht und Kapazität > 0.

// `#[utoipa::path(params(…))]` erzeugt für IntoParams-Typen qualifizierte Pfade.
#![allow(unused_qualifications)]

use aigw_core::resource::{DeploymentSpec, Id, Object, ObjectMeta};
use aigw_core::types::ProviderKind;
use aigw_core::validate;
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

/// Filter für Deployments.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query, rename_all = "camelCase")]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DeploymentQuery {
    /// Seitengröße (1–500, Standard 50).
    pub limit: Option<u32>,
    /// Fortsetzung (`nextCursor` der vorigen Seite).
    pub cursor: Option<String>,
    /// Teilstring im Namen oder in `providerModel`.
    pub q: Option<String>,
    /// Label-Selektor `k=v,k2`.
    pub label: Option<String>,
    /// Nur Deployments dieser Modellgruppe (Name).
    pub model_group: Option<String>,
    /// Nur dieser Anbieter.
    pub provider: Option<ProviderKind>,
    /// Nur Deployments mit diesen Zugangsdaten (Name).
    pub credential: Option<String>,
    /// Nur aktive (`true`) bzw. deaktivierte (`false`).
    pub enabled: Option<bool>,
}

plain_resource!(
    spec = DeploymentSpec,
    res = Deployment,
    input = DeploymentInput,
    list = DeploymentList,
    query = DeploymentQuery,
    tag = "deployments",
    path = "/admin/api/v1/deployments",
    item_path = "/admin/api/v1/deployments/{name}",
    what = "Deployment",
    handlers = [
        list_deployments,
        create_deployment,
        get_deployment,
        put_deployment,
        delete_deployment
    ]
);

impl AdminResource for DeploymentSpec {
    type Res = Deployment;
    type Query = DeploymentQuery;
    const COLLECTION: &'static str = "deployments";

    fn target(_name: &str, _spec: &Self) -> Target {
        Target::Catalog
    }

    fn list_args(q: &Self::Query) -> ListArgs<'_> {
        ListArgs {
            limit: q.limit,
            cursor: q.cursor.as_deref(),
            q: q.q.as_deref(), // auch providerModel (Suchspalte im Store)
            label: q.label.as_deref(),
        }
    }

    fn list_filters(q: &Self::Query) -> Result<Vec<ListFilter>, Problem> {
        let mut f = Vec::new();
        f.extend(
            q.model_group
                .iter()
                .map(|g| ListFilter::ModelGroup(g.clone())),
        );
        f.extend(
            q.provider
                .map(|p| ListFilter::Provider(p.as_str().to_owned())),
        );
        f.extend(
            q.credential
                .iter()
                .map(|c| ListFilter::Credential(c.clone())),
        );
        f.extend(q.enabled.map(ListFilter::Enabled));
        Ok(f)
    }

    async fn check(
        st: &AdminState,
        _name: &str,
        spec: &Self,
        _current: Option<&Object<Self>>,
    ) -> Result<(), Problem> {
        validate::deployment(&st.upstreams, spec).map_err(Problem::invalid)?;
        // F177: die CA-Datei muss (nach Auflösen von Symlinks) in upstream.ca_dir liegen und
        // lesbar sein – geprüft auf dem Gateway selbst (GitOps per CLI prüft nur textuell).
        if let (Some(dir), Some(serde_json::Value::String(ca))) =
            (st.upstreams.ca_dir(), spec.settings.get("caFile"))
        {
            aigw_core::paths::resolve_within(dir, ca)
                .map_err(|e| Problem::invalid(format!("settings.caFile: {e}")))?;
        }
        if let Some(cred) = &spec.credential {
            // Unbekannte Zugangsdaten → 422 (wie andere Verweise).
            let c = st.store.get_credential(cred).await.map_err(|e| match e {
                aigw_store::StoreError::NotFound { kind, name } => {
                    aigw_store::StoreError::InvalidReference { kind, name }
                }
                other => other,
            })?;
            validate::deployment_credential(spec, cred, c.spec.provider)
                .map_err(Problem::invalid)?;
        }
        Ok(())
    }
}

/// Routen der Deployments (ohne Authentifizierungsschicht).
pub(crate) fn router() -> axum::Router<AdminState> {
    use axum::routing::get;
    axum::Router::new()
        .route(
            "/admin/api/v1/deployments",
            get(list_deployments).post(create_deployment),
        )
        .route(
            "/admin/api/v1/deployments/{name}",
            get(get_deployment)
                .put(put_deployment)
                .delete(delete_deployment),
        )
}
