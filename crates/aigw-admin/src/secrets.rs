// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Allgemeine Geheimnisse (`Secret`, F149): `/admin/api/v1/secrets`.
//!
//! Für Webhook-HMAC-Schlüssel (`secret: {secret: <name>}`), Guardrail-Dienste
//! (`content-safety`: `secret: <name>`) und MCP-Server (`spec.secret`) – statt des früheren
//! Behelfs über `ProviderCredential`. Regeln wie bei Zugangsdaten:
//!
//! - Werte nur im Request (`spec.data.<feld>.value`), sofort mit dem Master-Key verschlüsselt;
//!   keine Antwort, kein Fehlertext und kein Audit-Eintrag enthält sie.
//! - `valueFrom` ist über die Admin-API nicht erlaubt (würde Umgebung/Dateisystem des Servers
//!   lesen); GitOps löst `valueFrom` auf dem eigenen Rechner auf.
//! - `PUT` ersetzt alles, `PATCH` (JSON Merge Patch) ändert einzelne Felder (`null` entfernt).
//! - Rechte wie Zugangsdaten: lesen jede wirksame Rolle (nur Metadaten), schreiben nur ein
//!   globaler `admin`.

// `#[utoipa::path(params(…))]` erzeugt für IntoParams-Typen qualifizierte Pfade.
#![allow(unused_qualifications)]

use std::collections::BTreeMap;
use std::sync::Arc;

use aigw_core::resource::{self as core, Id, ObjectMeta, SecretRef, SecretSpec, SecretString};
use aigw_store::Keyring;
use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse as _;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::AdminState;
use crate::auth::{Permission, Principal, Target};
use crate::problem::Problem;
use crate::resources::{
    ApiResult, BASE, ListArgs, St, check_if_match, created_at, etag_of, json_with, list_query,
    no_store, page_parts, same_name,
};

/// Geheimnis (gelesen) – **ohne** Werte.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Secret {
    /// ID (UUIDv7).
    #[schema(value_type = String, format = Uuid)]
    pub id: Id,
    /// Metadaten.
    pub metadata: ObjectMeta,
    /// Beschreibung und Feldnamen.
    pub spec: SecretSummary,
    /// Vom Server verwaltete Angaben.
    pub status: SecretStatus,
}

/// Beschreibung und Namen der gespeicherten Felder.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretSummary {
    /// Beschreibung.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Namen der gespeicherten Felder (sortiert), z. B. `["value"]`.
    pub fields: Vec<String>,
}

/// Status eines Geheimnisses.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretStatus {
    /// ID des Master-Keys, mit dem verschlüsselt ist (Rotation).
    pub key_id: String,
}

impl From<core::Secret> for Secret {
    fn from(s: core::Secret) -> Self {
        Self {
            id: s.id,
            metadata: s.metadata,
            status: SecretStatus {
                key_id: s.spec.key_id,
            },
            spec: SecretSummary {
                description: s.spec.description,
                fields: s.spec.fields,
            },
        }
    }
}

/// Seite einer Liste: Geheimnisse.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretList {
    /// Einträge (nach Name sortiert).
    pub items: Vec<Secret>,
    /// Cursor der nächsten Seite (`?cursor=`), fehlt auf der letzten Seite.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Ein Geheimwert in der Eingabe.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SecretValue {
    /// Klartext (wird sofort verschlüsselt, nie wieder ausgegeben).
    pub value: SecretString,
}

/// Spezifikation beim Anlegen/Ersetzen.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SecretInputSpec {
    /// Beschreibung (Zweck, Besitzer).
    #[serde(default)]
    pub description: Option<String>,
    /// Felder, z. B. `{"value": {"value": "…"}}`.
    pub data: BTreeMap<String, SecretValue>,
}

/// Geheimnis (Eingabe für Anlegen und Ersetzen).
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SecretInput {
    /// Metadaten (`name`, `labels`).
    pub metadata: ObjectMeta,
    /// Beschreibung und Werte.
    pub spec: SecretInputSpec,
}

/// Metadaten im `PATCH`.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SecretPatchMeta {
    /// Muss, falls angegeben, zum Pfad passen.
    #[serde(default)]
    pub name: Option<String>,
    /// Ersetzt die Labels vollständig (weglassen = unverändert).
    #[serde(default)]
    pub labels: Option<BTreeMap<String, String>>,
}

/// Spezifikation im `PATCH`.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SecretPatchSpec {
    /// Neue Beschreibung (`null` entfernt sie, weglassen = unverändert).
    #[serde(default, with = "double_option")]
    #[schema(value_type = Option<String>)]
    pub description: Option<Option<String>>,
    /// Zu ändernde Felder: Wert setzt, `null` entfernt; nicht genannte Felder bleiben.
    #[serde(default)]
    #[schema(value_type = BTreeMap<String, SecretValue>)]
    pub data: BTreeMap<String, Option<SecretValue>>,
}

mod double_option {
    use serde::{Deserialize, Deserializer};

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Option<String>>, D::Error> {
        Option::<String>::deserialize(d).map(Some)
    }
}

/// Teiländerung (JSON Merge Patch).
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SecretPatch {
    /// Metadaten.
    #[serde(default)]
    pub metadata: Option<SecretPatchMeta>,
    /// Spezifikation.
    #[serde(default)]
    pub spec: Option<SecretPatchSpec>,
}

/// Filter für Geheimnisse.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct SecretQuery {
    /// Seitengröße (1–500, Standard 50).
    pub limit: Option<u32>,
    /// Fortsetzung (`nextCursor` der vorigen Seite).
    pub cursor: Option<String>,
    /// Teilstring im Namen.
    pub q: Option<String>,
    /// Label-Selektor `k=v,k2`.
    pub label: Option<String>,
}

/// JSON-Fehler ohne Geheimwerte (serde nennt bei Typfehlern den Wert).
fn redacted(r: JsonRejection) -> Problem {
    let mut p = Problem::from(r);
    if let Some(d) = p.detail.as_mut()
        && let Some(pos) = d.find("string \"")
    {
        d.truncate(pos);
        d.push_str("string (Wert ausgelassen)");
    }
    p
}

fn keyring(st: &AdminState) -> Result<&Arc<Keyring>, Problem> {
    st.keyring.as_ref().ok_or_else(|| {
        tracing::error!("Admin-API ohne Master-Key: Geheimnisse können nicht geschrieben werden");
        Problem::internal()
    })
}

fn check_value(field: &str, v: &SecretValue) -> Result<(), Problem> {
    if v.value.expose().is_empty() {
        return Err(Problem::invalid(format!(
            "spec.data.{field}: Wert darf nicht leer sein"
        )));
    }
    if v.value.expose().len() > 64 * 1024 {
        return Err(Problem::invalid(format!(
            "spec.data.{field}: höchstens 64 KiB"
        )));
    }
    Ok(())
}

fn validate_input(spec: &SecretInputSpec) -> Result<(), Problem> {
    for (f, v) in &spec.data {
        check_value(f, v)?;
    }
    aigw_store::validate_secret(
        spec.description.as_deref(),
        spec.data.keys().map(String::as_str),
    )
    .map_err(Problem::invalid)
}

fn to_store_spec(spec: SecretInputSpec) -> SecretSpec {
    SecretSpec {
        description: spec.description,
        data: spec
            .data
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    SecretRef {
                        value: Some(v.value),
                        value_from: None,
                    },
                )
            })
            .collect(),
    }
}

fn respond(status: StatusCode, s: core::Secret) -> axum::response::Response {
    let view = Secret::from(s);
    no_store(json_with(status, Some(etag_of(&view)), &view))
}

/// Listet Geheimnisse (ohne Werte).
#[utoipa::path(
    get, path = "/admin/api/v1/secrets", tag = "secrets",
    params(SecretQuery),
    responses(
        (status = 200, description = "Seite", body = SecretList),
        (status = 400, description = "Ungültige Parameter", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn list_secrets(
    State(st): St,
    Extension(p): Extension<Principal>,
    q: Result<Query<SecretQuery>, QueryRejection>,
) -> ApiResult {
    p.require_on(Permission::Read, &Target::Catalog)?;
    let Query(q) = q?;
    let args = ListArgs {
        limit: q.limit,
        cursor: q.cursor.as_deref(),
        q: q.q.as_deref(),
        label: q.label.as_deref(),
    };
    let lq = list_query(&args, Vec::new(), aigw_store::AuditScope::All)?;
    let (items, next_cursor) = page_parts::<_, Secret>(st.store.list_secrets_page(&lq).await?);
    Ok(json_with(
        StatusCode::OK,
        None,
        &SecretList { items, next_cursor },
    ))
}

/// Legt ein Geheimnis an (Werte werden verschlüsselt, Antwort ohne Werte).
#[utoipa::path(
    post, path = "/admin/api/v1/secrets", tag = "secrets",
    request_body = SecretInput,
    responses(
        (status = 201, description = "Angelegt (ohne Werte)", body = Secret,
            headers(("ETag" = String, description = "Version"), ("Location" = String, description = "Pfad"))),
        (status = 400, description = "Ungültige Anfrage", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Name bereits vergeben", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Ungültige Eingabe", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn create_secret(
    State(st): St,
    Extension(p): Extension<Principal>,
    body: Result<Json<SecretInput>, JsonRejection>,
) -> ApiResult {
    p.require_on(Permission::Write, &Target::Catalog)?;
    let Json(input) = body.map_err(redacted)?;
    validate_input(&input.spec)?;
    let keyring = keyring(&st)?;
    let _guard = st.write_lock.lock().await;
    let s = st
        .store
        .create_secret(
            p.actor(),
            keyring,
            input.metadata,
            to_store_spec(input.spec),
        )
        .await?;
    let location = format!("{BASE}/secrets/{}", s.metadata.name);
    Ok(created_at(respond(StatusCode::CREATED, s), &location))
}

/// Liest ein Geheimnis (ohne Werte).
#[utoipa::path(
    get, path = "/admin/api/v1/secrets/{name}", tag = "secrets",
    params(("name" = String, Path, description = "Name")),
    responses(
        (status = 200, description = "Gefunden", body = Secret,
            headers(("ETag" = String, description = "Version für If-Match"))),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn get_secret(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
) -> ApiResult {
    p.require_on(Permission::Read, &Target::Catalog)?;
    Ok(respond(StatusCode::OK, st.store.get_secret(&name).await?))
}

/// Ersetzt Labels, Beschreibung und **alle** Werte.
#[utoipa::path(
    put, path = "/admin/api/v1/secrets/{name}", tag = "secrets",
    request_body = SecretInput,
    params(
        ("name" = String, Path, description = "Name"),
        ("If-Match" = Option<String>, Header, description = "ETag aus dem letzten Lesen (optimistische Sperre)"),
    ),
    responses(
        (status = 200, description = "Ersetzt (ohne Werte)", body = Secret,
            headers(("ETag" = String, description = "Neue Version"))),
        (status = 400, description = "Ungültige Anfrage", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "ETag passt nicht", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Ungültige Eingabe", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn put_secret(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Result<Json<SecretInput>, JsonRejection>,
) -> ApiResult {
    p.require_on(Permission::Write, &Target::Catalog)?;
    let Json(input) = body.map_err(redacted)?;
    let meta = same_name(&name, input.metadata)?;
    validate_input(&input.spec)?;
    let keyring = keyring(&st)?;
    let _guard = st.write_lock.lock().await;
    let current = st.store.get_secret(&name).await?;
    check_if_match(&headers, &etag_of(&Secret::from(current)))?;
    let s = st
        .store
        .update_secret(p.actor(), keyring, meta, to_store_spec(input.spec))
        .await?;
    Ok(respond(StatusCode::OK, s))
}

/// Ändert einzelne Felder (JSON Merge Patch).
#[utoipa::path(
    patch, path = "/admin/api/v1/secrets/{name}", tag = "secrets",
    request_body(content = SecretPatch, content_type = "application/merge-patch+json"),
    params(
        ("name" = String, Path, description = "Name"),
        ("If-Match" = Option<String>, Header, description = "ETag aus dem letzten Lesen (optimistische Sperre)"),
    ),
    responses(
        (status = 200, description = "Geändert (ohne Werte)", body = Secret,
            headers(("ETag" = String, description = "Neue Version"))),
        (status = 400, description = "Ungültige Anfrage", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "ETag passt nicht", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Ungültige Eingabe", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn patch_secret(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Result<Json<SecretPatch>, JsonRejection>,
) -> ApiResult {
    p.require_on(Permission::Write, &Target::Catalog)?;
    let Json(patch) = body.map_err(redacted)?;
    let meta = patch.metadata.unwrap_or_default();
    if let Some(n) = &meta.name
        && *n != name
    {
        return Err(Problem::invalid(format!(
            "metadata.name {n:?} passt nicht zum Pfad {name:?} (Umbenennen nicht möglich)"
        )));
    }
    let spec = patch.spec.unwrap_or_default();
    for (f, v) in &spec.data {
        if let Some(v) = v {
            check_value(f, v)?;
        }
    }
    let keyring = keyring(&st)?;
    let _guard = st.write_lock.lock().await;
    let current = st.store.get_secret(&name).await?;
    check_if_match(&headers, &etag_of(&Secret::from(current.clone())))?;
    let mut fields: std::collections::BTreeSet<&str> =
        current.spec.fields.iter().map(String::as_str).collect();
    for (f, v) in &spec.data {
        if v.is_some() {
            fields.insert(f);
        } else {
            fields.remove(f.as_str());
        }
    }
    let description = match &spec.description {
        Some(d) => d.as_deref(),
        None => current.spec.description.as_deref(),
    };
    aigw_store::validate_secret(description, fields).map_err(Problem::invalid)?;
    let changes = spec
        .data
        .into_iter()
        .map(|(k, v)| (k, v.map(|v| v.value)))
        .collect();
    let s = st
        .store
        .patch_secret(
            p.actor(),
            keyring,
            &name,
            meta.labels,
            spec.description,
            changes,
        )
        .await?;
    Ok(respond(StatusCode::OK, s))
}

/// Löscht ein Geheimnis (`409`, solange MCP-Server es nutzen).
#[utoipa::path(
    delete, path = "/admin/api/v1/secrets/{name}", tag = "secrets",
    params(
        ("name" = String, Path, description = "Name"),
        ("If-Match" = Option<String>, Header, description = "ETag aus dem letzten Lesen (optimistische Sperre)"),
    ),
    responses(
        (status = 204, description = "Gelöscht"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Wird noch verwendet", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "ETag passt nicht", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn delete_secret(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> ApiResult {
    p.require_on(Permission::Write, &Target::Catalog)?;
    let _guard = st.write_lock.lock().await;
    let current = st.store.get_secret(&name).await?;
    check_if_match(&headers, &etag_of(&Secret::from(current)))?;
    st.store
        .delete(p.actor(), core::ResourceKind::Secret, &name)
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Routen der Geheimnisse (ohne Authentifizierungsschicht).
pub(crate) fn router() -> axum::Router<AdminState> {
    use axum::routing::get;
    axum::Router::new()
        .route("/admin/api/v1/secrets", get(list_secrets).post(create_secret))
        .route(
            "/admin/api/v1/secrets/{name}",
            get(get_secret)
                .put(put_secret)
                .patch(patch_secret)
                .delete(delete_secret),
        )
}
