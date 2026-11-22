// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Zugangsdaten der Anbieter (AIGW-65).
//!
//! - Geheimwerte kommen **nur** im Request (`spec.data.<feld>.value`), werden sofort mit dem
//!   Master-Key verschlüsselt (AES-256-GCM, `aigw_store::crypto`) und erscheinen in keiner
//!   Antwort, keinem Fehlertext und keinem Audit-Eintrag. Antworten zeigen Anbieter,
//!   Feldnamen, `status.hasSecret`, `status.keyId` und die Zeitstempel.
//! - `valueFrom` (Umgebungsvariable/Datei) ist über die Admin-API **nicht** erlaubt: Es würde
//!   Umgebung und Dateisystem des Servers lesen. GitOps (`aigw config apply`) löst `valueFrom`
//!   auf dem eigenen Rechner auf und sendet den Wert.
//! - `PUT` ersetzt alle Felder; `PATCH` (JSON Merge Patch) ändert einzelne Felder, nicht
//!   genannte behalten ihren Wert, `null` entfernt ein Feld.
//! - `spec.entra` (AIGW-84, nur `azure-openai`/`anthropic`): Anmeldung über Microsoft Entra ID
//!   (Managed/Workload Identity, Service Principal mit Secret oder Zertifikat). Nicht geheim,
//!   wird ausgegeben; Deployments mit `credential: <name>` übernehmen sie. Bei Managed/Workload
//!   Identity bleibt `spec.data` leer.

// `#[utoipa::path(params(…))]` erzeugt für IntoParams-Typen qualifizierte Pfade.
#![allow(unused_qualifications)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use aigw_core::resource::{
    self as core, DeploymentSpec, EntraAuth, Id, ObjectMeta, ProviderCredentialSpec, SecretRef,
    SecretString,
};
use aigw_core::types::ProviderKind;
use aigw_core::validate;
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

// ---------------------------------------------------------------------------------------------
// Typen
// ---------------------------------------------------------------------------------------------

/// Zugangsdaten (gelesen) – **ohne** Werte.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCredential {
    /// ID (UUIDv7).
    #[schema(value_type = String, format = Uuid)]
    pub id: Id,
    /// Metadaten (mit `createdAt`/`updatedAt`; `updatedAt` ändert sich auch beim Ersetzen
    /// eines Werts).
    pub metadata: ObjectMeta,
    /// Anbieter und Feldnamen.
    pub spec: ProviderCredentialSummary,
    /// Vom Server verwaltete Angaben.
    pub status: ProviderCredentialStatus,
}

/// Anbieter und Namen der gespeicherten Felder.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCredentialSummary {
    /// Anbieter.
    pub provider: ProviderKind,
    /// Entra-ID-Anmeldung (AIGW-84; nicht geheim).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entra: Option<EntraAuth>,
    /// Namen der gespeicherten Felder (sortiert), z. B. `["apiKey"]`.
    pub fields: Vec<String>,
}

/// Status der Zugangsdaten.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCredentialStatus {
    /// Mindestens ein Geheimwert ist gespeichert.
    pub has_secret: bool,
    /// ID des Master-Keys, mit dem verschlüsselt ist (Rotation).
    pub key_id: String,
}

impl From<core::ProviderCredential> for ProviderCredential {
    fn from(c: core::ProviderCredential) -> Self {
        Self {
            id: c.id,
            metadata: c.metadata,
            status: ProviderCredentialStatus {
                has_secret: !c.spec.fields.is_empty(),
                key_id: c.spec.key_id,
            },
            spec: ProviderCredentialSummary {
                provider: c.spec.provider,
                entra: c.spec.entra,
                fields: c.spec.fields,
            },
        }
    }
}

/// Seite einer Liste: Zugangsdaten.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCredentialList {
    /// Einträge (nach Name sortiert).
    pub items: Vec<ProviderCredential>,
    /// Cursor der nächsten Seite (`?cursor=`), fehlt auf der letzten Seite.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Ein Geheimwert in der Eingabe.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CredentialValue {
    /// Klartext (wird sofort verschlüsselt, nie wieder ausgegeben).
    pub value: SecretString,
}

/// Spezifikation beim Anlegen/Ersetzen.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderCredentialInputSpec {
    /// Anbieter.
    pub provider: ProviderKind,
    /// Entra-ID-Anmeldung (AIGW-84; nur `azure-openai` und `anthropic`).
    #[serde(default)]
    pub entra: Option<EntraAuth>,
    /// Felder, z. B. `{"apiKey": {"value": "sk-…"}}` (mit `entra` und Managed/Workload
    /// Identity leer).
    #[serde(default)]
    pub data: BTreeMap<String, CredentialValue>,
}

/// Zugangsdaten (Eingabe für Anlegen und Ersetzen).
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderCredentialInput {
    /// Metadaten (`name`, `labels`).
    pub metadata: ObjectMeta,
    /// Anbieter und Werte.
    pub spec: ProviderCredentialInputSpec,
}

/// Metadaten im `PATCH`.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CredentialPatchMeta {
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
pub struct CredentialPatchSpec {
    /// Neuer Anbieter (weglassen = unverändert).
    #[serde(default)]
    pub provider: Option<ProviderKind>,
    /// Entra-ID-Anmeldung als Ganzes ersetzen (`null` entfernt sie, weglassen = unverändert).
    #[serde(default, with = "double_option")]
    #[schema(value_type = Option<EntraAuth>)]
    pub entra: Option<Option<EntraAuth>>,
    /// Zu ändernde Felder: Wert setzt, `null` entfernt; nicht genannte Felder bleiben.
    #[serde(default)]
    #[schema(value_type = BTreeMap<String, CredentialValue>)]
    pub data: BTreeMap<String, Option<CredentialValue>>,
}

mod double_option {
    use aigw_core::resource::EntraAuth;
    use serde::{Deserialize, Deserializer};

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Option<EntraAuth>>, D::Error> {
        Option::<EntraAuth>::deserialize(d).map(Some)
    }
}

/// Teiländerung (JSON Merge Patch).
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderCredentialPatch {
    /// Metadaten.
    #[serde(default)]
    pub metadata: Option<CredentialPatchMeta>,
    /// Spezifikation.
    #[serde(default)]
    pub spec: Option<CredentialPatchSpec>,
}

/// Filter für Zugangsdaten.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct ProviderCredentialQuery {
    /// Seitengröße (1–500, Standard 50).
    pub limit: Option<u32>,
    /// Fortsetzung (`nextCursor` der vorigen Seite).
    pub cursor: Option<String>,
    /// Teilstring im Namen.
    pub q: Option<String>,
    /// Label-Selektor `k=v,k2`.
    pub label: Option<String>,
    /// Nur dieser Anbieter.
    pub provider: Option<ProviderKind>,
}

// ---------------------------------------------------------------------------------------------
// Prüfungen
// ---------------------------------------------------------------------------------------------

/// JSON-Fehler ohne Geheimwerte: serde nennt bei Typfehlern den Wert (`invalid type:
/// string "sk-…"`), der hier abgeschnitten wird.
fn redacted(r: JsonRejection) -> Problem {
    let mut p = Problem::from(r);
    redact_detail(&mut p);
    p
}

fn redact_detail(p: &mut Problem) {
    if let Some(d) = p.detail.as_mut()
        && let Some(pos) = d.find("string \"")
    {
        d.truncate(pos);
        d.push_str("string (Wert ausgelassen)");
    }
}

fn keyring(st: &AdminState) -> Result<&Arc<Keyring>, Problem> {
    st.keyring.as_ref().ok_or_else(|| {
        tracing::error!("Admin-API ohne Master-Key: Zugangsdaten können nicht geschrieben werden");
        Problem::internal()
    })
}

/// Prüft die resultierende Feldmenge gegen die Vorgaben des Anbieters.
#[cfg(test)]
fn check_fields<'a>(
    provider: ProviderKind,
    fields: impl IntoIterator<Item = &'a str>,
) -> Result<(), Problem> {
    validate::credential_fields(provider, fields).map_err(Problem::invalid)
}

/// Prüft Anbieter, Entra-ID-Anmeldung und resultierende Feldmenge (AIGW-84).
fn check_credential<'a>(
    st: &AdminState,
    provider: ProviderKind,
    entra: Option<&EntraAuth>,
    fields: impl IntoIterator<Item = &'a str>,
) -> Result<(), Problem> {
    validate::credential(&st.upstreams, provider, entra, fields).map_err(Problem::invalid)
}

fn check_value(field: &str, v: &CredentialValue) -> Result<(), Problem> {
    validate::credential_value(field, v.value.expose()).map_err(Problem::invalid)
}

/// Ein Wechsel von Anbieter oder Entra-ID-Anmeldung darf keine ungültigen Deployments
/// zurücklassen (Kompatibilitätsliste F65, zusammengeführte Einstellungen AIGW-84).
async fn check_users(
    st: &AdminState,
    name: &str,
    provider: ProviderKind,
    entra: Option<&EntraAuth>,
) -> Result<(), Problem> {
    let mut problems = Vec::new();
    for d in st.store.list::<DeploymentSpec>().await? {
        if d.spec.credential.as_deref() != Some(name) {
            continue;
        }
        if let Err(m) =
            validate::deployment_credential_auth(&st.upstreams, &d.spec, name, provider, entra)
        {
            problems.push(format!("{}: {m}", d.metadata.name));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(Problem::conflict(format!(
            "Änderung nicht möglich – Deployments mit diesen Zugangsdaten würden ungültig: {}",
            problems.join("; ")
        )))
    }
}

fn to_store_spec(spec: ProviderCredentialInputSpec) -> ProviderCredentialSpec {
    ProviderCredentialSpec {
        provider: spec.provider,
        entra: spec.entra,
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

fn validate_input(st: &AdminState, spec: &ProviderCredentialInputSpec) -> Result<(), Problem> {
    for (f, v) in &spec.data {
        check_value(f, v)?;
    }
    check_credential(
        st,
        spec.provider,
        spec.entra.as_ref(),
        spec.data.keys().map(String::as_str),
    )
}

fn respond(status: StatusCode, c: core::ProviderCredential) -> axum::response::Response {
    let view = ProviderCredential::from(c);
    no_store(json_with(status, Some(etag_of(&view)), &view))
}

// ---------------------------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------------------------

/// Listet Zugangsdaten (ohne Werte).
#[utoipa::path(
    get, path = "/admin/api/v1/provider-credentials", tag = "provider-credentials",
    params(ProviderCredentialQuery),
    responses(
        (status = 200, description = "Seite", body = ProviderCredentialList),
        (status = 400, description = "Ungültige Parameter", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn list_credentials(
    State(st): St,
    Extension(p): Extension<Principal>,
    q: Result<Query<ProviderCredentialQuery>, QueryRejection>,
) -> ApiResult {
    p.require_on(Permission::Read, &Target::Catalog)?;
    let Query(q) = q?;
    let args = ListArgs {
        limit: q.limit,
        cursor: q.cursor.as_deref(),
        q: q.q.as_deref(),
        label: q.label.as_deref(),
    };
    let filters = q
        .provider
        .map(|p| aigw_store::ListFilter::Provider(p.as_str().to_owned()))
        .into_iter()
        .collect();
    let lq = list_query(&args, filters, aigw_store::AuditScope::All)?;
    let (items, next_cursor) =
        page_parts::<_, ProviderCredential>(st.store.list_credentials_page(&lq).await?);
    Ok(json_with(
        StatusCode::OK,
        None,
        &ProviderCredentialList { items, next_cursor },
    ))
}

/// Legt Zugangsdaten an (Werte werden verschlüsselt, Antwort ohne Werte).
#[utoipa::path(
    post, path = "/admin/api/v1/provider-credentials", tag = "provider-credentials",
    request_body = ProviderCredentialInput,
    responses(
        (status = 201, description = "Angelegt (ohne Werte)", body = ProviderCredential,
            headers(("ETag" = String, description = "Version"), ("Location" = String, description = "Pfad"))),
        (status = 400, description = "Ungültige Anfrage", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Name bereits vergeben", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Ungültige Eingabe (Pflichtfelder des Anbieters, valueFrom)", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn create_credential(
    State(st): St,
    Extension(p): Extension<Principal>,
    body: Result<Json<ProviderCredentialInput>, JsonRejection>,
) -> ApiResult {
    p.require_on(Permission::Write, &Target::Catalog)?;
    let Json(input) = body.map_err(redacted)?;
    validate_input(&st, &input.spec)?;
    let keyring = keyring(&st)?;
    let _guard = st.write_lock.lock().await;
    let c = st
        .store
        .create_credential(
            p.actor(),
            keyring,
            input.metadata,
            to_store_spec(input.spec),
        )
        .await?;
    let location = format!("{BASE}/provider-credentials/{}", c.metadata.name);
    Ok(created_at(respond(StatusCode::CREATED, c), &location))
}

/// Liest Zugangsdaten (ohne Werte).
#[utoipa::path(
    get, path = "/admin/api/v1/provider-credentials/{name}", tag = "provider-credentials",
    params(("name" = String, Path, description = "Name")),
    responses(
        (status = 200, description = "Gefunden", body = ProviderCredential,
            headers(("ETag" = String, description = "Version für If-Match"))),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn get_credential(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
) -> ApiResult {
    p.require_on(Permission::Read, &Target::Catalog)?;
    Ok(respond(
        StatusCode::OK,
        st.store.get_credential(&name).await?,
    ))
}

/// Ersetzt Labels, Anbieter und **alle** Werte.
#[utoipa::path(
    put, path = "/admin/api/v1/provider-credentials/{name}", tag = "provider-credentials",
    request_body = ProviderCredentialInput,
    params(
        ("name" = String, Path, description = "Name"),
        ("If-Match" = String, Header, description = "ETag aus dem letzten Lesen oder `*` (Pflicht, F38; fehlt er: 428)"),
    ),
    responses(
        (status = 200, description = "Ersetzt (ohne Werte)", body = ProviderCredential,
            headers(("ETag" = String, description = "Neue Version"))),
        (status = 400, description = "Ungültige Anfrage", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Anbieter- oder Anmeldungswechsel würde Deployments ungültig machen", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "ETag passt nicht", body = Problem, content_type = "application/problem+json"),
        (status = 428, description = "If-Match fehlt (F38)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Ungültige Eingabe", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn put_credential(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Result<Json<ProviderCredentialInput>, JsonRejection>,
) -> ApiResult {
    p.require_on(Permission::Write, &Target::Catalog)?;
    let Json(input) = body.map_err(redacted)?;
    let meta = same_name(&name, input.metadata)?;
    validate_input(&st, &input.spec)?;
    let keyring = keyring(&st)?;
    let _guard = st.write_lock.lock().await;
    let current = st.store.get_credential(&name).await?;
    let expect = check_if_match(
        &headers,
        &etag_of(&ProviderCredential::from(current.clone())),
        current.metadata.updated_at,
    )?;
    check_users(&st, &name, input.spec.provider, input.spec.entra.as_ref()).await?;
    let c = st
        .store
        .update_credential(p.actor(), keyring, meta, to_store_spec(input.spec), expect)
        .await?;
    Ok(respond(StatusCode::OK, c))
}

/// Ändert einzelne Felder (JSON Merge Patch): angegebene Werte ersetzen, `null` entfernt,
/// nicht genannte Felder behalten ihren gespeicherten Wert.
#[utoipa::path(
    patch, path = "/admin/api/v1/provider-credentials/{name}", tag = "provider-credentials",
    request_body(content = ProviderCredentialPatch, content_type = "application/merge-patch+json"),
    params(
        ("name" = String, Path, description = "Name"),
        ("If-Match" = String, Header, description = "ETag aus dem letzten Lesen oder `*` (Pflicht, F38; fehlt er: 428)"),
    ),
    responses(
        (status = 200, description = "Geändert (ohne Werte)", body = ProviderCredential,
            headers(("ETag" = String, description = "Neue Version"))),
        (status = 400, description = "Ungültige Anfrage", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Anbieter- oder Anmeldungswechsel würde Deployments ungültig machen", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "ETag passt nicht", body = Problem, content_type = "application/problem+json"),
        (status = 428, description = "If-Match fehlt (F38)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Ungültige Eingabe", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn patch_credential(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Result<Json<ProviderCredentialPatch>, JsonRejection>,
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
    let current = st.store.get_credential(&name).await?;
    let expect = check_if_match(
        &headers,
        &etag_of(&ProviderCredential::from(current.clone())),
        current.metadata.updated_at,
    )?;
    let provider = spec.provider.unwrap_or(current.spec.provider);
    // Resultierende Feldmenge ohne Entschlüsseln prüfen.
    let mut fields: BTreeSet<&str> = current.spec.fields.iter().map(String::as_str).collect();
    for (f, v) in &spec.data {
        if v.is_some() {
            fields.insert(f);
        } else {
            fields.remove(f.as_str());
        }
    }
    let entra = match &spec.entra {
        Some(e) => e.as_ref(),
        None => current.spec.entra.as_ref(),
    };
    check_credential(&st, provider, entra, fields)?;
    check_users(&st, &name, provider, entra).await?;
    let changes = spec
        .data
        .into_iter()
        .map(|(k, v)| (k, v.map(|v| v.value)))
        .collect();
    let c = st
        .store
        .patch_credential(
            p.actor(),
            keyring,
            &name,
            meta.labels,
            spec.provider,
            spec.entra,
            changes,
            expect,
        )
        .await?;
    Ok(respond(StatusCode::OK, c))
}

/// Löscht Zugangsdaten (`409`, solange Deployments oder MCP-Server sie nutzen).
#[utoipa::path(
    delete, path = "/admin/api/v1/provider-credentials/{name}", tag = "provider-credentials",
    params(
        ("name" = String, Path, description = "Name"),
        ("If-Match" = String, Header, description = "ETag aus dem letzten Lesen oder `*` (Pflicht, F38; fehlt er: 428)"),
    ),
    responses(
        (status = 204, description = "Gelöscht"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Wird noch verwendet", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "ETag passt nicht", body = Problem, content_type = "application/problem+json"),
        (status = 428, description = "If-Match fehlt (F38)", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn delete_credential(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> ApiResult {
    p.require_on(Permission::Write, &Target::Catalog)?;
    let _guard = st.write_lock.lock().await;
    let current = st.store.get_credential(&name).await?;
    let updated_at = current.metadata.updated_at;
    let expect = check_if_match(
        &headers,
        &etag_of(&ProviderCredential::from(current)),
        updated_at,
    )?;
    st.store
        .delete_expect(
            p.actor(),
            core::ResourceKind::ProviderCredential,
            &name,
            expect,
        )
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Routen der Zugangsdaten (ohne Authentifizierungsschicht).
pub(crate) fn router() -> axum::Router<AdminState> {
    use axum::routing::get;
    axum::Router::new()
        .route(
            "/admin/api/v1/provider-credentials",
            get(list_credentials).post(create_credential),
        )
        .route(
            "/admin/api/v1/provider-credentials/{name}",
            get(get_credential)
                .put(put_credential)
                .patch(patch_credential)
                .delete(delete_credential),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_fields_per_provider() {
        assert!(check_fields(ProviderKind::OpenAi, ["apiKey"]).is_ok());
        assert!(check_fields(ProviderKind::OpenAi, ["token"]).is_err());
        assert!(check_fields(ProviderKind::OpenAi, []).is_err());
        assert!(
            check_fields(
                ProviderKind::AzureOpenAi,
                ["tenantId", "clientId", "clientSecret"]
            )
            .is_ok()
        );
        assert!(check_fields(ProviderKind::AzureOpenAi, ["tenantId", "clientId"]).is_err());
        assert!(
            check_fields(
                ProviderKind::Bedrock,
                ["accessKeyId", "secretAccessKey", "sessionToken"]
            )
            .is_ok()
        );
        assert!(check_fields(ProviderKind::Bedrock, ["accessKeyId"]).is_err());
        assert!(check_fields(ProviderKind::Ollama, ["anything"]).is_ok());
        assert!(check_fields(ProviderKind::Ollama, ["1bad"]).is_err());
        assert!(check_fields(ProviderKind::Ollama, ["bad-name"]).is_err());
    }

    #[test]
    fn json_errors_do_not_echo_secrets() {
        let mut p = Problem::invalid(
            "spec.data.apiKey: invalid type: string \"sk-geheim\", expected struct CredentialValue",
        );
        redact_detail(&mut p);
        assert!(!p.detail.unwrap_or_default().contains("sk-geheim"));
    }
}
