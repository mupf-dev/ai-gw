// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! WASM-Module für Plugins der Art `wasm` (AIGW-63): `/admin/api/v1/wasm-modules`.
//!
//! - `GET /wasm-modules` – Liste (Metadaten, Verwendung in Bindungen); jede Rolle mit
//!   Leserecht auf den Katalog.
//! - `GET /wasm-modules/{name}` – Metadaten (`ETag` = SHA-256);
//!   `GET /wasm-modules/{name}/content` – die Komponente selbst (`application/wasm`).
//! - `PUT /wasm-modules/{name}?sha256=…&description=…` – Hochladen bzw. Ersetzen (Body:
//!   Komponente, `Content-Type: application/wasm`). Der SHA-256 ist **Pflicht** und muss zum
//!   Inhalt passen (Integrität des Uploads); Ersetzen braucht `If-Match`. **Nur ein globaler
//!   `admin`** (Freigabe: ein Modul läuft im Gateway-Prozess). Größe: `wasm.max_module_bytes`.
//! - `DELETE /wasm-modules/{name}` – nur, wenn keine Bindung mehr darauf verweist (`409`).
//!
//! Signaturen (AIGW-101): `PUT …?keyId=…&signature=…` (Ed25519 über
//! `aigw-wasm-module-v1:<sha256>`, `aigw wasm sign`). Mit `wasm.trusted_keys` prüft der Upload
//! gegen die Richtlinie (`422` bei fehlender Pflicht-Signatur, unbekanntem Schlüssel oder
//! ungültiger Signatur); Liste und Einzelabfrage zeigen den Prüfstatus (`signature`).
//!
//! Sigstore (AIGW-106): Upload als JSON-Umschlag (`Content-Type: application/json`,
//! `{"content": "<Base64>", "sigstoreBundle": {…}}`) mit einem Bundle aus `cosign sign-blob
//! --bundle`; geprüft gegen `wasm.sigstore` (Trust-Root offline, erlaubte Identitäten).
//!
//! Vier-Augen-Freigabe (AIGW-106, `wasm.four_eyes`; Standard im Profil `strict`): ein Upload
//! wird nur **eingereicht** (`202`, Status „wartet auf Freigabe“) und erst mit
//! `POST /wasm-modules/{name}/approve?sha256=…` durch einen **anderen** globalen `admin` aktiv
//! (Admin-Keys zählen als die Person, die sie angelegt hat). `DELETE …/{name}/pending` verwirft,
//! `GET …/{name}/pending/content` lädt die Einreichung zur Prüfung herunter.
//!
//! Inhalte erscheinen nie im Audit-Log (nur Name, SHA-256, Größe). Modus „nur YAML“: Schreiben
//! gesperrt – Module kommen dann aus `wasm.modules` der Startkonfiguration.

// `#[utoipa::path(params(…))]` erzeugt für IntoParams-Typen qualifizierte Pfade.
#![allow(unused_qualifications)]

use aigw_core::wasm::ModuleSignature;
use aigw_store::wasm::{
    NewWasmModule, PendingWasmModuleInfo, PutOutcome, SubmitOutcome, WasmModuleInfo,
};
use axum::body::Bytes;
use axum::extract::rejection::QueryRejection;
use axum::extract::{DefaultBodyLimit, Extension, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::IntoResponse as _;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::AdminState;
use crate::auth::{Permission, Principal, Target};
use crate::problem::Problem;
use crate::resources::{ApiResult, BASE, St, check_if_match, created_at, json_with};

/// Pfad der Sammlung.
pub const PATH: &str = "/admin/api/v1/wasm-modules";

/// Ein WASM-Modul (Metadaten).
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WasmModule {
    /// ID (UUIDv7).
    #[schema(value_type = String, format = Uuid)]
    pub id: aigw_core::resource::Id,
    /// Name.
    pub name: String,
    /// SHA-256 des Inhalts (hex).
    pub sha256: String,
    /// Größe in Byte.
    pub size: u64,
    /// Beschreibung.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Hochgeladen von.
    pub created_by: String,
    /// Angelegt (RFC 3339).
    pub created_at: String,
    /// Zuletzt ersetzt (RFC 3339).
    pub updated_at: String,
    /// Plugin-Bindungen, die das Modul verwenden.
    pub used_by: Vec<String>,
    /// Signatur und Prüfergebnis gegen die aktuelle Richtlinie (AIGW-101, AIGW-106).
    pub signature: SignatureInfo,
    /// Hochgeladen bzw. freigegeben (Vier-Augen-Freigabe, AIGW-106).
    pub approval: ApprovalInfo,
    /// Eingereichte neue Version, die auf Freigabe wartet (nur Einzelabfrage).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<PendingWasmModule>,
}

/// Herkunft der aktiven Version (AIGW-106).
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalInfo {
    /// Hochgeladen von.
    pub uploaded_by: String,
    /// Freigegeben von (fehlt = ohne Vier-Augen-Freigabe aktiv geworden).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_by: Option<String>,
    /// Freigegeben am (RFC 3339).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_at: Option<String>,
}

/// Eingereichte Version, die auf Freigabe wartet (AIGW-106).
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PendingWasmModule {
    /// Name des Moduls.
    pub name: String,
    /// SHA-256 der eingereichten Version (für `approve?sha256=…`).
    pub sha256: String,
    /// Größe in Byte.
    pub size: u64,
    /// Beschreibung.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Hochgeladen von.
    pub uploaded_by: String,
    /// Hochgeladen am (RFC 3339).
    pub uploaded_at: String,
    /// SHA-256 der aktiven Version, die ersetzt würde (fehlt = neues Modul).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replaces: Option<String>,
    /// Signaturen gegen die aktuelle Richtlinie.
    pub signature: SignatureInfo,
    /// Darf der anfragende Principal freigeben (globaler `admin`, nicht der Hochlader)?
    pub can_approve: bool,
}

/// Prüfstatus der Signatur eines Moduls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SignatureStatus {
    /// Gültige Signatur eines vertrauenswürdigen Schlüssels.
    Valid,
    /// Signatur passt nicht zu Inhalt und Schlüssel.
    Invalid,
    /// Schlüssel-ID nicht in `wasm.trusted_keys` (z. B. nach Schlüsselwechsel entfernt).
    UnknownKey,
    /// Unsigniert.
    Unsigned,
    /// Nicht geprüft (keine `wasm.trusted_keys` konfiguriert).
    Unchecked,
    /// Sigstore-Bundle gültig, aber von keiner erlaubten Identität (AIGW-106).
    Identity,
}

/// Prüfstatus eines Sigstore-Bundles (AIGW-106).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SigstoreStatus {
    /// Gültig, erlaubte Identität.
    Valid,
    /// Kette, Log-Eintrag oder Signatur ungültig (oder `wasm.sigstore` nicht konfiguriert).
    Invalid,
    /// Gültig, aber keine erlaubte Identität.
    Identity,
}

/// Sigstore-Bundle eines Moduls (AIGW-106).
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SigstoreInfo {
    /// Prüfergebnis.
    pub status: SigstoreStatus,
    /// Subjekt des Zertifikats (z. B. Workflow-URI, E-Mail).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// OIDC-Issuer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    /// Zeitpunkt des Rekor-Eintrags (RFC 3339).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integrated_time: Option<String>,
    /// Index im Rekor-Log.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_index: Option<i64>,
    /// Fehler (bei `invalid`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Signatur eines Moduls (AIGW-101).
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SignatureInfo {
    /// Schlüssel-ID der Signatur (fehlt = unsigniert).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
    /// Prüfergebnis.
    pub status: SignatureStatus,
    /// Darf das Modul unter der aktuellen Richtlinie geladen werden? (`false`: Bindungen
    /// darauf sind ungültig → `failMode`.)
    pub loadable: bool,
    /// Sigstore-Bundle (AIGW-106; fehlt = ohne).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sigstore: Option<SigstoreInfo>,
}

/// Liste der Module.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WasmModuleList {
    /// Module (nach Name sortiert).
    pub items: Vec<WasmModule>,
    /// Signatur Pflicht (`wasm.require_signature`, Standard: sobald Schlüssel konfiguriert).
    pub signature_required: bool,
    /// IDs der vertrauenswürdigen Schlüssel (`wasm.trusted_keys`).
    pub trusted_keys: Vec<String>,
    /// Erlaubte Sigstore-Identitäten (`wasm.sigstore.identities`, AIGW-106).
    pub sigstore_identities: Vec<SigstoreIdentityView>,
    /// Vier-Augen-Freigabe aktiv (`wasm.four_eyes`, AIGW-106).
    pub four_eyes: bool,
    /// Eingereichte Versionen, die auf Freigabe warten (nach Name sortiert).
    pub pending: Vec<PendingWasmModule>,
}

/// Erlaubte Sigstore-Identität (reguläre Ausdrücke wie konfiguriert).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SigstoreIdentityView {
    /// OIDC-Issuer.
    pub issuer: String,
    /// Subjekt (SAN).
    pub subject: String,
}

/// JSON-Umschlag für den Upload mit Sigstore-Bundle (`Content-Type: application/json`).
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WasmUpload {
    /// Komponente (Base64, Standard-Alphabet).
    pub content: String,
    /// Sigstore-Bundle (`cosign sign-blob --bundle`, JSON-Objekt).
    #[schema(value_type = Option<Object>)]
    pub sigstore_bundle: Option<serde_json::Value>,
}

/// Parameter von `POST /wasm-modules/{name}/approve`.
#[derive(Debug, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct ApproveQuery {
    /// SHA-256 der geprüften Einreichung (Pflicht: freigegeben wird genau diese Version).
    pub sha256: String,
}

/// Binärer Inhalt (WebAssembly-Komponente).
#[derive(Debug, ToSchema)]
#[schema(value_type = String, format = Binary)]
pub struct WasmBinary(pub Vec<u8>);

/// Parameter von `PUT /wasm-modules/{name}`.
#[derive(Debug, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct UploadQuery {
    /// SHA-256 des Inhalts (hex, Pflicht; Integritätsprüfung).
    pub sha256: String,
    /// Beschreibung (höchstens 1024 Zeichen).
    pub description: Option<String>,
    /// Schlüssel-ID der Signatur (aus `wasm.trusted_keys`; nur zusammen mit `signature`).
    #[serde(rename = "keyId")]
    pub key_id: Option<String>,
    /// Ed25519-Signatur (Base64, Standard- oder URL-Alphabet) über
    /// `aigw-wasm-module-v1:<sha256>` (`aigw wasm sign`).
    pub signature: Option<String>,
}

fn rfc3339(t: time::OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn etag(sha: &str) -> String {
    format!("\"{sha}\"")
}

fn signature_info(
    trust: &aigw_core::wasm::TrustPolicy,
    sha256: &str,
    sig: Option<&ModuleSignature>,
    bundle: Option<&str>,
) -> SignatureInfo {
    use aigw_core::wasm::{SignatureError, Verified};
    let res = trust.verify_all(sha256, sig, bundle);
    let status = match &res {
        Ok(Verified::Valid { .. } | Verified::Sigstore(_)) => SignatureStatus::Valid,
        Ok(Verified::Unchecked) => SignatureStatus::Unchecked,
        Ok(Verified::Unsigned) | Err(SignatureError::Missing) => SignatureStatus::Unsigned,
        Err(SignatureError::UnknownKey(_)) => SignatureStatus::UnknownKey,
        Err(SignatureError::Identity { .. }) => SignatureStatus::Identity,
        Err(
            SignatureError::Invalid(_) | SignatureError::Digest(_) | SignatureError::Sigstore(_),
        ) => SignatureStatus::Invalid,
    };
    let sigstore = bundle.map(|b| match trust.verify_sigstore(sha256, b) {
        Ok(s) => SigstoreInfo {
            status: SigstoreStatus::Valid,
            subject: Some(s.subject),
            issuer: Some(s.issuer),
            integrated_time: time::OffsetDateTime::from_unix_timestamp(s.integrated_time)
                .ok()
                .map(rfc3339),
            log_index: Some(s.log_index),
            message: None,
        },
        Err(SignatureError::Identity { subject, issuer }) => SigstoreInfo {
            status: SigstoreStatus::Identity,
            subject: Some(subject),
            issuer: Some(issuer),
            integrated_time: None,
            log_index: None,
            message: None,
        },
        Err(e) => SigstoreInfo {
            status: SigstoreStatus::Invalid,
            subject: None,
            issuer: None,
            integrated_time: None,
            log_index: None,
            message: Some(e.to_string()),
        },
    });
    SignatureInfo {
        key_id: sig.map(|s| s.key_id.clone()),
        status,
        loadable: res.is_ok(),
        sigstore,
    }
}

fn view(
    trust: &aigw_core::wasm::TrustPolicy,
    i: WasmModuleInfo,
    used_by: Vec<String>,
) -> WasmModule {
    let signature = signature_info(
        trust,
        &i.sha256,
        i.signature.as_ref(),
        i.sigstore_bundle.as_deref(),
    );
    let approval = ApprovalInfo {
        uploaded_by: i.uploaded_by.unwrap_or_else(|| i.created_by.clone()),
        approved_by: i.approved_by,
        approved_at: i.approved_at.map(rfc3339),
    };
    WasmModule {
        id: i.id,
        name: i.name,
        sha256: i.sha256,
        size: i.size,
        description: i.description,
        created_by: i.created_by,
        created_at: rfc3339(i.created_at),
        updated_at: rfc3339(i.updated_at),
        used_by,
        signature,
        approval,
        pending: None,
    }
}

fn pending_view(
    trust: &aigw_core::wasm::TrustPolicy,
    p: PendingWasmModuleInfo,
    replaces: Option<String>,
    can_approve: bool,
) -> PendingWasmModule {
    let signature = signature_info(
        trust,
        &p.sha256,
        p.signature.as_ref(),
        p.sigstore_bundle.as_deref(),
    );
    PendingWasmModule {
        name: p.name,
        sha256: p.sha256,
        size: p.size,
        description: p.description,
        uploaded_by: p.uploaded_by,
        uploaded_at: rfc3339(p.uploaded_at),
        replaces,
        signature,
        can_approve,
    }
}

/// Die Person hinter einem Principal (Vier-Augen-Prinzip): Admin-Keys zählen als die Person,
/// die sie angelegt hat (rekursiv bis zu einem Nutzer). Keys, die der Bootstrap-Admin bzw. die
/// Startkonfiguration angelegt hat, gelten als eigene Person (sie werden in der Einrichtung an
/// verschiedene Halter ausgegeben; der Bootstrap-Zugang endet mit dem ersten Admin-Key).
async fn person_of(st: &AdminState, p: &Principal) -> Result<String, Problem> {
    let mut subject = p.subject.clone();
    for _ in 0..8 {
        let Some(name) = subject.strip_prefix("admin-key:") else {
            return Ok(subject);
        };
        match st.store.get_admin_api_key(name).await? {
            Some(k) if k.created_by.starts_with("user:") => return Ok(k.created_by),
            Some(k) if k.created_by.starts_with("admin-key:") => subject = k.created_by,
            _ => return Ok(subject),
        }
    }
    Ok(subject)
}

/// Darf `p` die Einreichung freigeben? (globaler `admin`, weder Hochlader noch dieselbe
/// Person.) `Err` mit Begründung.
fn may_approve(
    p: &Principal,
    person: &str,
    pending: &PendingWasmModuleInfo,
) -> Result<(), Problem> {
    if !crate::rbac::is_global_admin(&p.roles) {
        return Err(Problem::forbidden(
            "Freigabe nur durch einen globalen admin",
        ));
    }
    if p.subject == pending.uploaded_by || person == pending.uploader_person {
        return Err(Problem::forbidden(format!(
            "Vier-Augen-Prinzip: {} hat die Version hochgeladen – die Freigabe muss eine andere \
             Person erteilen",
            pending.uploaded_by
        )));
    }
    Ok(())
}

async fn users_by_module(
    st: &AdminState,
) -> Result<std::collections::BTreeMap<String, Vec<String>>, Problem> {
    let mut m: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for b in st
        .store
        .list::<aigw_core::resource::PluginBindingSpec>()
        .await?
    {
        if let Some(module) = aigw_store::wasm::binding_module(&b.spec) {
            m.entry(module.to_owned())
                .or_default()
                .push(b.metadata.name);
        }
    }
    Ok(m)
}

fn not_found(name: &str) -> Problem {
    Problem::not_found(format!("WASM-Modul {name:?} nicht gefunden"))
}

fn match_pending_hint(name: &str) -> Problem {
    Problem::not_found(format!(
        "WASM-Modul {name:?} nicht aktiv (eingereichte Versionen: GET {PATH}, Feld pending)"
    ))
}

/// Module auflisten.
#[utoipa::path(
    get, path = "/admin/api/v1/wasm-modules", tag = "wasm-modules",
    responses(
        (status = 200, description = "Module", body = WasmModuleList),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub(crate) async fn list_wasm_modules(
    State(st): St,
    Extension(p): Extension<Principal>,
) -> ApiResult {
    p.require_on(Permission::Read, &Target::Catalog)?;
    let mut users = users_by_module(&st).await?;
    let items = st
        .store
        .list_wasm_modules()
        .await?
        .into_iter()
        .map(|i| {
            let u = users.remove(&i.name).unwrap_or_default();
            view(&st.wasm_trust, i, u)
        })
        .collect();
    let items: Vec<WasmModule> = items;
    let person = person_of(&st, &p).await?;
    let pending = st
        .store
        .list_pending_wasm_modules()
        .await?
        .into_iter()
        .map(|i| {
            let replaces = items
                .iter()
                .find(|m| m.name == i.name)
                .map(|m| m.sha256.clone());
            let ok = may_approve(&p, &person, &i).is_ok();
            pending_view(&st.wasm_trust, i, replaces, ok)
        })
        .collect();
    let list = WasmModuleList {
        items,
        signature_required: st.wasm_trust.required(),
        trusted_keys: st.wasm_trust.key_ids().map(str::to_owned).collect(),
        sigstore_identities: st
            .wasm_trust
            .sigstore()
            .map(|s| {
                s.identities()
                    .iter()
                    .map(|i| SigstoreIdentityView {
                        issuer: i.issuer.clone(),
                        subject: i.subject.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        four_eyes: st.wasm_four_eyes,
        pending,
    };
    Ok(json_with(StatusCode::OK, None, &list))
}

/// Metadaten eines Moduls.
#[utoipa::path(
    get, path = "/admin/api/v1/wasm-modules/{name}", tag = "wasm-modules",
    params(("name" = String, Path, description = "Name des Moduls")),
    responses(
        (status = 200, description = "Modul", body = WasmModule),
        (status = 404, description = "Unbekannt", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub(crate) async fn get_wasm_module(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
) -> ApiResult {
    p.require_on(Permission::Read, &Target::Catalog)?;
    let info = st
        .store
        .wasm_module_info(&name)
        .await?
        .ok_or_else(|| match_pending_hint(&name))?;
    let users = st.store.wasm_module_users(&name).await?;
    let tag = etag(&info.sha256);
    let mut v = view(&st.wasm_trust, info, users);
    if let Some(pi) = st.store.pending_wasm_module_info(&name).await? {
        let person = person_of(&st, &p).await?;
        let ok = may_approve(&p, &person, &pi).is_ok();
        v.pending = Some(pending_view(&st.wasm_trust, pi, Some(v.sha256.clone()), ok));
    }
    Ok(json_with(StatusCode::OK, Some(tag), &v))
}

/// Inhalt eines Moduls (Komponente).
#[utoipa::path(
    get, path = "/admin/api/v1/wasm-modules/{name}/content", tag = "wasm-modules",
    params(("name" = String, Path, description = "Name des Moduls")),
    responses(
        (status = 200, description = "Komponente", body = WasmBinary, content_type = "application/wasm"),
        (status = 404, description = "Unbekannt", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub(crate) async fn get_wasm_module_content(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
) -> ApiResult {
    p.require_on(Permission::Read, &Target::Catalog)?;
    let m = st
        .store
        .wasm_module(&name)
        .await?
        .ok_or_else(|| not_found(&name))?;
    Ok(wasm_response(m.content, &m.sha256, &format!("{name}.wasm")))
}

/// Modul hochladen oder ersetzen (nur globaler `admin`). Mit Vier-Augen-Freigabe
/// (`wasm.four_eyes`) wird die Version nur eingereicht (`202`).
#[utoipa::path(
    put, path = "/admin/api/v1/wasm-modules/{name}", tag = "wasm-modules",
    params(("name" = String, Path, description = "Name des Moduls"), UploadQuery),
    request_body(content(
        (WasmBinary = "application/wasm"),
        (WasmUpload = "application/json"),
    ), description = "WebAssembly-Komponente (WIT-Welt aigw:plugin/plugin) bzw. JSON-Umschlag mit Sigstore-Bundle"),
    responses(
        (status = 201, description = "Angelegt", body = WasmModule),
        (status = 200, description = "Ersetzt bzw. unverändert", body = WasmModule),
        (status = 202, description = "Eingereicht, wartet auf Freigabe (wasm.four_eyes)", body = PendingWasmModule),
        (status = 403, description = "Nur globaler admin", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "If-Match passt nicht", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "Zu groß (wasm.max_module_bytes)", body = Problem, content_type = "application/problem+json"),
        (status = 415, description = "Content-Type nicht application/wasm bzw. application/json", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Kein gültiges Modul, SHA-256 passt nicht oder Signatur fehlt/ungültig (wasm.trusted_keys, wasm.sigstore)", body = Problem, content_type = "application/problem+json"),
        (status = 428, description = "Ersetzen ohne If-Match", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub(crate) async fn put_wasm_module(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
    q: Result<Query<UploadQuery>, QueryRejection>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult {
    p.require_on(Permission::Write, &Target::Catalog)?;
    let Query(q) = q.map_err(|e| Problem::bad_request(e.body_text()))?;
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let ct = ct.split(';').next().unwrap_or("").trim();
    let (content, sigstore_bundle) = match ct {
        "application/wasm" | "application/octet-stream" => (body.to_vec(), None),
        "application/json" => {
            let u: WasmUpload = serde_json::from_slice(&body)
                .map_err(|e| Problem::bad_request(format!("JSON-Umschlag: {e}")))?;
            let content = base64::engine::general_purpose::STANDARD
                .decode(u.content.trim())
                .map_err(|_| Problem::invalid("content: kein gültiges Base64"))?;
            if content.len() as u64 > st.wasm_max_bytes {
                return Err(Problem::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "payload-too-large",
                    "Zu groß",
                    format!(
                        "Modul: höchstens {} Byte (wasm.max_module_bytes)",
                        st.wasm_max_bytes
                    ),
                ));
            }
            let bundle = match u.sigstore_bundle {
                None | Some(serde_json::Value::Null) => None,
                Some(v @ serde_json::Value::Object(_)) => Some(v.to_string()),
                Some(serde_json::Value::String(s)) => Some(s),
                Some(_) => {
                    return Err(Problem::invalid(
                        "sigstoreBundle: JSON-Objekt (Sigstore-Bundle) erwartet",
                    ));
                }
            };
            (content, bundle)
        }
        _ => {
            return Err(Problem::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported-media-type",
                "Falscher Medientyp",
                "Content-Type: application/wasm (Komponente) bzw. application/json (Umschlag mit \
                 Sigstore-Bundle) erwartet",
            ));
        }
    };
    let signature = match (q.key_id.as_deref(), q.signature.as_deref()) {
        (None, None) => None,
        (Some(k), Some(s)) => Some(ModuleSignature::parse(k, s).map_err(Problem::invalid)?),
        _ => {
            return Err(Problem::invalid(
                "keyId und signature nur gemeinsam angeben",
            ));
        }
    };
    if let Some(b) = &sigstore_bundle {
        aigw_core::wasm::check_bundle_format(b).map_err(Problem::invalid)?;
    }
    // Signaturen gegen die Richtlinie (über den SHA-256 des tatsächlichen Inhalts; passt der
    // angegebene SHA nicht, meldet das der Store).
    let actual = aigw_core::wasm::sha256_hex(&content);
    if aigw_core::wasm::normalize_sha256(&q.sha256).as_deref() == Ok(actual.as_str()) {
        st.wasm_trust
            .verify_all(&actual, signature.as_ref(), sigstore_bundle.as_deref())
            .map_err(|e| Problem::invalid(format!("signature: {e}")))?;
    }
    let new = NewWasmModule {
        name: name.clone(),
        description: q.description.filter(|d| !d.trim().is_empty()),
        content,
        sha256: Some(q.sha256),
        signature,
        sigstore_bundle,
    };
    let person = if st.wasm_four_eyes {
        Some(person_of(&st, &p).await?)
    } else {
        None
    };
    let _guard = st.write_lock.lock().await;
    let current = st.store.wasm_module_info(&name).await?;
    if let Some(cur) = &current {
        check_if_match(&headers, &etag(&cur.sha256), None)?;
    }
    if let Some(person) = person {
        let outcome = st
            .store
            .submit_wasm_module(p.actor(), &person, new, st.wasm_max_bytes)
            .await?;
        return Ok(match outcome {
            SubmitOutcome::Pending { info, .. } => {
                let ok = may_approve(&p, &person, &info).is_ok();
                let v = pending_view(&st.wasm_trust, info, current.map(|c| c.sha256), ok);
                json_with(StatusCode::ACCEPTED, None, &v)
            }
            SubmitOutcome::Unchanged(info) => {
                let users = st.store.wasm_module_users(&name).await?;
                let tag = etag(&info.sha256);
                json_with(
                    StatusCode::OK,
                    Some(tag),
                    &view(&st.wasm_trust, info, users),
                )
            }
        });
    }
    let outcome = st
        .store
        .put_wasm_module(p.actor(), new, st.wasm_max_bytes)
        .await?;
    let users = st.store.wasm_module_users(&name).await?;
    let status = match outcome {
        PutOutcome::Created(_) => StatusCode::CREATED,
        PutOutcome::Replaced(_) | PutOutcome::Unchanged(_) => StatusCode::OK,
    };
    let info = outcome.info().clone();
    let tag = etag(&info.sha256);
    let res = json_with(status, Some(tag), &view(&st.wasm_trust, info, users));
    Ok(if status == StatusCode::CREATED {
        created_at(res, &format!("{BASE}/wasm-modules/{name}"))
    } else {
        res
    })
}

/// Eingereichte Version freigeben (Vier-Augen-Prinzip, AIGW-106): nur ein globaler `admin`,
/// der die Version nicht selbst hochgeladen hat. Die Signaturen werden erneut gegen die
/// aktuelle Richtlinie geprüft.
#[utoipa::path(
    post, path = "/admin/api/v1/wasm-modules/{name}/approve", tag = "wasm-modules",
    params(("name" = String, Path, description = "Name des Moduls"), ApproveQuery),
    responses(
        (status = 200, description = "Freigegeben, jetzt aktiv", body = WasmModule),
        (status = 403, description = "Kein globaler admin oder Hochlader selbst (Vier-Augen-Prinzip)", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Keine Einreichung", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Einreichung inzwischen ersetzt (SHA-256 passt nicht)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Signatur unter der aktuellen Richtlinie ungültig", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub(crate) async fn approve_wasm_module(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
    q: Result<Query<ApproveQuery>, QueryRejection>,
) -> ApiResult {
    p.require_on(Permission::Write, &Target::Catalog)?;
    let Query(q) = q.map_err(|e| Problem::bad_request(e.body_text()))?;
    let sha = aigw_core::wasm::normalize_sha256(&q.sha256).map_err(Problem::invalid)?;
    let person = person_of(&st, &p).await?;
    let _guard = st.write_lock.lock().await;
    let pending = st
        .store
        .pending_wasm_module_info(&name)
        .await?
        .ok_or_else(|| Problem::not_found(format!("keine Einreichung für WASM-Modul {name:?}")))?;
    may_approve(&p, &person, &pending)?;
    if pending.sha256 != sha {
        return Err(Problem::conflict(format!(
            "Einreichung von {name:?} hat SHA-256 {} (inzwischen ersetzt?)",
            pending.sha256
        )));
    }
    st.wasm_trust
        .verify_all(
            &pending.sha256,
            pending.signature.as_ref(),
            pending.sigstore_bundle.as_deref(),
        )
        .map_err(|e| Problem::invalid(format!("signature: {e}")))?;
    let outcome = st
        .store
        .approve_wasm_module(p.actor(), &name, &sha)
        .await?
        .ok_or_else(|| Problem::not_found(format!("keine Einreichung für WASM-Modul {name:?}")))?;
    let info = outcome.info().clone();
    let users = st.store.wasm_module_users(&name).await?;
    let tag = etag(&info.sha256);
    Ok(json_with(
        StatusCode::OK,
        Some(tag),
        &view(&st.wasm_trust, info, users),
    ))
}

/// Eingereichte Version verwerfen (jeder globale `admin`, auch der Hochlader selbst).
#[utoipa::path(
    delete, path = "/admin/api/v1/wasm-modules/{name}/pending", tag = "wasm-modules",
    params(("name" = String, Path, description = "Name des Moduls")),
    responses(
        (status = 204, description = "Verworfen"),
        (status = 403, description = "Nur globaler admin", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Keine Einreichung", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub(crate) async fn reject_wasm_module(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
) -> ApiResult {
    p.require_on(Permission::Write, &Target::Catalog)?;
    let _guard = st.write_lock.lock().await;
    st.store
        .reject_wasm_module(p.actor(), &name)
        .await?
        .ok_or_else(|| Problem::not_found(format!("keine Einreichung für WASM-Modul {name:?}")))?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Inhalt einer Einreichung (zur Prüfung vor der Freigabe).
#[utoipa::path(
    get, path = "/admin/api/v1/wasm-modules/{name}/pending/content", tag = "wasm-modules",
    params(("name" = String, Path, description = "Name des Moduls")),
    responses(
        (status = 200, description = "Komponente", body = WasmBinary, content_type = "application/wasm"),
        (status = 404, description = "Keine Einreichung", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub(crate) async fn get_pending_content(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
) -> ApiResult {
    p.require_on(Permission::Read, &Target::Catalog)?;
    let (info, content) =
        st.store.pending_wasm_module(&name).await?.ok_or_else(|| {
            Problem::not_found(format!("keine Einreichung für WASM-Modul {name:?}"))
        })?;
    Ok(wasm_response(
        content,
        &info.sha256,
        &format!("{name}-pending.wasm"),
    ))
}

fn wasm_response(content: Bytes, sha: &str, filename: &str) -> axum::response::Response {
    let mut res = (StatusCode::OK, content).into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/wasm"),
    );
    if let Ok(v) = HeaderValue::from_str(&etag(sha)) {
        h.insert(header::ETAG, v);
    }
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    res
}

/// Modul löschen (nur globaler `admin`, nur ohne verweisende Bindungen).
#[utoipa::path(
    delete, path = "/admin/api/v1/wasm-modules/{name}", tag = "wasm-modules",
    params(("name" = String, Path, description = "Name des Moduls")),
    responses(
        (status = 204, description = "Gelöscht"),
        (status = 404, description = "Unbekannt", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Noch von Plugin-Bindungen verwendet", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "If-Match passt nicht", body = Problem, content_type = "application/problem+json"),
        (status = 428, description = "If-Match fehlt", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub(crate) async fn delete_wasm_module(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> ApiResult {
    p.require_on(Permission::Write, &Target::Catalog)?;
    let _guard = st.write_lock.lock().await;
    let cur = st
        .store
        .wasm_module_info(&name)
        .await?
        .ok_or_else(|| not_found(&name))?;
    check_if_match(&headers, &etag(&cur.sha256), None)?;
    st.store
        .delete_wasm_module(p.actor(), &name)
        .await?
        .ok_or_else(|| not_found(&name))?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

fn aigw_sigstore_max_bundle() -> usize {
    aigw_core::wasm::MAX_SIGSTORE_BUNDLE_BYTES
}

/// Prüft den Modul-Verweis einer Bindung der Art `wasm` gegen die Datenbank (Modul vorhanden,
/// SHA-256 passend). GitOps prüft das nicht (Module können später hochgeladen werden).
pub(crate) async fn check_binding(
    st: &AdminState,
    spec: &aigw_core::resource::PluginBindingSpec,
) -> Result<(), Problem> {
    let Some(module) = aigw_store::wasm::binding_module(spec) else {
        return Ok(());
    };
    let Some(info) = st.store.wasm_module_info(module).await? else {
        if st.store.pending_wasm_module_info(module).await?.is_some() {
            return Err(Problem::invalid(format!(
                "config.module: WASM-Modul {module:?} wartet auf Freigabe (Vier-Augen-Prinzip: \
                 POST {PATH}/{module}/approve)"
            )));
        }
        return Err(Problem::invalid(format!(
            "config.module: unbekanntes WASM-Modul {module:?} (erst hochladen: PUT {PATH}/{module})"
        )));
    };
    if let Some(sha) = spec
        .config
        .get("sha256")
        .and_then(serde_json::Value::as_str)
    {
        let sha = aigw_core::wasm::normalize_sha256(sha).map_err(Problem::invalid)?;
        if sha != info.sha256 {
            let waiting = st
                .store
                .pending_wasm_module_info(module)
                .await?
                .is_some_and(|p| p.sha256 == sha);
            return Err(Problem::invalid(if waiting {
                format!(
                    "config.sha256: diese Version von {module:?} wartet noch auf Freigabe \
                     (aktiv: {})",
                    info.sha256
                )
            } else {
                format!(
                    "config.sha256: Modul {module:?} hat SHA-256 {}",
                    info.sha256
                )
            }));
        }
    }
    Ok(())
}

pub(crate) fn router(max_bytes: u64) -> axum::Router<AdminState> {
    use axum::routing::{delete, get, post};
    // JSON-Umschlag: Base64 (4/3) plus Sigstore-Bundle.
    let limit = usize::try_from(max_bytes)
        .unwrap_or(usize::MAX)
        .saturating_mul(4)
        / 3
        + 2 * aigw_sigstore_max_bundle()
        + 4096;
    axum::Router::new()
        .route(PATH, get(list_wasm_modules))
        .route(
            "/admin/api/v1/wasm-modules/{name}",
            get(get_wasm_module)
                .put(put_wasm_module)
                .delete(delete_wasm_module)
                .layer(DefaultBodyLimit::max(limit)),
        )
        .route(
            "/admin/api/v1/wasm-modules/{name}/content",
            get(get_wasm_module_content),
        )
        .route(
            "/admin/api/v1/wasm-modules/{name}/approve",
            post(approve_wasm_module),
        )
        .route(
            "/admin/api/v1/wasm-modules/{name}/pending",
            delete(reject_wasm_module),
        )
        .route(
            "/admin/api/v1/wasm-modules/{name}/pending/content",
            get(get_pending_content),
        )
}
