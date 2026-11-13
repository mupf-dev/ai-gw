// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Ressourcen der Admin-API: Organisationen, Teams, Nutzer, API-Keys (AIGW-28) und
//! Modellgruppen (AIGW-65) sowie die generischen CRUD-Abläufe, die auch
//! [`crate::deployments`] und [`crate::prices`] nutzen.
//!
//! Konventionen (Details: `docs/konzept/08-admin-api.md`):
//! - Ressourcen werden über ihren **Namen** adressiert (`/organizations/{name}`), Körper wie
//!   in GitOps-YAML: `{metadata: {name, labels}, spec}`; gelesen zusätzlich `id`.
//! - Listen: `?limit=` (1–500, Standard 50), `?cursor=` (aus `nextCursor`), `?q=`
//!   (Teilstring im Namen), `?label=k=v,k2` und ressourcenspezifische Filter; sortiert nach Name.
//! - `ETag` bei Einzelobjekten, `If-Match` bei Änderungen (optimistische Sperre, `412`).
//! - Fehler als Problem Details ([`Problem`]).

// `#[utoipa::path(params(…))]` erzeugt für IntoParams-Typen qualifizierte Pfade.
#![allow(unused_qualifications)]

use std::collections::HashSet;
use std::fmt::Write as _;
use std::future::Future;

use aigw_core::resource::{
    ApiKey, ApiKeySpec, ApiKeyState, CreatedApiKey, Id, ModelGroupSpec, Object, ObjectMeta,
    OrganizationSpec, TeamSpec, UserSpec,
};
use aigw_core::validate;
use aigw_store::{PlainResource, Store};
use aws_lc_rs::digest;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use utoipa::{IntoParams, ToSchema};

use crate::AdminState;
use crate::auth::{Permission, Principal};
use crate::problem::Problem;

pub(crate) type St = State<AdminState>;
pub(crate) type ApiResult = Result<Response, Problem>;

/// Basis aller Ressourcenpfade.
pub const BASE: &str = "/admin/api/v1";
/// Standardgröße einer Seite.
pub const DEFAULT_LIMIT: u32 = 50;
/// Größte erlaubte Seite.
pub const MAX_LIMIT: u32 = 500;
/// Standard-Übergangszeit bei der Rotation: so lange gilt der alte Key weiter.
pub const DEFAULT_ROTATION_GRACE_SECS: u64 = 24 * 3600;
/// Längste erlaubte Übergangszeit (365 Tage).
pub const MAX_ROTATION_GRACE_SECS: u64 = 365 * 24 * 3600;

// ---------------------------------------------------------------------------------------------
// Hilfen: ETag, Paginierung, Filter
// ---------------------------------------------------------------------------------------------

/// Starker ETag über den Inhalt (SHA-256 des JSON, 128 Bit, in Anführungszeichen).
pub fn etag_of<T: Serialize>(v: &T) -> String {
    let bytes = serde_json::to_vec(v).unwrap_or_default();
    let d = digest::digest(&digest::SHA256, &bytes);
    let mut s = String::with_capacity(34);
    s.push('"');
    for b in d.as_ref().iter().take(16) {
        let _ = write!(s, "{b:02x}");
    }
    s.push('"');
    s
}

/// ETag eines API-Keys – ohne `lastUsedAt`, das sich bei Nutzung ändert, ohne dass jemand
/// den Key bearbeitet hätte.
fn key_etag(k: &ApiKey) -> String {
    etag_of(&(
        &k.id,
        &k.metadata,
        &k.spec,
        k.status.state,
        &k.status.prefix,
    ))
}

/// Prüft `If-Match` (fehlt der Header, wird ohne Prüfung geschrieben).
pub(crate) fn check_if_match(headers: &HeaderMap, current: &str) -> Result<(), Problem> {
    let Some(v) = headers.get(header::IF_MATCH) else {
        return Ok(());
    };
    let v = v
        .to_str()
        .map_err(|_| Problem::bad_request("If-Match ist kein gültiger Header-Wert"))?;
    if v.split(',')
        .map(str::trim)
        .any(|t| t == "*" || t == current)
    {
        Ok(())
    } else {
        Err(Problem::precondition_failed())
    }
}

pub(crate) fn json_with<T: Serialize>(
    status: StatusCode,
    etag: Option<String>,
    body: &T,
) -> Response {
    let mut res = (status, Json(body)).into_response();
    if let Some(tag) = etag.and_then(|t| HeaderValue::from_str(&t).ok()) {
        res.headers_mut().insert(header::ETAG, tag);
    }
    res
}

pub(crate) fn created_at(mut res: Response, location: &str) -> Response {
    if let Ok(v) = HeaderValue::from_str(location) {
        res.headers_mut().insert(header::LOCATION, v);
    }
    res
}

pub(crate) fn no_store(mut res: Response) -> Response {
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

/// Gemeinsame Listenparameter.
pub(crate) struct ListArgs<'a> {
    pub(crate) limit: Option<u32>,
    pub(crate) cursor: Option<&'a str>,
    pub(crate) q: Option<&'a str>,
    pub(crate) label: Option<&'a str>,
}

/// Label-Selektor `k=v,k2` (alle müssen passen; ohne `=` genügt das Vorhandensein).
fn parse_labels(sel: Option<&str>) -> Result<Vec<(String, Option<String>)>, Problem> {
    let Some(sel) = sel else {
        return Ok(Vec::new());
    };
    sel.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|part| match part.split_once('=') {
            Some((k, _)) if k.trim().is_empty() => Err(Problem::bad_request(format!(
                "label: leerer Schlüssel in {part:?}"
            ))),
            Some((k, v)) => Ok((k.trim().to_owned(), Some(v.trim().to_owned()))),
            None => Ok((part.to_owned(), None)),
        })
        .collect()
}

/// Filtert nach `q`/`label` und schneidet die Seite nach `cursor`/`limit` aus.
/// `items` muss nach Name sortiert sein (so liefert sie der Store).
pub(crate) fn paginate<T>(
    items: Vec<T>,
    meta: impl Fn(&T) -> &ObjectMeta,
    args: &ListArgs<'_>,
) -> Result<(Vec<T>, Option<String>), Problem> {
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(Problem::bad_request(format!(
            "limit muss zwischen 1 und {MAX_LIMIT} liegen"
        )));
    }
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    let labels = parse_labels(args.label)?;
    let mut page: Vec<T> = items
        .into_iter()
        .filter(|i| {
            let m = meta(i);
            args.cursor.is_none_or(|c| m.name.as_str() > c)
                && args.q.is_none_or(|q| m.name.contains(q))
                && labels.iter().all(|(k, v)| match (m.labels.get(k), v) {
                    (Some(have), Some(want)) => have == want,
                    (Some(_), None) => true,
                    (None, _) => false,
                })
        })
        .take(limit + 1)
        .collect();
    let next = if page.len() > limit {
        page.truncate(limit);
        page.last().map(|i| meta(i).name.clone())
    } else {
        None
    };
    Ok((page, next))
}

/// Prüft, dass alle Modellnamen (Name oder Alias einer Modellgruppe) existieren.
async fn check_models(store: &Store, models: &[String]) -> Result<(), Problem> {
    if models.is_empty() {
        return Ok(());
    }
    let groups = store.list::<ModelGroupSpec>().await?;
    let known: HashSet<&str> = groups
        .iter()
        .flat_map(|g| {
            std::iter::once(g.metadata.name.as_str())
                .chain(g.spec.aliases.iter().map(String::as_str))
        })
        .collect();
    let unknown: Vec<&str> = models
        .iter()
        .map(String::as_str)
        .filter(|m| !known.contains(m))
        .collect();
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(Problem::invalid(format!(
            "allowedModels: unbekannte Modellgruppen {unknown:?}"
        )))
    }
}

/// Name aus dem Körper muss zum Pfad passen (Umbenennen wird nicht unterstützt).
pub(crate) fn same_name(path: &str, mut meta: ObjectMeta) -> Result<ObjectMeta, Problem> {
    if meta.name.is_empty() {
        meta.name = path.to_owned();
    }
    if meta.name != path {
        return Err(Problem::invalid(format!(
            "metadata.name {:?} passt nicht zum Pfad {path:?} (Umbenennen nicht möglich)",
            meta.name
        )));
    }
    Ok(meta)
}

// ---------------------------------------------------------------------------------------------
// Generische CRUD-Abläufe für Organisationen, Teams und Nutzer
// ---------------------------------------------------------------------------------------------

/// Über die generischen Abläufe verwaltete Ressource.
pub(crate) trait AdminResource: PlainResource {
    /// Gelesene Darstellung (konkreter Typ für die OpenAPI).
    type Res: From<Object<Self>> + Serialize;
    /// Listenfilter.
    type Query;
    /// Pfadsegment (`organizations`).
    const COLLECTION: &'static str;

    /// Gemeinsame Listenparameter.
    fn list_args(q: &Self::Query) -> ListArgs<'_>;

    /// Ressourcenspezifischer Filter.
    fn filter(_o: &Object<Self>, _q: &Self::Query) -> bool {
        true
    }

    /// Modellfreigaben (werden beim Schreiben gegen die Modellgruppen geprüft).
    fn allowed_models(&self) -> &[String] {
        &[]
    }

    /// Filter über die ganze (nach Name sortierte) Liste, z. B. „nur gültige Preisversion“.
    fn post_filter(
        items: Vec<Object<Self>>,
        _q: &Self::Query,
    ) -> Result<Vec<Object<Self>>, Problem> {
        Ok(items)
    }

    /// Name beim Anlegen ohne `metadata.name` (`None` = Name ist Pflicht).
    fn default_name(_spec: &Self) -> Option<String> {
        None
    }

    /// Inhaltliche Prüfung vor dem Schreiben (unter der Schreibsperre); `current` ist beim
    /// Ersetzen der bisherige Stand.
    fn check<'a>(
        _st: &'a AdminState,
        _name: &'a str,
        _spec: &'a Self,
        _current: Option<&'a Object<Self>>,
    ) -> impl Future<Output = Result<(), Problem>> + Send + 'a {
        async { Ok(()) }
    }

    /// Prüfung vor dem Löschen (unter der Schreibsperre), z. B. Verweise ohne Fremdschlüssel.
    fn check_delete<'a>(
        _st: &'a AdminState,
        _current: &'a Object<Self>,
    ) -> impl Future<Output = Result<(), Problem>> + Send + 'a {
        async { Ok(()) }
    }
}

pub(crate) async fn list_impl<R: AdminResource>(
    st: &AdminState,
    q: &R::Query,
) -> Result<(Vec<R::Res>, Option<String>), Problem> {
    let items: Vec<Object<R>> = st
        .store
        .list::<R>()
        .await?
        .into_iter()
        .filter(|o| R::filter(o, q))
        .collect();
    let items = R::post_filter(items, q)?;
    let (page, next) = paginate(items, |o| &o.metadata, &R::list_args(q))?;
    Ok((page.into_iter().map(R::Res::from).collect(), next))
}

pub(crate) async fn get_impl<R: AdminResource>(st: &AdminState, name: &str) -> ApiResult {
    let obj: Object<R> = st.store.get(name).await?;
    let tag = etag_of(&obj);
    Ok(json_with(StatusCode::OK, Some(tag), &R::Res::from(obj)))
}

pub(crate) async fn create_impl<R: AdminResource>(
    st: &AdminState,
    p: &Principal,
    mut meta: ObjectMeta,
    spec: R,
) -> ApiResult {
    p.require(Permission::Write)?;
    if meta.name.is_empty()
        && let Some(name) = R::default_name(&spec)
    {
        meta.name = name;
    }
    check_models(&st.store, spec.allowed_models()).await?;
    let _guard = st.write_lock.lock().await;
    R::check(st, &meta.name, &spec, None).await?;
    let obj = st.store.create(p.actor(), meta, spec).await?;
    let tag = etag_of(&obj);
    let location = format!("{BASE}/{}/{}", R::COLLECTION, obj.metadata.name);
    Ok(created_at(
        json_with(StatusCode::CREATED, Some(tag), &R::Res::from(obj)),
        &location,
    ))
}

pub(crate) async fn put_impl<R: AdminResource>(
    st: &AdminState,
    p: &Principal,
    name: &str,
    headers: &HeaderMap,
    meta: ObjectMeta,
    spec: R,
) -> ApiResult {
    p.require(Permission::Write)?;
    let meta = same_name(name, meta)?;
    check_models(&st.store, spec.allowed_models()).await?;
    let _guard = st.write_lock.lock().await;
    let current: Object<R> = st.store.get(name).await?;
    check_if_match(headers, &etag_of(&current))?;
    R::check(st, name, &spec, Some(&current)).await?;
    let obj = st.store.update(p.actor(), meta, spec).await?;
    let tag = etag_of(&obj);
    Ok(json_with(StatusCode::OK, Some(tag), &R::Res::from(obj)))
}

pub(crate) async fn delete_impl<R: AdminResource>(
    st: &AdminState,
    p: &Principal,
    name: &str,
    headers: &HeaderMap,
) -> ApiResult {
    p.require(Permission::Write)?;
    let _guard = st.write_lock.lock().await;
    let current: Object<R> = st.store.get(name).await?;
    check_if_match(headers, &etag_of(&current))?;
    R::check_delete(st, &current).await?;
    st.store.delete(p.actor(), R::KIND, name).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Erzeugt Darstellung, Eingabe- und Listentyp sowie die fünf CRUD-Handler einer Art.
macro_rules! plain_resource {
    (
        spec = $spec:ident, res = $res:ident, input = $input:ident, list = $list:ident,
        query = $query:ident, tag = $tag:tt,
        path = $path:tt, item_path = $item:tt, what = $what:tt,
        handlers = [$list_fn:ident, $create_fn:ident, $get_fn:ident, $put_fn:ident, $delete_fn:ident]
        $(, input_meta = [$($iattr:meta),*])?
    ) => {
        #[doc = concat!($what, " (gelesen).")]
        #[derive(Debug, Clone, Serialize, ToSchema)]
        #[serde(rename_all = "camelCase")]
        pub struct $res {
            /// ID (UUIDv7).
            #[schema(value_type = String, format = Uuid)]
            pub id: Id,
            /// Metadaten.
            pub metadata: ObjectMeta,
            /// Spezifikation.
            pub spec: $spec,
        }

        impl From<Object<$spec>> for $res {
            fn from(o: Object<$spec>) -> Self {
                Self {
                    id: o.id,
                    metadata: o.metadata,
                    spec: o.spec,
                }
            }
        }

        #[doc = concat!($what, " (Eingabe für Anlegen und Ersetzen).")]
        #[derive(Debug, Clone, Deserialize, ToSchema)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        pub struct $input {
            /// Metadaten (`name`, `labels`; vom Server gepflegte Felder werden ignoriert).
            $($(#[$iattr])*)?
            pub metadata: ObjectMeta,
            /// Spezifikation.
            pub spec: $spec,
        }

        #[doc = concat!("Seite einer Liste: ", $what, ".")]
        #[derive(Debug, Clone, Serialize, ToSchema)]
        #[serde(rename_all = "camelCase")]
        pub struct $list {
            /// Einträge (nach Name sortiert).
            pub items: Vec<$res>,
            /// Cursor der nächsten Seite (`?cursor=`), fehlt auf der letzten Seite.
            #[serde(skip_serializing_if = "Option::is_none")]
            pub next_cursor: Option<String>,
        }

        #[doc = concat!("Listet: ", $what, ".")]
        #[utoipa::path(
            get, path = $path, tag = $tag, params($query),
            responses(
                (status = 200, description = "Seite", body = $list),
                (status = 400, description = "Ungültige Parameter", body = Problem, content_type = "application/problem+json"),
                (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
                (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
            ),
            security(("bearer" = []))
        )]
        pub(crate) async fn $list_fn(
            State(st): St,
            Extension(p): Extension<Principal>,
            q: Result<Query<$query>, QueryRejection>,
        ) -> ApiResult {
            p.require(Permission::Read)?;
            let Query(q) = q?;
            let (items, next_cursor) = list_impl::<$spec>(&st, &q).await?;
            Ok(json_with(StatusCode::OK, None, &$list { items, next_cursor }))
        }

        #[doc = concat!("Legt an: ", $what, ".")]
        #[utoipa::path(
            post, path = $path, tag = $tag, request_body = $input,
            responses(
                (status = 201, description = "Angelegt", body = $res,
                    headers(("ETag" = String, description = "Version"), ("Location" = String, description = "Pfad"))),
                (status = 400, description = "Ungültige Anfrage", body = Problem, content_type = "application/problem+json"),
                (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
                (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
                (status = 409, description = "Name bereits vergeben", body = Problem, content_type = "application/problem+json"),
                (status = 422, description = "Ungültige Eingabe oder Verweis", body = Problem, content_type = "application/problem+json"),
            ),
            security(("bearer" = []))
        )]
        pub(crate) async fn $create_fn(
            State(st): St,
            Extension(p): Extension<Principal>,
            body: Result<Json<$input>, JsonRejection>,
        ) -> ApiResult {
            let Json(input) = body?;
            create_impl(&st, &p, input.metadata, input.spec).await
        }

        #[doc = concat!("Liest: ", $what, ".")]
        #[utoipa::path(
            get, path = $item, tag = $tag,
            params(("name" = String, Path, description = "Name")),
            responses(
                (status = 200, description = "Gefunden", body = $res,
                    headers(("ETag" = String, description = "Version für If-Match"))),
                (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
                (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
                (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
            ),
            security(("bearer" = []))
        )]
        pub(crate) async fn $get_fn(
            State(st): St,
            Extension(p): Extension<Principal>,
            Path(name): Path<String>,
        ) -> ApiResult {
            p.require(Permission::Read)?;
            get_impl::<$spec>(&st, &name).await
        }

        #[doc = concat!("Ersetzt Labels und Spezifikation: ", $what, ".")]
        #[utoipa::path(
            put, path = $item, tag = $tag, request_body = $input,
            params(
                ("name" = String, Path, description = "Name"),
                ("If-Match" = Option<String>, Header, description = "ETag aus dem letzten Lesen (optimistische Sperre)"),
            ),
            responses(
                (status = 200, description = "Geändert", body = $res,
                    headers(("ETag" = String, description = "Neue Version"))),
                (status = 400, description = "Ungültige Anfrage", body = Problem, content_type = "application/problem+json"),
                (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
                (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
                (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
                (status = 412, description = "ETag passt nicht", body = Problem, content_type = "application/problem+json"),
                (status = 422, description = "Ungültige Eingabe oder Verweis", body = Problem, content_type = "application/problem+json"),
            ),
            security(("bearer" = []))
        )]
        pub(crate) async fn $put_fn(
            State(st): St,
            Extension(p): Extension<Principal>,
            Path(name): Path<String>,
            headers: HeaderMap,
            body: Result<Json<$input>, JsonRejection>,
        ) -> ApiResult {
            let Json(input) = body?;
            put_impl(&st, &p, &name, &headers, input.metadata, input.spec).await
        }

        #[doc = concat!("Löscht: ", $what, ".")]
        #[utoipa::path(
            delete, path = $item, tag = $tag,
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
        pub(crate) async fn $delete_fn(
            State(st): St,
            Extension(p): Extension<Principal>,
            Path(name): Path<String>,
            headers: HeaderMap,
        ) -> ApiResult {
            delete_impl::<$spec>(&st, &p, &name, &headers).await
        }
    };
}

pub(crate) use plain_resource;

// ---------------------------------------------------------------------------------------------
// Organisationen
// ---------------------------------------------------------------------------------------------

/// Filter für Organisationen.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct OrganizationQuery {
    /// Seitengröße (1–500, Standard 50).
    pub limit: Option<u32>,
    /// Fortsetzung (`nextCursor` der vorigen Seite).
    pub cursor: Option<String>,
    /// Teilstring im Namen.
    pub q: Option<String>,
    /// Label-Selektor `k=v,k2`.
    pub label: Option<String>,
}

plain_resource!(
    spec = OrganizationSpec,
    res = Organization,
    input = OrganizationInput,
    list = OrganizationList,
    query = OrganizationQuery,
    tag = "organizations",
    path = "/admin/api/v1/organizations",
    item_path = "/admin/api/v1/organizations/{name}",
    what = "Organisation",
    handlers = [
        list_organizations,
        create_organization,
        get_organization,
        put_organization,
        delete_organization
    ]
);

impl AdminResource for OrganizationSpec {
    type Res = Organization;
    type Query = OrganizationQuery;
    const COLLECTION: &'static str = "organizations";

    fn list_args(q: &Self::Query) -> ListArgs<'_> {
        ListArgs {
            limit: q.limit,
            cursor: q.cursor.as_deref(),
            q: q.q.as_deref(),
            label: q.label.as_deref(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Teams
// ---------------------------------------------------------------------------------------------

/// Filter für Teams.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct TeamQuery {
    /// Seitengröße (1–500, Standard 50).
    pub limit: Option<u32>,
    /// Fortsetzung (`nextCursor` der vorigen Seite).
    pub cursor: Option<String>,
    /// Teilstring im Namen.
    pub q: Option<String>,
    /// Label-Selektor `k=v,k2`.
    pub label: Option<String>,
    /// Nur Teams dieser Organisation (Name).
    pub organization: Option<String>,
}

plain_resource!(
    spec = TeamSpec,
    res = Team,
    input = TeamInput,
    list = TeamList,
    query = TeamQuery,
    tag = "teams",
    path = "/admin/api/v1/teams",
    item_path = "/admin/api/v1/teams/{name}",
    what = "Team",
    handlers = [list_teams, create_team, get_team, put_team, delete_team]
);

impl AdminResource for TeamSpec {
    type Res = Team;
    type Query = TeamQuery;
    const COLLECTION: &'static str = "teams";

    fn list_args(q: &Self::Query) -> ListArgs<'_> {
        ListArgs {
            limit: q.limit,
            cursor: q.cursor.as_deref(),
            q: q.q.as_deref(),
            label: q.label.as_deref(),
        }
    }

    fn filter(o: &Object<Self>, q: &Self::Query) -> bool {
        q.organization
            .as_ref()
            .is_none_or(|org| *org == o.spec.organization)
    }

    fn allowed_models(&self) -> &[String] {
        &self.allowed_models
    }
}

// ---------------------------------------------------------------------------------------------
// Nutzer
// ---------------------------------------------------------------------------------------------

/// Filter für Nutzer.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct UserQuery {
    /// Seitengröße (1–500, Standard 50).
    pub limit: Option<u32>,
    /// Fortsetzung (`nextCursor` der vorigen Seite).
    pub cursor: Option<String>,
    /// Teilstring im Namen.
    pub q: Option<String>,
    /// Label-Selektor `k=v,k2`.
    pub label: Option<String>,
    /// Exakte E-Mail-Adresse (ohne Beachtung der Groß-/Kleinschreibung).
    pub email: Option<String>,
}

plain_resource!(
    spec = UserSpec,
    res = User,
    input = UserInput,
    list = UserList,
    query = UserQuery,
    tag = "users",
    path = "/admin/api/v1/users",
    item_path = "/admin/api/v1/users/{name}",
    what = "Nutzer",
    handlers = [list_users, create_user, get_user, put_user, delete_user]
);

impl AdminResource for UserSpec {
    type Res = User;
    type Query = UserQuery;
    const COLLECTION: &'static str = "users";

    fn list_args(q: &Self::Query) -> ListArgs<'_> {
        ListArgs {
            limit: q.limit,
            cursor: q.cursor.as_deref(),
            q: q.q.as_deref(),
            label: q.label.as_deref(),
        }
    }

    fn filter(o: &Object<Self>, q: &Self::Query) -> bool {
        q.email.as_ref().is_none_or(|want| {
            o.spec
                .email
                .as_ref()
                .is_some_and(|e| e.eq_ignore_ascii_case(want))
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Modellgruppen
// ---------------------------------------------------------------------------------------------

/// Filter für Modellgruppen.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct ModelGroupQuery {
    /// Seitengröße (1–500, Standard 50).
    pub limit: Option<u32>,
    /// Fortsetzung (`nextCursor` der vorigen Seite).
    pub cursor: Option<String>,
    /// Teilstring im Namen oder in einem Alias.
    pub q: Option<String>,
    /// Label-Selektor `k=v,k2`.
    pub label: Option<String>,
}

plain_resource!(
    spec = ModelGroupSpec,
    res = ModelGroup,
    input = ModelGroupInput,
    list = ModelGroupList,
    query = ModelGroupQuery,
    tag = "model-groups",
    path = "/admin/api/v1/model-groups",
    item_path = "/admin/api/v1/model-groups/{name}",
    what = "Modellgruppe",
    handlers = [
        list_model_groups,
        create_model_group,
        get_model_group,
        put_model_group,
        delete_model_group
    ]
);

impl AdminResource for ModelGroupSpec {
    type Res = ModelGroup;
    type Query = ModelGroupQuery;
    const COLLECTION: &'static str = "model-groups";

    fn list_args(q: &Self::Query) -> ListArgs<'_> {
        // `q` sucht auch in Aliassen, daher in `filter` statt in `paginate`.
        ListArgs {
            limit: q.limit,
            cursor: q.cursor.as_deref(),
            q: None,
            label: q.label.as_deref(),
        }
    }

    fn filter(g: &Object<Self>, q: &Self::Query) -> bool {
        q.q.as_deref().is_none_or(|s| {
            g.metadata.name.contains(s) || g.spec.aliases.iter().any(|a| a.contains(s))
        })
    }

    async fn check(
        st: &AdminState,
        name: &str,
        spec: &Self,
        _current: Option<&Object<Self>>,
    ) -> Result<(), Problem> {
        validate::aliases(name, spec).map_err(Problem::invalid)?;
        group_settings(&spec.settings).map_err(Problem::invalid)?;
        let groups = st.store.list::<ModelGroupSpec>().await?;
        let by_name = groups
            .iter()
            .map(|g| (g.metadata.name.as_str(), &g.spec))
            .collect();
        validate::fallbacks(&by_name, name, spec).map_err(Problem::invalid)
    }

    async fn check_delete(st: &AdminState, current: &Object<Self>) -> Result<(), Problem> {
        let name = &current.metadata.name;
        let users: Vec<String> = st
            .store
            .list::<ModelGroupSpec>()
            .await?
            .into_iter()
            .filter(|g| g.spec.fallbacks.iter().any(|f| f.model_group == *name))
            .map(|g| g.metadata.name)
            .collect();
        if users.is_empty() {
            Ok(())
        } else {
            Err(Problem::conflict(format!(
                "Modellgruppe {name:?} ist Fallback-Ziel von {users:?}"
            )))
        }
    }
}

/// Prüft `settings` einer Modellgruppe (`retry`, `circuitBreaker`, `healthCheck`) mit dem
/// Parser der Datenebene ([`aigw_proxy::routing::GroupPolicy::from_settings`]), damit
/// Tippfehler beim Schreiben auffallen statt still durch Standardwerte ersetzt zu werden.
/// Auch als `ApplyOptions::group_settings` für GitOps.
pub fn group_settings(settings: &aigw_core::resource::JsonObject) -> Result<(), String> {
    aigw_proxy::routing::GroupPolicy::from_settings(settings)
        .map(drop)
        .map_err(|e| format!("settings: {e}"))
}

// ---------------------------------------------------------------------------------------------
// API-Keys
// ---------------------------------------------------------------------------------------------

/// API-Key (Eingabe für Anlegen und Ersetzen).
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApiKeyInput {
    /// Metadaten (`name`, `labels`).
    pub metadata: ObjectMeta,
    /// Spezifikation.
    pub spec: ApiKeySpec,
}

/// Seite einer Liste: API-Keys (ohne Klartext, nur Präfix).
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyList {
    /// Einträge (nach Name sortiert).
    pub items: Vec<ApiKey>,
    /// Cursor der nächsten Seite (`?cursor=`), fehlt auf der letzten Seite.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Filter für API-Keys.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct ApiKeyQuery {
    /// Seitengröße (1–500, Standard 50).
    pub limit: Option<u32>,
    /// Fortsetzung (`nextCursor` der vorigen Seite).
    pub cursor: Option<String>,
    /// Teilstring im Namen oder Präfix.
    pub q: Option<String>,
    /// Label-Selektor `k=v,k2`.
    pub label: Option<String>,
    /// Nur Keys dieses Teams (Name).
    pub team: Option<String>,
    /// Nur Keys dieses Nutzers (Name).
    pub user: Option<String>,
    /// Nur Keys von Teams dieser Organisation (Name).
    pub organization: Option<String>,
    /// Nur Keys in diesem Zustand.
    pub state: Option<ApiKeyState>,
}

/// Rotation eines API-Keys.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RotateApiKeyRequest {
    /// Name des neuen Keys (Standard: alter Name mit Zeitstempel `-JJJJMMTThhmmss`).
    #[serde(default)]
    pub name: Option<String>,
    /// So lange gilt der alte Key noch (Sekunden, Standard 86400, `0` = sofort ungültig).
    #[serde(default)]
    pub grace_period_seconds: Option<u64>,
}

/// Ergebnis der Rotation.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RotatedApiKey {
    /// Neuer Key samt Klartext (nur in dieser Antwort).
    pub new_key: CreatedApiKey,
    /// Alter Key mit gesetztem Ablauf.
    pub previous_key: ApiKey,
}

/// Listet API-Keys (nur Präfix, nie Klartext oder Hash).
#[utoipa::path(
    get, path = "/admin/api/v1/api-keys", tag = "api-keys", params(ApiKeyQuery),
    responses(
        (status = 200, description = "Seite", body = ApiKeyList),
        (status = 400, description = "Ungültige Parameter", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn list_api_keys(
    State(st): St,
    Extension(p): Extension<Principal>,
    q: Result<Query<ApiKeyQuery>, QueryRejection>,
) -> ApiResult {
    p.require(Permission::Read)?;
    let Query(q) = q?;
    // Teams der Organisation (nur wenn danach gefiltert wird).
    let org_teams: Option<HashSet<String>> = match &q.organization {
        Some(org) => Some(
            st.store
                .list::<TeamSpec>()
                .await?
                .into_iter()
                .filter(|t| t.spec.organization == *org)
                .map(|t| t.metadata.name)
                .collect(),
        ),
        None => None,
    };
    let items: Vec<ApiKey> = st
        .store
        .list_api_keys()
        .await?
        .into_iter()
        .filter(|k| {
            q.q.as_deref()
                .is_none_or(|s| k.metadata.name.contains(s) || k.status.prefix.contains(s))
                && q.team
                    .as_ref()
                    .is_none_or(|t| k.spec.team.as_ref() == Some(t))
                && q.user
                    .as_ref()
                    .is_none_or(|u| k.spec.user.as_ref() == Some(u))
                && q.state.is_none_or(|s| k.status.state == s)
                && org_teams
                    .as_ref()
                    .is_none_or(|ts| k.spec.team.as_ref().is_some_and(|t| ts.contains(t)))
        })
        .collect();
    let args = ListArgs {
        limit: q.limit,
        cursor: q.cursor.as_deref(),
        q: None,
        label: q.label.as_deref(),
    };
    let (items, next_cursor) = paginate(items, |k| &k.metadata, &args)?;
    Ok(json_with(
        StatusCode::OK,
        None,
        &ApiKeyList { items, next_cursor },
    ))
}

/// Legt einen API-Key an. Der Klartext (`secret`) steht **nur** in dieser Antwort.
#[utoipa::path(
    post, path = "/admin/api/v1/api-keys", tag = "api-keys", request_body = ApiKeyInput,
    responses(
        (status = 201, description = "Angelegt (mit Klartext)", body = CreatedApiKey,
            headers(("ETag" = String, description = "Version"), ("Location" = String, description = "Pfad"))),
        (status = 400, description = "Ungültige Anfrage", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Name bereits vergeben", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Ungültige Eingabe oder Verweis", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn create_api_key(
    State(st): St,
    Extension(p): Extension<Principal>,
    body: Result<Json<ApiKeyInput>, JsonRejection>,
) -> ApiResult {
    p.require(Permission::Write)?;
    let Json(input) = body?;
    check_models(&st.store, &input.spec.allowed_models).await?;
    let created = st
        .store
        .create_api_key(p.actor(), input.metadata, input.spec)
        .await?;
    let tag = key_etag(&created.key);
    let location = format!("{BASE}/api-keys/{}", created.key.metadata.name);
    Ok(no_store(created_at(
        json_with(StatusCode::CREATED, Some(tag), &created),
        &location,
    )))
}

/// Liest einen API-Key (nur Präfix).
#[utoipa::path(
    get, path = "/admin/api/v1/api-keys/{name}", tag = "api-keys",
    params(("name" = String, Path, description = "Name")),
    responses(
        (status = 200, description = "Gefunden", body = ApiKey,
            headers(("ETag" = String, description = "Version für If-Match"))),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn get_api_key(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
) -> ApiResult {
    p.require(Permission::Read)?;
    let k = st.store.get_api_key(&name).await?;
    Ok(json_with(StatusCode::OK, Some(key_etag(&k)), &k))
}

/// Ersetzt Labels und Spezifikation eines API-Keys (Präfix, Hash und Zustand bleiben).
#[utoipa::path(
    put, path = "/admin/api/v1/api-keys/{name}", tag = "api-keys", request_body = ApiKeyInput,
    params(
        ("name" = String, Path, description = "Name"),
        ("If-Match" = Option<String>, Header, description = "ETag aus dem letzten Lesen (optimistische Sperre)"),
    ),
    responses(
        (status = 200, description = "Geändert", body = ApiKey,
            headers(("ETag" = String, description = "Neue Version"))),
        (status = 400, description = "Ungültige Anfrage", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "ETag passt nicht", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Ungültige Eingabe oder Verweis", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn put_api_key(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Result<Json<ApiKeyInput>, JsonRejection>,
) -> ApiResult {
    p.require(Permission::Write)?;
    let Json(input) = body?;
    let meta = same_name(&name, input.metadata)?;
    check_models(&st.store, &input.spec.allowed_models).await?;
    let _guard = st.write_lock.lock().await;
    let current = st.store.get_api_key(&name).await?;
    check_if_match(&headers, &key_etag(&current))?;
    let k = st.store.update_api_key(p.actor(), meta, input.spec).await?;
    Ok(json_with(StatusCode::OK, Some(key_etag(&k)), &k))
}

/// Löscht einen API-Key.
#[utoipa::path(
    delete, path = "/admin/api/v1/api-keys/{name}", tag = "api-keys",
    params(
        ("name" = String, Path, description = "Name"),
        ("If-Match" = Option<String>, Header, description = "ETag aus dem letzten Lesen (optimistische Sperre)"),
    ),
    responses(
        (status = 204, description = "Gelöscht"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "ETag passt nicht", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn delete_api_key(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> ApiResult {
    p.require(Permission::Write)?;
    let _guard = st.write_lock.lock().await;
    let current = st.store.get_api_key(&name).await?;
    check_if_match(&headers, &key_etag(&current))?;
    st.store
        .delete(p.actor(), aigw_core::resource::ResourceKind::ApiKey, &name)
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn set_state(
    st: &AdminState,
    p: &Principal,
    name: &str,
    headers: &HeaderMap,
    state: ApiKeyState,
) -> ApiResult {
    p.require(Permission::Write)?;
    let _guard = st.write_lock.lock().await;
    let current = st.store.get_api_key(name).await?;
    check_if_match(headers, &key_etag(&current))?;
    let k = if current.status.state == state {
        current // idempotent, keine neue Konfigurationsversion
    } else {
        st.store.set_api_key_state(p.actor(), name, state).await?
    };
    Ok(json_with(StatusCode::OK, Some(key_etag(&k)), &k))
}

/// Sperrt einen API-Key (wirkt nach dem nächsten Snapshot-Neubau, i. d. R. < 1 s).
#[utoipa::path(
    post, path = "/admin/api/v1/api-keys/{name}/revoke", tag = "api-keys",
    params(
        ("name" = String, Path, description = "Name"),
        ("If-Match" = Option<String>, Header, description = "ETag aus dem letzten Lesen (optimistische Sperre)"),
    ),
    responses(
        (status = 200, description = "Gesperrt", body = ApiKey,
            headers(("ETag" = String, description = "Neue Version"))),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "ETag passt nicht", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn revoke_api_key(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> ApiResult {
    set_state(&st, &p, &name, &headers, ApiKeyState::Revoked).await
}

/// Entsperrt einen gesperrten API-Key.
#[utoipa::path(
    post, path = "/admin/api/v1/api-keys/{name}/reactivate", tag = "api-keys",
    params(
        ("name" = String, Path, description = "Name"),
        ("If-Match" = Option<String>, Header, description = "ETag aus dem letzten Lesen (optimistische Sperre)"),
    ),
    responses(
        (status = 200, description = "Wieder aktiv", body = ApiKey,
            headers(("ETag" = String, description = "Neue Version"))),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "ETag passt nicht", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn reactivate_api_key(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> ApiResult {
    set_state(&st, &p, &name, &headers, ApiKeyState::Active).await
}

/// Standardname des rotierten Keys: Basisname (ohne früheren Zeitstempel) + `-JJJJMMTThhmmss`.
fn rotated_name(old: &str, now: OffsetDateTime) -> String {
    let base = match old.rsplit_once('-') {
        Some((b, ts)) if ts.len() == 14 && ts.bytes().all(|c| c.is_ascii_digit()) => b,
        _ => old,
    };
    let stamp = format!(
        "{:04}{:02}{:02}{:02}{:02}{:02}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    );
    // Name max. 128 Zeichen; Namen sind ASCII, daher ist das Kürzen nach Bytes sicher.
    let keep = 128 - stamp.len() - 1;
    let base = base.get(..base.len().min(keep)).unwrap_or(base);
    format!("{base}-{stamp}")
}

/// Rotiert einen API-Key: neuer Key mit gleicher Spezifikation (Klartext nur in dieser
/// Antwort), der alte läuft nach der Übergangszeit ab.
#[utoipa::path(
    post, path = "/admin/api/v1/api-keys/{name}/rotate", tag = "api-keys",
    request_body(content = RotateApiKeyRequest, description = "Optional; leerer Körper = Standardwerte"),
    params(
        ("name" = String, Path, description = "Name des bisherigen Keys"),
        ("If-Match" = Option<String>, Header, description = "ETag aus dem letzten Lesen (optimistische Sperre)"),
    ),
    responses(
        (status = 201, description = "Neuer Key angelegt (mit Klartext)", body = RotatedApiKey,
            headers(("Location" = String, description = "Pfad des neuen Keys"))),
        (status = 400, description = "Ungültige Anfrage", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Key gesperrt oder neuer Name vergeben", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "ETag passt nicht", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn rotate_api_key(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult {
    p.require(Permission::Write)?;
    let req: RotateApiKeyRequest = if body.iter().all(u8::is_ascii_whitespace) {
        RotateApiKeyRequest::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| Problem::bad_request(e.to_string()))?
    };
    let grace = req
        .grace_period_seconds
        .unwrap_or(DEFAULT_ROTATION_GRACE_SECS);
    if grace > MAX_ROTATION_GRACE_SECS {
        return Err(Problem::invalid(format!(
            "gracePeriodSeconds höchstens {MAX_ROTATION_GRACE_SECS}"
        )));
    }
    let now = OffsetDateTime::now_utc();
    let old_expires = now + time::Duration::seconds(i64::try_from(grace).unwrap_or(0));
    let new_name = req.name.unwrap_or_else(|| rotated_name(&name, now));

    let _guard = st.write_lock.lock().await;
    let current = st.store.get_api_key(&name).await?;
    check_if_match(&headers, &key_etag(&current))?;
    if current.status.state != ApiKeyState::Active {
        return Err(Problem::conflict(
            "Gesperrte Keys können nicht rotiert werden",
        ));
    }
    let labels = current.metadata.labels.clone();
    let meta = ObjectMeta {
        name: new_name,
        labels,
        ..ObjectMeta::default()
    };
    let (new_key, previous_key) = st
        .store
        .rotate_api_key(p.actor(), &name, meta, old_expires)
        .await?;
    let location = format!("{BASE}/api-keys/{}", new_key.key.metadata.name);
    Ok(no_store(created_at(
        json_with(
            StatusCode::CREATED,
            None,
            &RotatedApiKey {
                new_key,
                previous_key,
            },
        ),
        &location,
    )))
}

// ---------------------------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------------------------

/// Routen der Ressourcen (ohne Authentifizierungsschicht).
pub(crate) fn router() -> Router<AdminState> {
    Router::new()
        .route(
            "/admin/api/v1/organizations",
            get(list_organizations).post(create_organization),
        )
        .route(
            "/admin/api/v1/organizations/{name}",
            get(get_organization)
                .put(put_organization)
                .delete(delete_organization),
        )
        .route("/admin/api/v1/teams", get(list_teams).post(create_team))
        .route(
            "/admin/api/v1/teams/{name}",
            get(get_team).put(put_team).delete(delete_team),
        )
        .route("/admin/api/v1/users", get(list_users).post(create_user))
        .route(
            "/admin/api/v1/users/{name}",
            get(get_user).put(put_user).delete(delete_user),
        )
        .route(
            "/admin/api/v1/api-keys",
            get(list_api_keys).post(create_api_key),
        )
        .route(
            "/admin/api/v1/api-keys/{name}",
            get(get_api_key).put(put_api_key).delete(delete_api_key),
        )
        .route("/admin/api/v1/api-keys/{name}/revoke", post(revoke_api_key))
        .route(
            "/admin/api/v1/api-keys/{name}/reactivate",
            post(reactivate_api_key),
        )
        .route("/admin/api/v1/api-keys/{name}/rotate", post(rotate_api_key))
        .route(
            "/admin/api/v1/model-groups",
            get(list_model_groups).post(create_model_group),
        )
        .route(
            "/admin/api/v1/model-groups/{name}",
            get(get_model_group)
                .put(put_model_group)
                .delete(delete_model_group),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(name: &str, labels: &[(&str, &str)]) -> ObjectMeta {
        ObjectMeta {
            name: name.into(),
            labels: labels
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            ..ObjectMeta::default()
        }
    }

    #[test]
    fn pagination_with_cursor_and_filters() {
        let items: Vec<ObjectMeta> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|n| meta(n, &[("env", if *n < "c" { "dev" } else { "prod" })]))
            .collect();
        let args = |limit, cursor, label| ListArgs {
            limit: Some(limit),
            cursor,
            q: None,
            label,
        };
        let (p, next) = paginate(items.clone(), |m| m, &args(2, None, None)).unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(next.as_deref(), Some("b"));
        let (p, next) = paginate(items.clone(), |m| m, &args(2, Some("b"), None)).unwrap();
        assert_eq!(p[0].name, "c");
        assert_eq!(next.as_deref(), Some("d"));
        let (p, next) = paginate(items.clone(), |m| m, &args(2, Some("d"), None)).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(next, None);
        let (p, _) = paginate(items.clone(), |m| m, &args(50, None, Some("env=prod"))).unwrap();
        assert_eq!(p.len(), 3);
        let (p, _) = paginate(items.clone(), |m| m, &args(50, None, Some("missing"))).unwrap();
        assert!(p.is_empty());
        assert!(paginate(items.clone(), |m| m, &args(0, None, None)).is_err());
        assert!(paginate(items, |m| m, &args(501, None, None)).is_err());
    }

    #[test]
    fn if_match_semantics() {
        let mut h = HeaderMap::new();
        assert!(check_if_match(&h, "\"a\"").is_ok());
        h.insert(header::IF_MATCH, HeaderValue::from_static("\"a\""));
        assert!(check_if_match(&h, "\"a\"").is_ok());
        assert_eq!(check_if_match(&h, "\"b\"").unwrap_err().status, 412);
        h.insert(header::IF_MATCH, HeaderValue::from_static("*"));
        assert!(check_if_match(&h, "\"b\"").is_ok());
        h.insert(header::IF_MATCH, HeaderValue::from_static("\"x\", \"b\""));
        assert!(check_if_match(&h, "\"b\"").is_ok());
    }

    #[test]
    fn etag_changes_with_content() {
        let a = meta("a", &[]);
        let b = meta("a", &[("x", "y")]);
        assert_ne!(etag_of(&a), etag_of(&b));
        assert_eq!(etag_of(&a), etag_of(&a.clone()));
        assert_eq!(etag_of(&a).len(), 34);
    }

    #[test]
    fn rotated_names() {
        let t = OffsetDateTime::from_unix_timestamp(1_763_000_000).unwrap();
        assert_eq!(rotated_name("ci", t), "ci-20251113021320");
        assert_eq!(rotated_name("ci-20240101000000", t), "ci-20251113021320");
        let long = "a".repeat(128);
        assert_eq!(rotated_name(&long, t).len(), 128);
    }
}
