// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Admin-API-Keys für Automatisierung (AIGW-32): `sk-aigw-admin-<Präfix>-<Geheimnis>` mit
//! Rollen und Ablauf. Gespeichert wird nur SHA-256; der Klartext steht nur in der Antwort auf
//! das Anlegen. Verwalten dürfen nur globale `admin`s (Lesen: globale Leserollen).
//!
//! `admin.auth.max_admin_key_lifetime` (F87) begrenzt `expiresAt`: fehlt die Angabe, setzt der
//! Server jetzt + Höchstlaufzeit; spätere Zeitpunkte → `422`. Einzelobjekte tragen einen
//! `ETag`, `PUT`/`DELETE` brauchen `If-Match` (F38).

use aigw_core::resource::{OrganizationSpec, RoleBinding, TeamSpec, validate_name};
use aigw_store::admin_auth::{AdminApiKeyRecord, AdminApiKeySpec};
use aigw_store::keys::ADMIN_KEY_PREFIX;
use aigw_store::{Store, StoreError, hash_api_key};
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};
use utoipa::ToSchema;

use crate::AdminState;
use crate::auth::{AdminAuthenticator, AuthMethod, BoxFuture, Permission, Principal, bearer_token};
use crate::problem::Problem;
use crate::rbac;
use crate::resources::{check_if_match, etag_of, json_with};

type ApiResult = Result<Response, Problem>;

/// Nutzung wird höchstens so oft gespeichert.
const TOUCH_INTERVAL_SECS: i64 = 60;

/// Admin-API-Key (gelesen; nie Klartext oder Hash).
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AdminApiKey {
    /// ID (UUIDv7).
    #[schema(value_type = String, format = Uuid)]
    pub id: aigw_core::resource::Id,
    /// Name.
    pub name: String,
    /// Sichtbares Präfix (12 Zeichen nach `sk-aigw-admin-`).
    pub prefix: String,
    /// Beschreibung.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Rollen.
    pub roles: Vec<RoleBinding>,
    /// Ablauf.
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub expires_at: Option<OffsetDateTime>,
    /// Angelegt von.
    pub created_by: String,
    /// Angelegt.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
    /// Zuletzt geändert.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub updated_at: OffsetDateTime,
    /// Zuletzt benutzt (auf eine Minute genau).
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub last_used_at: Option<OffsetDateTime>,
}

impl From<AdminApiKeyRecord> for AdminApiKey {
    fn from(r: AdminApiKeyRecord) -> Self {
        Self {
            id: r.id,
            name: r.name,
            prefix: r.prefix,
            description: r.spec.description,
            roles: r.spec.roles,
            expires_at: r.spec.expires_at,
            created_by: r.created_by,
            created_at: r.created_at,
            updated_at: r.updated_at,
            last_used_at: r.last_used_at,
        }
    }
}

/// Eingabe für das Anlegen.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdminApiKeyInput {
    /// Name (a-z, 0-9, `-._`).
    pub name: String,
    /// Beschreibung (Zweck, Eigentümer).
    #[serde(default)]
    pub description: Option<String>,
    /// Rollen (mindestens eine).
    pub roles: Vec<RoleBinding>,
    /// Ablauf (RFC 3339, in der Zukunft); ohne Angabe unbefristet.
    #[serde(default, with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub expires_at: Option<OffsetDateTime>,
}

/// Eingabe für das Ändern (ersetzt Beschreibung, Rollen und Ablauf).
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdminApiKeyUpdate {
    /// Beschreibung.
    #[serde(default)]
    pub description: Option<String>,
    /// Rollen (mindestens eine).
    pub roles: Vec<RoleBinding>,
    /// Ablauf (RFC 3339); ohne Angabe unbefristet.
    #[serde(default, with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub expires_at: Option<OffsetDateTime>,
}

/// Angelegter Admin-API-Key mit Klartext (nur in dieser Antwort).
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreatedAdminApiKey {
    /// Key.
    pub key: AdminApiKey,
    /// Klartext `sk-aigw-admin-…` (nur einmal sichtbar).
    pub secret: String,
}

/// Liste der Admin-API-Keys.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AdminApiKeyList {
    /// Einträge (nach Name sortiert).
    pub items: Vec<AdminApiKey>,
}

/// Prüft Rollen: mindestens eine, alle wirksam, Verweise vorhanden.
async fn check_roles(store: &Store, roles: &[RoleBinding]) -> Result<(), Problem> {
    if roles.is_empty() {
        return Err(Problem::invalid("roles: mindestens eine Rolle angeben"));
    }
    for b in roles {
        if !rbac::is_effective(b) {
            return Err(Problem::invalid(format!(
                "roles: {:?} braucht organization (org-admin) bzw. team (team-admin); \
                 gitops gilt nur global (ohne organization/team)",
                b.role
            )));
        }
        if let Some(o) = &b.organization {
            exists::<OrganizationSpec>(store, o, "organization").await?;
        }
        if let Some(t) = &b.team {
            let team = exists::<TeamSpec>(store, t, "team").await?;
            if let Some(o) = &b.organization
                && team.spec.organization != *o
            {
                return Err(Problem::invalid(format!(
                    "roles: Team {t:?} gehört nicht zur Organisation {o:?}"
                )));
            }
        }
    }
    Ok(())
}

async fn exists<R: aigw_store::Resource>(
    store: &Store,
    name: &str,
    what: &str,
) -> Result<aigw_core::resource::Object<R>, Problem> {
    match store.get::<R>(name).await {
        Ok(o) => Ok(o),
        Err(StoreError::NotFound { .. }) => Err(Problem::invalid(format!(
            "roles: unbekannte {what} {name:?}"
        ))),
        Err(e) => Err(e.into()),
    }
}

/// Prüft `expiresAt` (Zukunft; höchstens `max_lifetime` Sekunden ab jetzt, F87) und setzt
/// ohne Angabe bei konfigurierter Höchstlaufzeit jetzt + Höchstlaufzeit.
pub(crate) fn effective_expiry(
    e: Option<OffsetDateTime>,
    max_lifetime: Option<u64>,
) -> Result<Option<OffsetDateTime>, Problem> {
    let now = OffsetDateTime::now_utc();
    if e.is_some_and(|t| t <= now) {
        return Err(Problem::invalid("expiresAt liegt in der Vergangenheit"));
    }
    let Some(max) = max_lifetime else {
        return Ok(e);
    };
    let limit = now + Duration::seconds(i64::try_from(max).unwrap_or(i64::MAX / 1000));
    match e {
        None => Ok(Some(limit)),
        Some(t) if t <= limit => Ok(Some(t)),
        Some(_) => Err(Problem::invalid(format!(
            "expiresAt: höchstens {max} s in der Zukunft (admin.auth.max_admin_key_lifetime)"
        ))),
    }
}

/// ETag eines Admin-API-Keys (ohne `lastUsedAt`, das sich bei Nutzung ändert).
fn admin_key_etag(k: &AdminApiKeyRecord) -> String {
    etag_of(&(
        &k.id,
        &k.name,
        &k.prefix,
        &k.spec,
        &k.created_by,
        k.updated_at,
    ))
}

async fn latch_bootstrap(store: &Store, name: &str, roles: &[RoleBinding]) {
    if rbac::is_global_admin(roles) {
        match store
            .disable_bootstrap(&format!("admin-api-key:{name}"))
            .await
        {
            Ok(true) => tracing::info!(
                key = name,
                "Admin-API-Key mit Rolle admin angelegt – Bootstrap-Token (auto) dauerhaft abgeschaltet"
            ),
            Ok(false) => {}
            Err(e) => tracing::warn!(error = %e, "Bootstrap-Abschaltung nicht gespeichert"),
        }
    }
}

async fn get_or_404(store: &Store, name: &str) -> Result<AdminApiKeyRecord, Problem> {
    store
        .get_admin_api_key(name)
        .await?
        .ok_or_else(|| Problem::not_found(format!("Admin-API-Key {name:?} nicht gefunden")))
}

/// Listet Admin-API-Keys.
#[utoipa::path(
    get, path = "/admin/api/v1/admin-api-keys", tag = "admin-api-keys",
    responses(
        (status = 200, description = "Alle Admin-API-Keys", body = AdminApiKeyList),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub(crate) async fn list_admin_api_keys(
    State(st): State<AdminState>,
    Extension(p): Extension<Principal>,
) -> ApiResult {
    p.require(Permission::Read)?;
    let items = st
        .store
        .list_admin_api_keys()
        .await?
        .into_iter()
        .map(AdminApiKey::from)
        .collect();
    Ok(Json(AdminApiKeyList { items }).into_response())
}

/// Legt einen Admin-API-Key an (nur globale `admin`s). Der Klartext steht **nur** hier.
#[utoipa::path(
    post, path = "/admin/api/v1/admin-api-keys", tag = "admin-api-keys", request_body = AdminApiKeyInput,
    responses(
        (status = 201, description = "Angelegt (mit Klartext)", body = CreatedAdminApiKey,
            headers(("Location" = String, description = "Pfad"))),
        (status = 400, description = "Ungültige Anfrage", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Name bereits vergeben", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Ungültige Eingabe", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub(crate) async fn create_admin_api_key(
    State(st): State<AdminState>,
    Extension(p): Extension<Principal>,
    body: Result<Json<AdminApiKeyInput>, JsonRejection>,
) -> ApiResult {
    p.require(Permission::Write)?;
    let Json(input) = body?;
    validate_name(&input.name).map_err(Problem::invalid)?;
    check_roles(&st.store, &input.roles).await?;
    let expires_at = effective_expiry(input.expires_at, st.auth_opts.max_admin_key_lifetime)?;
    let spec = AdminApiKeySpec {
        description: input.description,
        roles: input.roles,
        expires_at,
    };
    let created = st
        .store
        .create_admin_api_key(p.actor(), &input.name, spec)
        .await?;
    latch_bootstrap(&st.store, &created.key.name, &created.key.spec.roles).await;
    let location = format!("/admin/api/v1/admin-api-keys/{}", created.key.name);
    let tag = admin_key_etag(&created.key);
    let mut res = (
        StatusCode::CREATED,
        Json(CreatedAdminApiKey {
            key: created.key.into(),
            secret: created.secret,
        }),
    )
        .into_response();
    let h = res.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Ok(v) = HeaderValue::from_str(&location) {
        h.insert(header::LOCATION, v);
    }
    if let Ok(v) = HeaderValue::from_str(&tag) {
        h.insert(header::ETAG, v);
    }
    Ok(res)
}

/// Liest einen Admin-API-Key.
#[utoipa::path(
    get, path = "/admin/api/v1/admin-api-keys/{name}", tag = "admin-api-keys",
    params(("name" = String, Path, description = "Name")),
    responses(
        (status = 200, description = "Gefunden", body = AdminApiKey),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub(crate) async fn get_admin_api_key(
    State(st): State<AdminState>,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
) -> ApiResult {
    p.require(Permission::Read)?;
    let k = get_or_404(&st.store, &name).await?;
    let tag = admin_key_etag(&k);
    Ok(json_with(StatusCode::OK, Some(tag), &AdminApiKey::from(k)))
}

/// Ändert Beschreibung, Rollen und Ablauf eines Admin-API-Keys (nur globale `admin`s).
#[utoipa::path(
    put, path = "/admin/api/v1/admin-api-keys/{name}", tag = "admin-api-keys", request_body = AdminApiKeyUpdate,
    params(
        ("name" = String, Path, description = "Name"),
        ("If-Match" = String, Header, description = "ETag aus dem letzten Lesen oder `*` (Pflicht, F38; fehlt er: 428)"),
    ),
    responses(
        (status = 200, description = "Geändert", body = AdminApiKey),
        (status = 400, description = "Ungültige Anfrage", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "ETag passt nicht", body = Problem, content_type = "application/problem+json"),
        (status = 428, description = "If-Match fehlt (F38)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Ungültige Eingabe", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub(crate) async fn put_admin_api_key(
    State(st): State<AdminState>,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Result<Json<AdminApiKeyUpdate>, JsonRejection>,
) -> ApiResult {
    p.require(Permission::Write)?;
    let Json(input) = body?;
    check_roles(&st.store, &input.roles).await?;
    let expires_at = effective_expiry(input.expires_at, st.auth_opts.max_admin_key_lifetime)?;
    let current = get_or_404(&st.store, &name).await?;
    let expect = check_if_match(
        &headers,
        &admin_key_etag(&current),
        Some(current.updated_at),
    )?;
    let spec = AdminApiKeySpec {
        description: input.description,
        roles: input.roles,
        expires_at,
    };
    let k = st
        .store
        .update_admin_api_key(p.actor(), &name, spec, expect)
        .await?;
    latch_bootstrap(&st.store, &k.name, &k.spec.roles).await;
    let tag = admin_key_etag(&k);
    Ok(json_with(StatusCode::OK, Some(tag), &AdminApiKey::from(k)))
}

/// Löscht (widerruft) einen Admin-API-Key; wirkt sofort.
#[utoipa::path(
    delete, path = "/admin/api/v1/admin-api-keys/{name}", tag = "admin-api-keys",
    params(
        ("name" = String, Path, description = "Name"),
        ("If-Match" = String, Header, description = "ETag aus dem letzten Lesen oder `*` (Pflicht, F38; fehlt er: 428)"),
    ),
    responses(
        (status = 204, description = "Gelöscht"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "ETag passt nicht", body = Problem, content_type = "application/problem+json"),
        (status = 428, description = "If-Match fehlt (F38)", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub(crate) async fn delete_admin_api_key(
    State(st): State<AdminState>,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> ApiResult {
    p.require(Permission::Write)?;
    let current = get_or_404(&st.store, &name).await?;
    let expect = check_if_match(
        &headers,
        &admin_key_etag(&current),
        Some(current.updated_at),
    )?;
    st.store
        .delete_admin_api_key(p.actor(), &name, expect)
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Routen (mit Anmeldung).
pub(crate) fn router() -> Router<AdminState> {
    Router::new()
        .route(
            "/admin/api/v1/admin-api-keys",
            get(list_admin_api_keys).post(create_admin_api_key),
        )
        .route(
            "/admin/api/v1/admin-api-keys/{name}",
            get(get_admin_api_key)
                .put(put_admin_api_key)
                .delete(delete_admin_api_key),
        )
}

/// Prüft `Authorization: Bearer sk-aigw-admin-…`.
#[derive(Debug)]
pub struct AdminKeyAuthenticator {
    store: Store,
}

impl AdminKeyAuthenticator {
    /// Neuer Prüfer.
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    async fn check(&self, token: &str) -> Result<Option<Principal>, StoreError> {
        let Some(k) = self.store.find_admin_api_key(&hash_api_key(token)).await? else {
            return Ok(None);
        };
        let now = OffsetDateTime::now_utc();
        if k.spec.expires_at.is_some_and(|e| e <= now) {
            tracing::debug!(key = %k.name, "Admin-API-Key abgelaufen");
            return Ok(None);
        }
        match k.last_used_at {
            // Erstnutzung: einmalig mit Audit-Ereignis `first-use` (AIGW-66).
            None => {
                self.store.touch_admin_api_key_first(&k, now).await?;
            }
            Some(t) if now - t >= Duration::seconds(TOUCH_INTERVAL_SECS) => {
                self.store.touch_admin_api_key(k.id, now).await?;
            }
            Some(_) => {}
        }
        Ok(Some(Principal {
            subject: format!("admin-key:{}", k.name),
            roles: k
                .spec
                .roles
                .into_iter()
                .filter(rbac::is_effective)
                .collect(),
            method: AuthMethod::AdminApiKey,
            display_name: k.spec.description,
            email: None,
            expires_at: k.spec.expires_at,
            csrf: None,
        }))
    }
}

impl AdminAuthenticator for AdminKeyAuthenticator {
    fn authenticate<'a>(&'a self, headers: &'a HeaderMap) -> BoxFuture<'a, Option<Principal>> {
        Box::pin(async move {
            let token = bearer_token(headers).filter(|t| t.starts_with(ADMIN_KEY_PREFIX))?;
            match self.check(token).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "Prüfung des Admin-API-Keys fehlgeschlagen");
                    None
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_lifetime_bounds_expiry() {
        let now = OffsetDateTime::now_utc();
        // ohne Höchstlaufzeit: wie angegeben (auch unbegrenzt)
        assert_eq!(effective_expiry(None, None).unwrap(), None);
        let t = now + Duration::days(400);
        assert_eq!(effective_expiry(Some(t), None).unwrap(), Some(t));
        assert!(effective_expiry(Some(now - Duration::seconds(1)), None).is_err());
        // mit Höchstlaufzeit (F87): fehlend → jetzt + max, darüber → 422
        let max = 90 * 86_400;
        let e = effective_expiry(None, Some(max)).unwrap().unwrap();
        assert!(
            e > now + Duration::days(89) && e <= now + Duration::days(90) + Duration::seconds(5)
        );
        let ok = now + Duration::days(30);
        assert_eq!(effective_expiry(Some(ok), Some(max)).unwrap(), Some(ok));
        let err = effective_expiry(Some(now + Duration::days(91)), Some(max)).unwrap_err();
        assert_eq!(err.status, 422);
    }
}
