// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

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
//! Inhalte erscheinen nie im Audit-Log (nur Name, SHA-256, Größe). Modus „nur YAML“: Schreiben
//! gesperrt – Module kommen dann aus `wasm.modules` der Startkonfiguration.

use aigw_store::wasm::{NewWasmModule, PutOutcome, WasmModuleInfo};
use axum::body::Bytes;
use axum::extract::rejection::QueryRejection;
use axum::extract::{DefaultBodyLimit, Extension, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::IntoResponse as _;
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
}

/// Liste der Module.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct WasmModuleList {
    /// Module (nach Name sortiert).
    pub items: Vec<WasmModule>,
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
}

fn rfc3339(t: time::OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn etag(sha: &str) -> String {
    format!("\"{sha}\"")
}

fn view(i: WasmModuleInfo, used_by: Vec<String>) -> WasmModule {
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
    }
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
            view(i, u)
        })
        .collect();
    Ok(json_with(StatusCode::OK, None, &WasmModuleList { items }))
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
        .ok_or_else(|| not_found(&name))?;
    let users = st.store.wasm_module_users(&name).await?;
    let tag = etag(&info.sha256);
    Ok(json_with(StatusCode::OK, Some(tag), &view(info, users)))
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
    let mut res = (StatusCode::OK, m.content).into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/wasm"),
    );
    if let Ok(v) = HeaderValue::from_str(&etag(&m.sha256)) {
        h.insert(header::ETAG, v);
    }
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{name}.wasm\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(res)
}

/// Modul hochladen oder ersetzen (nur globaler `admin`).
#[utoipa::path(
    put, path = "/admin/api/v1/wasm-modules/{name}", tag = "wasm-modules",
    params(("name" = String, Path, description = "Name des Moduls"), UploadQuery),
    request_body(content = WasmBinary, content_type = "application/wasm",
        description = "WebAssembly-Komponente (WIT-Welt aigw:plugin/plugin)"),
    responses(
        (status = 201, description = "Angelegt", body = WasmModule),
        (status = 200, description = "Ersetzt bzw. unverändert", body = WasmModule),
        (status = 403, description = "Nur globaler admin", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "If-Match passt nicht", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "Zu groß (wasm.max_module_bytes)", body = Problem, content_type = "application/problem+json"),
        (status = 415, description = "Content-Type nicht application/wasm", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Kein gültiges Modul oder SHA-256 passt nicht", body = Problem, content_type = "application/problem+json"),
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
    if !matches!(ct, "application/wasm" | "application/octet-stream") {
        return Err(Problem::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported-media-type",
            "Falscher Medientyp",
            "Content-Type: application/wasm erwartet",
        ));
    }
    let _guard = st.write_lock.lock().await;
    if let Some(cur) = st.store.wasm_module_info(&name).await? {
        check_if_match(&headers, &etag(&cur.sha256), None)?;
    }
    let outcome = st
        .store
        .put_wasm_module(
            p.actor(),
            NewWasmModule {
                name: name.clone(),
                description: q.description.filter(|d| !d.trim().is_empty()),
                content: body.to_vec(),
                sha256: Some(q.sha256),
            },
            st.wasm_max_bytes,
        )
        .await?;
    let users = st.store.wasm_module_users(&name).await?;
    let status = match outcome {
        PutOutcome::Created(_) => StatusCode::CREATED,
        PutOutcome::Replaced(_) | PutOutcome::Unchanged(_) => StatusCode::OK,
    };
    let info = outcome.info().clone();
    let tag = etag(&info.sha256);
    let res = json_with(status, Some(tag), &view(info, users));
    Ok(if status == StatusCode::CREATED {
        created_at(res, &format!("{BASE}/wasm-modules/{name}"))
    } else {
        res
    })
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
            return Err(Problem::invalid(format!(
                "config.sha256: Modul {module:?} hat SHA-256 {}",
                info.sha256
            )));
        }
    }
    Ok(())
}

pub(crate) fn router(max_bytes: u64) -> axum::Router<AdminState> {
    use axum::routing::get;
    let limit = usize::try_from(max_bytes).unwrap_or(usize::MAX);
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
}
