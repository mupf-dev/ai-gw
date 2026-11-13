// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! GitOps über die Admin-API (AIGW-33, `docs/konzept/09-gitops.md`).
//!
//! - `GET /admin/api/v1/config:export` – alle Ressourcen als YAML (`application/yaml`),
//!   Berechtigung `Read`.
//! - `POST /admin/api/v1/config:apply?dryRun=&prune=&apiKeys=` – Körper: YAML-Manifeste;
//!   Antwort: Plan bzw. Ergebnis (`ApplyReport`), Berechtigung `Write` (auch für den
//!   Trockenlauf, weil er Geheimnisse des Servers auflöst und vergleicht).
//!
//! Über die API dürfen Manifeste nur Umgebungsvariablen mit Präfix `AIGW_SECRET_` verwenden
//! und Dateien nur innerhalb von `gitops.secret_dir` (F52; [`SecretPolicy::restricted`],
//! [`SecretPolicy::with_file_dir`]) – sonst könnte ein Admin-Token beliebige Variablen oder
//! Dateien des Servers in Zugangsdaten kopieren. Die CLI hat diese Einschränkung nicht.
//!
//! Berechtigung (F55): Ziel [`Target::GitOps`] – `config:export` lesen globale
//! Leserollen und `gitops`, `config:apply` (auch Trockenlauf) globaler `admin` und `gitops`.

// `#[utoipa::path(params(…))]` erzeugt für IntoParams-Typen qualifizierte Pfade.
#![allow(unused_qualifications)]

use std::fmt;
use std::sync::Arc;

use aigw_store::gitops::{
    self, ApiKeyMode, ApplyOptions, ApplyReport, GitopsError, ProcessSecrets, SECRET_ENV_PREFIX,
    SecretPolicy,
};
use aigw_store::{Keyring, Store};
use axum::extract::rejection::QueryRejection;
use axum::extract::{Extension, Query, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde::Deserialize;
use utoipa::IntoParams;

use crate::auth::{AdminAuthenticator, Permission, Principal, Target, require_principal};
use crate::problem::Problem;
use crate::upstream::UpstreamPolicy;

/// Pfad des Exports.
pub const EXPORT_PATH: &str = "/admin/api/v1/config:export";
/// Pfad von Apply/Trockenlauf.
pub const APPLY_PATH: &str = "/admin/api/v1/config:apply";
/// Medientyp der Manifeste.
pub const YAML: &str = "application/yaml";

/// Zustand der GitOps-Routen.
#[derive(Clone)]
pub struct GitopsState {
    /// Datenbank.
    pub store: Store,
    /// Master-Keys (Zugangsdaten verschlüsseln und vergleichen).
    pub keyring: Arc<Keyring>,
    /// Anmeldung (dieselbe wie für die übrige Admin-API).
    pub auth: Arc<dyn AdminAuthenticator>,
    /// Regeln für Endpunkte von Deployments (wie bei den Ressourcen-Routen).
    pub upstreams: UpstreamPolicy,
    /// `gitops.secret_dir` (F52): `valueFrom.file` nur innerhalb dieses Verzeichnisses;
    /// ohne sind Dateien über die Admin-API verboten.
    pub secret_dir: Option<std::path::PathBuf>,
}

impl fmt::Debug for GitopsState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitopsState")
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

/// Parameter des Exports.
#[derive(Debug, Deserialize, IntoParams)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct ExportQuery {
    /// `ignore` (Standard, F4): API-Keys weglassen; `include`: ohne Klartext exportieren.
    #[serde(default)]
    api_keys: ApiKeyMode,
}

/// Parameter von Apply.
#[derive(Debug, Deserialize, IntoParams)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct ApplyQuery {
    /// Nur planen, nichts schreiben.
    #[serde(default)]
    dry_run: bool,
    /// Ressourcen löschen, die nicht im Manifest stehen.
    #[serde(default)]
    prune: bool,
    /// Umgang mit API-Keys (`ignore` = Standard, F4 | `include`).
    #[serde(default)]
    api_keys: ApiKeyMode,
}

fn problem(e: GitopsError) -> Problem {
    match e {
        GitopsError::Invalid(issues) => Problem::invalid(
            issues
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        e @ GitopsError::Concurrent { .. } => Problem::conflict(e.to_string()),
        GitopsError::Store(s) => s.into(),
        other => {
            tracing::error!(error = %other, "GitOps: unerwarteter Fehler");
            Problem::internal()
        }
    }
}

/// Exportiert alle Ressourcen als YAML-Mehrfachdokument (ohne Geheimnisse und Server-Felder).
#[utoipa::path(
    get, path = "/admin/api/v1/config:export", tag = "gitops", params(ExportQuery),
    responses(
        (status = 200, description = "Manifeste (aigw/v1alpha1)", body = String, content_type = "application/yaml"),
        (status = 400, description = "Ungültige Parameter", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn export_config(
    State(st): State<GitopsState>,
    Extension(p): Extension<Principal>,
    q: Result<Query<ExportQuery>, QueryRejection>,
) -> Result<Response, Problem> {
    p.require_on(Permission::Read, &Target::GitOps)?;
    let Query(q) = q?;
    let yaml = gitops::export_yaml(&st.store, q.api_keys)
        .await
        .map_err(problem)?;
    let mut res = yaml.into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(YAML));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(res)
}

/// Wendet Manifeste an (oder plant nur, `dryRun=true`): vollständige Validierung vorab, dann
/// **eine** Transaktion mit genau einer neuen `config_version`; alles oder nichts.
#[utoipa::path(
    post, path = "/admin/api/v1/config:apply", tag = "gitops", params(ApplyQuery),
    request_body(content = String, content_type = "application/yaml", description = "Manifeste (aigw/v1alpha1, Mehrfachdokument)"),
    responses(
        (status = 200, description = "Plan bzw. Ergebnis", body = ApplyReport),
        (status = 400, description = "Ungültige Anfrage", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Konfiguration zwischenzeitlich geändert oder DB-Konflikt", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Manifest ungültig (alle Fehler, nichts angewendet)", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn apply_config(
    State(st): State<GitopsState>,
    Extension(p): Extension<Principal>,
    q: Result<Query<ApplyQuery>, QueryRejection>,
    body: Bytes,
) -> Result<Response, Problem> {
    p.require_on(Permission::Write, &Target::GitOps)?;
    let Query(q) = q?;
    let text =
        std::str::from_utf8(&body).map_err(|_| Problem::bad_request("Körper ist kein UTF-8"))?;
    let docs = gitops::parse_documents(text).map_err(problem)?;
    let opts = ApplyOptions {
        dry_run: q.dry_run,
        prune: q.prune,
        api_keys: q.api_keys,
        actor: format!("{}:{}", gitops::GITOPS_ACTOR, p.actor()),
        secrets: SecretPolicy::restricted(SECRET_ENV_PREFIX).with_file_dir(st.secret_dir.clone()),
        upstreams: st.upstreams.clone(),
        group_settings: Some(crate::resources::group_settings),
        plugin_bindings: Some(crate::plugin_bindings::check),
    };
    let report = gitops::apply(&st.store, &st.keyring, &docs, &opts, &ProcessSecrets)
        .await
        .map_err(problem)?;
    Ok(Json(report).into_response())
}

/// Router der GitOps-Routen (mit Anmeldung).
pub fn router(state: GitopsState) -> Router {
    Router::new()
        .route(EXPORT_PATH, get(export_config))
        .route(APPLY_PATH, post(apply_config))
        .route_layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state.auth),
            require_principal,
        ))
        .with_state(state)
}
