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
//!   Filter, Geltungsbereich und Keyset-Cursor laufen in SQL (F42, `Store::list_page`).
//! - `ETag` bei Einzelobjekten, `If-Match` **Pflicht** bei Änderungen, Löschen und Aktionen
//!   (F38: fehlt er → `428`, passt er nicht → `412`; `*` erlaubt). Der Versionsvergleich läuft
//!   zusätzlich im `UPDATE`/`DELETE` der Datenbank ([`aigw_store::Expect`]) – dicht auch
//!   zwischen mehreren Instanzen.
//! - Fehler als Problem Details ([`Problem`]).

// `#[utoipa::path(params(…))]` erzeugt für IntoParams-Typen qualifizierte Pfade.
#![allow(unused_qualifications)]

use std::collections::{BTreeSet, HashSet};
use std::fmt::Write as _;
use std::future::Future;

use aigw_core::resource::{
    ApiKey, ApiKeySpec, ApiKeyState, CreatedApiKey, Id, ModelGroupSpec, Object, ObjectMeta,
    OrganizationSpec, TeamSpec, UserSpec,
};
use aigw_core::validate;
use aigw_store::{
    AuditScope, Expect, ListFilter, ListQuery, Page, PlainResource, Store, WriteExtras,
};
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
use crate::auth::{Permission, Principal, Target};
use crate::bounds::{self, BoundsQuery, KeyBoundsQuery};
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

/// Prüft `If-Match` (F38): Pflicht (`428`, wenn er fehlt), `*` oder eine Liste von ETags;
/// passt keiner → `412`. Liefert den erwarteten Stand für den Versionsvergleich in der
/// Datenbank: bei passendem ETag `updatedAt` des gelesenen Objekts, bei `*` keinen.
pub(crate) fn check_if_match(
    headers: &HeaderMap,
    current: &str,
    updated_at: Option<OffsetDateTime>,
) -> Result<Expect, Problem> {
    let Some(v) = headers.get(header::IF_MATCH) else {
        return Err(Problem::precondition_required());
    };
    let v = v
        .to_str()
        .map_err(|_| Problem::bad_request("If-Match ist kein gültiger Header-Wert"))?;
    let tags: Vec<&str> = v
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    if tags.is_empty() {
        return Err(Problem::precondition_required());
    }
    if tags.contains(&"*") {
        return Ok(Expect::Any);
    }
    if tags.contains(&current) {
        Ok(updated_at.map_or(Expect::Any, Expect::Version))
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

/// Listenabfrage für den Store (F42): Seitengröße prüfen, Label-Selektor zerlegen; Filter
/// und Bereich kommen vom Aufrufer.
pub(crate) fn list_query(
    args: &ListArgs<'_>,
    filters: Vec<ListFilter>,
    scope: AuditScope,
) -> Result<ListQuery, Problem> {
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(Problem::bad_request(format!(
            "limit muss zwischen 1 und {MAX_LIMIT} liegen"
        )));
    }
    Ok(ListQuery {
        limit,
        after: args.cursor.map(str::to_owned),
        search: args.q.filter(|q| !q.is_empty()).map(str::to_owned),
        labels: parse_labels(args.label)?,
        filters,
        scope,
    })
}

/// Seite als `(Einträge, nextCursor)`.
pub(crate) fn page_parts<T, U: From<T>>(page: Page<T>) -> (Vec<U>, Option<String>) {
    (page.items.into_iter().map(U::from).collect(), page.next)
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

    /// Ressourcenspezifische Filter (laufen in SQL, F42).
    fn list_filters(_q: &Self::Query) -> Result<Vec<ListFilter>, Problem> {
        Ok(Vec::new())
    }

    /// Modellfreigaben (werden beim Schreiben gegen die Modellgruppen geprüft).
    fn allowed_models(&self) -> &[String] {
        &[]
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

    /// Zusätzliche Schreibvorgänge beim Löschen (unter der Schreibsperre), z. B. Verweise
    /// entfernen mit `?cascade=remove` (F66).
    fn delete_extras<'a>(
        _st: &'a AdminState,
        _current: &'a Object<Self>,
        _q: &'a BoundsQuery,
    ) -> impl Future<Output = Result<WriteExtras, Problem>> + Send + 'a {
        async { Ok(WriteExtras::default()) }
    }

    /// Grenzen Key ≤ Team ≤ Organisation (AIGW-67, unter der Schreibsperre); liefert
    /// zusätzliche Schreibvorgänge (`?cascade=clamp`, Audit bei `?force=true`).
    fn bounds<'a>(
        _st: &'a AdminState,
        _p: &'a Principal,
        _name: &'a str,
        _spec: &'a Self,
        _current: Option<&'a Object<Self>>,
        _q: &'a BoundsQuery,
    ) -> impl Future<Output = Result<WriteExtras, Problem>> + Send + 'a {
        async { Ok(WriteExtras::default()) }
    }

    /// Geltungsbereich eines Objekts für die Rollenprüfung (AIGW-32).
    fn target(_name: &str, _spec: &Self) -> Target {
        Target::Global
    }

    /// Geltungsbereich beim Anlegen (Standard wie [`AdminResource::target`]).
    fn create_target(name: &str, spec: &Self) -> Target {
        Self::target(name, spec)
    }

    /// Geltungsbereich mit Datenbankzugriff (z. B. Organisation eines Teams, F183); Standard:
    /// [`AdminResource::target`].
    fn target_for<'a>(
        _st: &'a AdminState,
        name: &'a str,
        spec: &'a Self,
    ) -> impl Future<Output = Result<Target, Problem>> + Send + 'a {
        async move { Ok(Self::target(name, spec)) }
    }

    /// Geltungsbereich beim Anlegen mit Datenbankzugriff; Standard:
    /// [`AdminResource::create_target`] bzw. bei gleichem Ziel [`AdminResource::target_for`].
    fn create_target_for<'a>(
        st: &'a AdminState,
        name: &'a str,
        spec: &'a Self,
    ) -> impl Future<Output = Result<Target, Problem>> + Send + 'a {
        async move {
            let t = Self::create_target(name, spec);
            if t == Self::target(name, spec) {
                Self::target_for(st, name, spec).await
            } else {
                Ok(t)
            }
        }
    }
}

/// Lesbarer Bereich eines Principals für Listen (wie `Principal::can(Read, …)`): global, wenn
/// eine wirksame Zuweisung ohne Einschränkung besteht, sonst die eingeschränkten Zuweisungen.
pub(crate) fn read_scope(p: &Principal) -> AuditScope {
    let mut rules = Vec::new();
    for b in p.roles.iter().filter(|b| crate::rbac::grants_read(b)) {
        if b.organization.is_none() && b.team.is_none() {
            return AuditScope::All;
        }
        let rule = aigw_store::ScopeRule {
            organization: b.organization.clone(),
            team: b.team.clone(),
        };
        if !rules.contains(&rule) {
            rules.push(rule);
        }
    }
    AuditScope::Within(rules)
}

/// Katalog-Arten (Leserecht für jede wirksame Rolle).
fn is_catalog(kind: aigw_core::resource::ResourceKind) -> bool {
    use aigw_core::resource::ResourceKind as K;
    matches!(
        kind,
        K::ModelGroup
            | K::Deployment
            | K::Price
            | K::McpServer
            | K::ProviderCredential
            | K::PluginBinding
            | K::ToolPolicy
    )
}

pub(crate) async fn list_impl<R: AdminResource>(
    st: &AdminState,
    p: &Principal,
    q: &R::Query,
) -> Result<(Vec<R::Res>, Option<String>), Problem> {
    // Nur Objekte, die der Principal lesen darf (eingeschränkte Rollen sehen ihren Bereich);
    // der Bereich steht wie alle Filter im SQL (F42).
    let lq = list_query(&R::list_args(q), R::list_filters(q)?, read_scope(p))?;
    if is_catalog(R::KIND) && !p.can(Permission::Read, &Target::Catalog) {
        return Ok((Vec::new(), None));
    }
    Ok(page_parts(st.store.list_page::<R>(&lq).await?))
}

pub(crate) async fn get_impl<R: AdminResource>(
    st: &AdminState,
    p: &Principal,
    name: &str,
) -> ApiResult {
    let obj: Object<R> = st.store.get(name).await?;
    // Katalog-Arten (auch Plugin-Bindungen) liest jede wirksame Rolle – wie in der Liste.
    let target = if is_catalog(R::KIND) {
        Target::Catalog
    } else {
        R::target_for(st, &obj.metadata.name, &obj.spec).await?
    };
    p.require_on(Permission::Read, &target)?;
    let tag = etag_of(&obj);
    Ok(json_with(StatusCode::OK, Some(tag), &R::Res::from(obj)))
}

pub(crate) async fn create_impl<R: AdminResource>(
    st: &AdminState,
    p: &Principal,
    mut meta: ObjectMeta,
    spec: R,
    q: &BoundsQuery,
) -> ApiResult {
    if meta.name.is_empty()
        && let Some(name) = R::default_name(&spec)
    {
        meta.name = name;
    }
    p.require_on(
        Permission::Write,
        &R::create_target_for(st, &meta.name, &spec).await?,
    )?;
    check_models(&st.store, spec.allowed_models()).await?;
    let _guard = st.write_lock.lock().await;
    R::check(st, &meta.name, &spec, None).await?;
    let extras = R::bounds(st, p, &meta.name, &spec, None, q).await?;
    let obj = st.store.create_with(p.actor(), meta, spec, &extras).await?;
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
    q: &BoundsQuery,
) -> ApiResult {
    let meta = same_name(name, meta)?;
    // Neuer Zustand im Geltungsbereich (z. B. Team nicht in fremde Organisation verschieben).
    p.require_on(Permission::Write, &R::target_for(st, name, &spec).await?)?;
    check_models(&st.store, spec.allowed_models()).await?;
    let _guard = st.write_lock.lock().await;
    let current: Object<R> = st.store.get(name).await?;
    p.require_on(
        Permission::Write,
        &R::target_for(st, name, &current.spec).await?,
    )?;
    let expect = check_if_match(headers, &etag_of(&current), current.metadata.updated_at)?;
    R::check(st, name, &spec, Some(&current)).await?;
    let extras = R::bounds(st, p, name, &spec, Some(&current), q)
        .await?
        .expecting(expect);
    let obj = st.store.update_with(p.actor(), meta, spec, &extras).await?;
    let tag = etag_of(&obj);
    Ok(json_with(StatusCode::OK, Some(tag), &R::Res::from(obj)))
}

pub(crate) async fn delete_impl<R: AdminResource>(
    st: &AdminState,
    p: &Principal,
    name: &str,
    headers: &HeaderMap,
    q: &BoundsQuery,
) -> ApiResult {
    let _guard = st.write_lock.lock().await;
    let current: Object<R> = st.store.get(name).await?;
    // Verwaiste Objekte (Ziel gelöscht) darf ein globaler Admin entfernen.
    let target = R::target_for(st, name, &current.spec)
        .await
        .unwrap_or(Target::Global);
    p.require_on(Permission::Write, &target)?;
    let expect = check_if_match(headers, &etag_of(&current), current.metadata.updated_at)?;
    R::check_delete(st, &current).await?;
    let extras = R::delete_extras(st, &current, q).await?;
    st.store
        .delete_with(p.actor(), R::KIND, name, expect, &extras)
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Erzeugt Darstellung, Eingabe- und Listentyp sowie die fünf CRUD-Handler einer Art.
///
/// `res_extra = [#[doc = "…"] feld: Typ = funktion]` ergänzt die gelesene Darstellung um
/// abgeleitete Felder (`funktion(&Object<Spec>) -> Typ`, z. B. wirksame Standardwerte).
macro_rules! plain_resource {
    (
        spec = $spec:ident, res = $res:ident, input = $input:ident, list = $list:ident,
        query = $query:ident, tag = $tag:tt,
        path = $path:tt, item_path = $item:tt, what = $what:tt,
        handlers = [$list_fn:ident, $create_fn:ident, $get_fn:ident, $put_fn:ident, $delete_fn:ident]
        $(, input_meta = [$($iattr:meta),*])?
        $(, bounds = $bq:ty)?
        $(, delete_query = $dq:ty)?
        $(, res_extra = [$($(#[$fattr:meta])* $fname:ident : $fty:ty = $fexpr:path),* $(,)?])?
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
            $($(
                $(#[$fattr])*
                pub $fname: $fty,
            )*)?
        }

        impl From<Object<$spec>> for $res {
            fn from(o: Object<$spec>) -> Self {
                $($(let $fname = $fexpr(&o);)*)?
                Self {
                    id: o.id,
                    metadata: o.metadata,
                    spec: o.spec,
                    $($($fname,)*)?
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
            let Query(q) = q?;
            let (items, next_cursor) = list_impl::<$spec>(&st, &p, &q).await?;
            Ok(json_with(StatusCode::OK, None, &$list { items, next_cursor }))
        }

        #[doc = concat!("Legt an: ", $what, ".")]
        #[utoipa::path(
            post, path = $path, tag = $tag, request_body = $input, $(params($bq),)?
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
            $(bq: Result<Query<$bq>, QueryRejection>,)?
            body: Result<Json<$input>, JsonRejection>,
        ) -> ApiResult {
            #[allow(unused_variables)] // mit `bounds = …` überdeckt
            let q = $crate::bounds::BoundsQuery::default();
            $(let Query::<$bq>(q) = bq?;)?
            let Json(input) = body?;
            create_impl(&st, &p, input.metadata, input.spec, &q).await
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
            get_impl::<$spec>(&st, &p, &name).await
        }

        #[doc = concat!("Ersetzt Labels und Spezifikation: ", $what, ".")]
        #[utoipa::path(
            put, path = $item, tag = $tag, request_body = $input,
            params(
                ("name" = String, Path, description = "Name"),
                ("If-Match" = String, Header, description = "ETag aus dem letzten Lesen oder `*` (Pflicht, F38; fehlt er: 428)"),
                $($bq)?
            ),
            responses(
                (status = 200, description = "Geändert", body = $res,
                    headers(("ETag" = String, description = "Neue Version"))),
                (status = 400, description = "Ungültige Anfrage", body = Problem, content_type = "application/problem+json"),
                (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
                (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
                (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
                (status = 409, description = "Konflikt (z. B. bestehende Objekte über neuen Grenzen)", body = Problem, content_type = "application/problem+json"),
                (status = 412, description = "ETag passt nicht", body = Problem, content_type = "application/problem+json"),
                (status = 428, description = "If-Match fehlt (F38)", body = Problem, content_type = "application/problem+json"),
                (status = 422, description = "Ungültige Eingabe oder Verweis", body = Problem, content_type = "application/problem+json"),
            ),
            security(("bearer" = []))
        )]
        pub(crate) async fn $put_fn(
            State(st): St,
            Extension(p): Extension<Principal>,
            Path(name): Path<String>,
            headers: HeaderMap,
            $(bq: Result<Query<$bq>, QueryRejection>,)?
            body: Result<Json<$input>, JsonRejection>,
        ) -> ApiResult {
            #[allow(unused_variables)] // mit `bounds = …` überdeckt
            let q = $crate::bounds::BoundsQuery::default();
            $(let Query::<$bq>(q) = bq?;)?
            let Json(input) = body?;
            put_impl(&st, &p, &name, &headers, input.metadata, input.spec, &q).await
        }

        #[doc = concat!("Löscht: ", $what, ".")]
        #[utoipa::path(
            delete, path = $item, tag = $tag,
            params(
                ("name" = String, Path, description = "Name"),
                ("If-Match" = String, Header, description = "ETag aus dem letzten Lesen oder `*` (Pflicht, F38; fehlt er: 428)"),
                $($dq)?
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
        pub(crate) async fn $delete_fn(
            State(st): St,
            Extension(p): Extension<Principal>,
            Path(name): Path<String>,
            headers: HeaderMap,
            $(dq: Result<Query<$dq>, QueryRejection>,)?
        ) -> ApiResult {
            #[allow(unused_variables)] // mit `delete_query = …` überdeckt
            let q = $crate::bounds::BoundsQuery::default();
            $(let Query::<$dq>(q) = dq?;)?
            delete_impl::<$spec>(&st, &p, &name, &headers, &q).await
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
    ],
    bounds = BoundsQuery
);

impl AdminResource for OrganizationSpec {
    type Res = Organization;
    type Query = OrganizationQuery;
    const COLLECTION: &'static str = "organizations";

    fn target(name: &str, _spec: &Self) -> Target {
        Target::Organization(name.to_owned())
    }

    fn create_target(_name: &str, _spec: &Self) -> Target {
        Target::Global
    }

    async fn bounds(
        st: &AdminState,
        p: &Principal,
        name: &str,
        spec: &Self,
        current: Option<&Object<Self>>,
        q: &BoundsQuery,
    ) -> Result<WriteExtras, Problem> {
        bounds::organization(&st.store, p, name, spec, current.map(|c| &c.spec), q).await
    }

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
    handlers = [list_teams, create_team, get_team, put_team, delete_team],
    bounds = BoundsQuery
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

    fn list_filters(q: &Self::Query) -> Result<Vec<ListFilter>, Problem> {
        Ok(q.organization
            .iter()
            .map(|o| ListFilter::Organization(o.clone()))
            .collect())
    }

    fn allowed_models(&self) -> &[String] {
        &self.allowed_models
    }

    fn target(name: &str, spec: &Self) -> Target {
        Target::team(spec.organization.clone(), name)
    }

    async fn bounds(
        st: &AdminState,
        p: &Principal,
        name: &str,
        spec: &Self,
        current: Option<&Object<Self>>,
        q: &BoundsQuery,
    ) -> Result<WriteExtras, Problem> {
        bounds::team(&st.store, p, name, spec, current.map(|c| &c.spec), q).await
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

    fn list_filters(q: &Self::Query) -> Result<Vec<ListFilter>, Problem> {
        Ok(q.email
            .iter()
            .map(|e| ListFilter::Email(e.clone()))
            .collect())
    }

    async fn check(
        _st: &AdminState,
        _name: &str,
        spec: &Self,
        _current: Option<&Object<Self>>,
    ) -> Result<(), Problem> {
        // F90: OIDC-Identität (Issuer + Kennung) nur vollständig; F55: gitops nur global.
        validate::user(spec).map_err(Problem::invalid)
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
    ],
    bounds = BoundsQuery,
    delete_query = BoundsQuery
);

impl AdminResource for ModelGroupSpec {
    type Res = ModelGroup;
    type Query = ModelGroupQuery;
    const COLLECTION: &'static str = "model-groups";

    fn target(_name: &str, _spec: &Self) -> Target {
        Target::Catalog
    }

    fn list_args(q: &Self::Query) -> ListArgs<'_> {
        // `q` sucht auch in Aliassen (im Store über `model_alias`).
        ListArgs {
            limit: q.limit,
            cursor: q.cursor.as_deref(),
            q: q.q.as_deref(),
            label: q.label.as_deref(),
        }
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

    async fn bounds(
        st: &AdminState,
        _p: &Principal,
        name: &str,
        spec: &Self,
        current: Option<&Object<Self>>,
        q: &BoundsQuery,
    ) -> Result<WriteExtras, Problem> {
        let cascade = model_cascade(q)?;
        // F66: entfernte Aliasse dürfen keine Verweise zurücklassen.
        let Some(current) = current else {
            return Ok(WriteExtras::default());
        };
        let removed: BTreeSet<String> = group_names(&current.metadata.name, &current.spec)
            .difference(&group_names(name, spec))
            .cloned()
            .collect();
        model_refs(st, &removed, cascade).await
    }

    async fn delete_extras(
        st: &AdminState,
        current: &Object<Self>,
        q: &BoundsQuery,
    ) -> Result<WriteExtras, Problem> {
        let cascade = model_cascade(q)?;
        let names = group_names(&current.metadata.name, &current.spec);
        model_refs(st, &names, cascade).await
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

/// `?cascade=` bei Modellgruppen: nur `remove` (F66); `force`/`clamp` gibt es hier nicht.
fn model_cascade(q: &BoundsQuery) -> Result<bool, Problem> {
    if q.force.is_some() {
        return Err(Problem::bad_request(
            "force gibt es bei Modellgruppen nicht (nur ?cascade=remove)",
        ));
    }
    match q.cascade {
        None | Some(bounds::Cascade::Reject) => Ok(false),
        Some(bounds::Cascade::Remove) => Ok(true),
        Some(bounds::Cascade::Clamp) => Err(Problem::bad_request(
            "cascade=clamp gibt es bei Modellgruppen nicht (nur ?cascade=remove)",
        )),
    }
}

/// Alle Namen einer Modellgruppe (kanonischer Name und Aliasse).
fn group_names(name: &str, spec: &ModelGroupSpec) -> BTreeSet<String> {
    std::iter::once(name.to_owned())
        .chain(spec.aliases.iter().cloned())
        .collect()
}

/// Verweise auf die Modellnamen `names` (F66): `allowedModels` von Teams und Keys sowie
/// `limits[].modelGroup` von Organisationen, Teams und Keys. Ohne `cascade` → `409` mit
/// allen Verweisen (`violations`); mit `cascade` die Änderungen zum Entfernen (Audit
/// `cascade-remove`) – außer eine Freigabeliste würde dadurch leer (leer = **alle**
/// Modelle; das wäre eine Rechteausweitung), dann ebenfalls `409`.
pub(crate) async fn model_refs(
    st: &AdminState,
    names: &BTreeSet<String>,
    cascade: bool,
) -> Result<WriteExtras, Problem> {
    use aigw_core::validate::bounds::{Level, Violation};
    let mut extras = WriteExtras::default();
    if names.is_empty() {
        return Ok(extras);
    }
    let mut found = Vec::new();
    let mut widening = Vec::new();
    let hit = |m: &Option<String>| m.as_ref().is_some_and(|m| names.contains(m));
    let viol = |level: Level, name: &str, field: String, what: &str| Violation {
        level,
        name: name.to_owned(),
        field,
        message: format!("verweist auf Modellgruppe/Alias {what}"),
    };
    let limit_refs = |level: Level, name: &str, limits: &[aigw_core::resource::LimitSpec]| {
        limits
            .iter()
            .enumerate()
            .filter(|(_, l)| hit(&l.model_group))
            .map(|(i, l)| {
                viol(
                    level,
                    name,
                    format!("limits[{i}].modelGroup"),
                    &format!("{:?}", l.model_group.as_deref().unwrap_or_default()),
                )
            })
            .collect::<Vec<_>>()
    };
    for o in st.store.list::<OrganizationSpec>().await? {
        let v = limit_refs(Level::Organization, &o.metadata.name, &o.spec.limits);
        if v.is_empty() {
            continue;
        }
        found.extend(v);
        let mut spec = o.spec.clone();
        spec.limits.retain(|l| !hit(&l.model_group));
        extras.unref_organizations.push((o, spec));
    }
    for t in st.store.list::<TeamSpec>().await? {
        let mut v = limit_refs(Level::Team, &t.metadata.name, &t.spec.limits);
        let allowed: Vec<&String> = t
            .spec
            .allowed_models
            .iter()
            .filter(|m| names.contains(*m))
            .collect();
        if !allowed.is_empty() {
            v.push(viol(
                Level::Team,
                &t.metadata.name,
                "allowedModels".into(),
                &format!("{allowed:?}"),
            ));
        }
        if v.is_empty() {
            continue;
        }
        let mut spec = t.spec.clone();
        spec.limits.retain(|l| !hit(&l.model_group));
        spec.allowed_models.retain(|m| !names.contains(m));
        if !t.spec.allowed_models.is_empty() && spec.allowed_models.is_empty() {
            widening.push(viol(
                Level::Team,
                &t.metadata.name,
                "allowedModels".into(),
                "als einzige Freigabe – Entfernen gäbe alle Modelle frei",
            ));
        }
        found.extend(v);
        extras.unref_teams.push((t, spec));
    }
    for k in st.store.list_api_keys().await? {
        let mut v = limit_refs(Level::ApiKey, &k.metadata.name, &k.spec.limits);
        let allowed: Vec<&String> = k
            .spec
            .allowed_models
            .iter()
            .filter(|m| names.contains(*m))
            .collect();
        if !allowed.is_empty() {
            v.push(viol(
                Level::ApiKey,
                &k.metadata.name,
                "allowedModels".into(),
                &format!("{allowed:?}"),
            ));
        }
        if v.is_empty() {
            continue;
        }
        let mut spec = k.spec.clone();
        spec.limits.retain(|l| !hit(&l.model_group));
        spec.allowed_models.retain(|m| !names.contains(m));
        if !k.spec.allowed_models.is_empty() && spec.allowed_models.is_empty() {
            widening.push(viol(
                Level::ApiKey,
                &k.metadata.name,
                "allowedModels".into(),
                "als einzige Freigabe – Entfernen gäbe alle Modelle frei",
            ));
        }
        found.extend(v);
        extras.unref_api_keys.push((k, spec));
    }
    if found.is_empty() {
        return Ok(WriteExtras::default());
    }
    if !cascade {
        return Err(Problem::conflict(format!(
            "Modellgruppe/Alias wird noch verwendet ({} Verweis(e)) – anpassen oder \
             ?cascade=remove verwenden: {}",
            found.len(),
            aigw_core::validate::bounds::summary(&found)
        ))
        .with_violations(found));
    }
    if !widening.is_empty() {
        return Err(Problem::conflict(format!(
            "Verweise lassen sich nicht automatisch entfernen (leere Freigabeliste = alle \
             Modelle): {}",
            aigw_core::validate::bounds::summary(&widening)
        ))
        .with_violations(widening));
    }
    Ok(extras)
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

/// Geltungsbereich eines Keys: Objekt im Team (Organisation über das Team); Keys ohne Team
/// liegen direkt in ihrer Organisation (F193: `org-admin` verwaltet sie, `team-admin` nicht).
/// Unbekanntes Team -> `422`.
pub(crate) async fn key_target(store: &Store, spec: &ApiKeySpec) -> Result<Target, Problem> {
    match &spec.team {
        Some(t) => match store.get::<TeamSpec>(t).await {
            Ok(team) => Ok(Target::in_team(team.spec.organization, t.clone())),
            Err(aigw_store::StoreError::NotFound { .. }) => {
                Err(Problem::invalid(format!("team: unbekanntes Team {t:?}")))
            }
            Err(e) => Err(e.into()),
        },
        None => Ok(Target::InOrganization(spec.organization.clone())),
    }
}

/// F3: Organisation ist Pflicht und muss existieren; mit Team die Organisation des Teams
/// (`422`). Keys ohne Team erben Limits, Budgets und Plugins der Organisation.
async fn check_key_org(store: &Store, spec: &ApiKeySpec) -> Result<(), Problem> {
    let team_org = match &spec.team {
        Some(t) => match store.get::<TeamSpec>(t).await {
            Ok(team) => Some(team.spec.organization),
            Err(aigw_store::StoreError::NotFound { .. }) => None,
            Err(e) => return Err(e.into()),
        },
        None => None,
    };
    validate::api_key_organization(spec, team_org.as_deref()).map_err(Problem::invalid)?;
    match store.get::<OrganizationSpec>(&spec.organization).await {
        Ok(_) => Ok(()),
        Err(aigw_store::StoreError::NotFound { .. }) => Err(Problem::invalid(format!(
            "organization: unbekannte Organisation {:?}",
            spec.organization
        ))),
        Err(e) => Err(e.into()),
    }
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
    let Query(q) = q?;
    let mut filters = Vec::new();
    filters.extend(q.team.iter().map(|t| ListFilter::Team(t.clone())));
    filters.extend(q.user.iter().map(|u| ListFilter::User(u.clone())));
    filters.extend(
        q.organization
            .iter()
            .map(|o| ListFilter::Organization(o.clone())),
    );
    filters.extend(q.state.map(|s| ListFilter::State(s.as_str().to_owned())));
    let args = ListArgs {
        limit: q.limit,
        cursor: q.cursor.as_deref(),
        q: q.q.as_deref(),
        label: q.label.as_deref(),
    };
    let lq = list_query(&args, filters, read_scope(&p))?;
    let (items, next_cursor) = page_parts(st.store.list_api_keys_page(&lq).await?);
    Ok(json_with(
        StatusCode::OK,
        None,
        &ApiKeyList { items, next_cursor },
    ))
}

/// Legt einen API-Key an. Der Klartext (`secret`) steht **nur** in dieser Antwort.
#[utoipa::path(
    post, path = "/admin/api/v1/api-keys", tag = "api-keys", request_body = ApiKeyInput,
    params(KeyBoundsQuery),
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
    q: Result<Query<KeyBoundsQuery>, QueryRejection>,
    body: Result<Json<ApiKeyInput>, JsonRejection>,
) -> ApiResult {
    let Query(q) = q?;
    let Json(input) = body?;
    p.require_on(
        Permission::Write,
        &key_target(&st.store, &input.spec).await?,
    )?;
    check_key_org(&st.store, &input.spec).await?;
    check_models(&st.store, &input.spec.allowed_models).await?;
    let _guard = st.write_lock.lock().await;
    let extras = bounds::api_key(&st.store, &p, &input.metadata.name, &input.spec, &q).await?;
    let created = st
        .store
        .create_api_key_with(p.actor(), input.metadata, input.spec, &extras)
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
    let k = st.store.get_api_key(&name).await?;
    p.require_on(Permission::Read, &key_target(&st.store, &k.spec).await?)?;
    Ok(json_with(StatusCode::OK, Some(key_etag(&k)), &k))
}

/// Ersetzt Labels und Spezifikation eines API-Keys (Präfix, Hash und Zustand bleiben).
#[utoipa::path(
    put, path = "/admin/api/v1/api-keys/{name}", tag = "api-keys", request_body = ApiKeyInput,
    params(
        ("name" = String, Path, description = "Name"),
        ("If-Match" = String, Header, description = "ETag aus dem letzten Lesen oder `*` (Pflicht, F38; fehlt er: 428)"),
        KeyBoundsQuery,
    ),
    responses(
        (status = 200, description = "Geändert", body = ApiKey,
            headers(("ETag" = String, description = "Neue Version"))),
        (status = 400, description = "Ungültige Anfrage", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "ETag passt nicht", body = Problem, content_type = "application/problem+json"),
        (status = 428, description = "If-Match fehlt (F38)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Ungültige Eingabe oder Verweis", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn put_api_key(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
    headers: HeaderMap,
    q: Result<Query<KeyBoundsQuery>, QueryRejection>,
    body: Result<Json<ApiKeyInput>, JsonRejection>,
) -> ApiResult {
    let Query(q) = q?;
    let Json(input) = body?;
    let meta = same_name(&name, input.metadata)?;
    p.require_on(
        Permission::Write,
        &key_target(&st.store, &input.spec).await?,
    )?;
    check_key_org(&st.store, &input.spec).await?;
    check_models(&st.store, &input.spec.allowed_models).await?;
    let _guard = st.write_lock.lock().await;
    let current = st.store.get_api_key(&name).await?;
    p.require_on(
        Permission::Write,
        &key_target(&st.store, &current.spec).await?,
    )?;
    let expect = check_if_match(&headers, &key_etag(&current), current.metadata.updated_at)?;
    if current.status.state == ApiKeyState::Revoked {
        return Err(Problem::conflict(
            "Widerrufene Keys sind endgültig und lassen sich nicht mehr ändern",
        ));
    }
    let extras = bounds::api_key(&st.store, &p, &name, &input.spec, &q)
        .await?
        .expecting(expect);
    let k = st
        .store
        .update_api_key_with(p.actor(), meta, input.spec, &extras)
        .await?;
    Ok(json_with(StatusCode::OK, Some(key_etag(&k)), &k))
}

/// Löscht einen API-Key.
#[utoipa::path(
    delete, path = "/admin/api/v1/api-keys/{name}", tag = "api-keys",
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
    security(("bearer" = []))
)]
pub(crate) async fn delete_api_key(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> ApiResult {
    let _guard = st.write_lock.lock().await;
    let current = st.store.get_api_key(&name).await?;
    p.require_on(
        Permission::Write,
        &key_target(&st.store, &current.spec).await?,
    )?;
    let expect = check_if_match(&headers, &key_etag(&current), current.metadata.updated_at)?;
    st.store
        .delete_expect(
            p.actor(),
            aigw_core::resource::ResourceKind::ApiKey,
            &name,
            expect,
        )
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
    let _guard = st.write_lock.lock().await;
    let current = st.store.get_api_key(name).await?;
    p.require_on(
        Permission::Write,
        &key_target(&st.store, &current.spec).await?,
    )?;
    let expect = check_if_match(headers, &key_etag(&current), current.metadata.updated_at)?;
    // Widerrufen ist endgültig (F37); gleicher Zustand ist idempotent (keine neue Version).
    let k = st
        .store
        .set_api_key_state(p.actor(), name, state, expect)
        .await?;
    Ok(json_with(StatusCode::OK, Some(key_etag(&k)), &k))
}

/// Widerruft einen API-Key **endgültig** (F37; wirkt nach dem nächsten Snapshot-Neubau,
/// i. d. R. < 1 s). Ein widerrufener Key lässt sich weder fortsetzen noch ändern oder
/// rotieren, nur löschen.
#[utoipa::path(
    post, path = "/admin/api/v1/api-keys/{name}/revoke", tag = "api-keys",
    params(
        ("name" = String, Path, description = "Name"),
        ("If-Match" = String, Header, description = "ETag aus dem letzten Lesen oder `*` (Pflicht, F38; fehlt er: 428)"),
    ),
    responses(
        (status = 200, description = "Widerrufen", body = ApiKey,
            headers(("ETag" = String, description = "Neue Version"))),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "ETag passt nicht", body = Problem, content_type = "application/problem+json"),
        (status = 428, description = "If-Match fehlt (F38)", body = Problem, content_type = "application/problem+json"),
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

/// Sperrt einen API-Key vorübergehend (F37, umkehrbar mit `resume`).
#[utoipa::path(
    post, path = "/admin/api/v1/api-keys/{name}/suspend", tag = "api-keys",
    params(
        ("name" = String, Path, description = "Name"),
        ("If-Match" = String, Header, description = "ETag aus dem letzten Lesen oder `*` (Pflicht, F38; fehlt er: 428)"),
    ),
    responses(
        (status = 200, description = "Gesperrt", body = ApiKey,
            headers(("ETag" = String, description = "Neue Version"))),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Key ist widerrufen (endgültig)", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "ETag passt nicht", body = Problem, content_type = "application/problem+json"),
        (status = 428, description = "If-Match fehlt (F38)", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn suspend_api_key(
    State(st): St,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> ApiResult {
    set_state(&st, &p, &name, &headers, ApiKeyState::Suspended).await
}

/// Hebt die vorübergehende Sperre eines API-Keys auf (F37; widerrufene Keys: `409`).
#[utoipa::path(
    post, path = "/admin/api/v1/api-keys/{name}/resume", tag = "api-keys",
    params(
        ("name" = String, Path, description = "Name"),
        ("If-Match" = String, Header, description = "ETag aus dem letzten Lesen oder `*` (Pflicht, F38; fehlt er: 428)"),
    ),
    responses(
        (status = 200, description = "Wieder aktiv", body = ApiKey,
            headers(("ETag" = String, description = "Neue Version"))),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nicht gefunden", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Key ist widerrufen (endgültig)", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "ETag passt nicht", body = Problem, content_type = "application/problem+json"),
        (status = 428, description = "If-Match fehlt (F38)", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn resume_api_key(
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
        ("If-Match" = String, Header, description = "ETag aus dem letzten Lesen oder `*` (Pflicht, F38; fehlt er: 428)"),
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
        (status = 428, description = "If-Match fehlt (F38)", body = Problem, content_type = "application/problem+json"),
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
    p.require_on(
        Permission::Write,
        &key_target(&st.store, &current.spec).await?,
    )?;
    let expect = check_if_match(&headers, &key_etag(&current), current.metadata.updated_at)?;
    if current.status.state != ApiKeyState::Active {
        return Err(Problem::conflict(
            "Gesperrte oder widerrufene Keys können nicht rotiert werden",
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
        .rotate_api_key(p.actor(), &name, meta, old_expires, expect)
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
            "/admin/api/v1/api-keys/{name}/suspend",
            post(suspend_api_key),
        )
        .route("/admin/api/v1/api-keys/{name}/resume", post(resume_api_key))
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
    fn list_query_checks_limit_and_labels() {
        let args = |limit, cursor, label| ListArgs {
            limit: Some(limit),
            cursor,
            q: Some("x"),
            label,
        };
        let lq = list_query(
            &args(2, Some("b"), Some("env=prod,tier")),
            vec![],
            AuditScope::All,
        )
        .unwrap();
        assert_eq!(lq.limit, 2);
        assert_eq!(lq.after.as_deref(), Some("b"));
        assert_eq!(lq.search.as_deref(), Some("x"));
        assert_eq!(
            lq.labels,
            vec![
                ("env".to_owned(), Some("prod".to_owned())),
                ("tier".to_owned(), None)
            ]
        );
        assert!(list_query(&args(0, None, None), vec![], AuditScope::All).is_err());
        assert!(list_query(&args(501, None, None), vec![], AuditScope::All).is_err());
        assert!(list_query(&args(5, None, Some("=x")), vec![], AuditScope::All).is_err());
    }

    #[test]
    fn if_match_semantics() {
        let t = OffsetDateTime::from_unix_timestamp(1_763_000_000).unwrap();
        let mut h = HeaderMap::new();
        // F38: Pflicht
        assert_eq!(
            check_if_match(&h, "\"a\"", Some(t)).unwrap_err().status,
            428
        );
        h.insert(header::IF_MATCH, HeaderValue::from_static(" "));
        assert_eq!(
            check_if_match(&h, "\"a\"", Some(t)).unwrap_err().status,
            428
        );
        h.insert(header::IF_MATCH, HeaderValue::from_static("\"a\""));
        assert_eq!(
            check_if_match(&h, "\"a\"", Some(t)).unwrap(),
            Expect::Version(t)
        );
        assert_eq!(check_if_match(&h, "\"a\"", None).unwrap(), Expect::Any);
        assert_eq!(
            check_if_match(&h, "\"b\"", Some(t)).unwrap_err().status,
            412
        );
        h.insert(header::IF_MATCH, HeaderValue::from_static("*"));
        assert_eq!(check_if_match(&h, "\"b\"", Some(t)).unwrap(), Expect::Any);
        h.insert(header::IF_MATCH, HeaderValue::from_static("\"x\", \"b\""));
        assert_eq!(
            check_if_match(&h, "\"b\"", Some(t)).unwrap(),
            Expect::Version(t)
        );
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
