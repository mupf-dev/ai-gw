// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Repository: CRUD der Ressourcen.
//!
//! Jede schreibende Operation läuft in **einer** Transaktion, die als Erstes `config_version`
//! erhöht (nimmt bei SQLite sofort die Schreibsperre, vermeidet Lock-Upgrades), dann die
//! Änderung ausführt und sie im `audit_log` festhält. Verweise kommen als Namen herein und
//! werden in der Transaktion zu IDs aufgelöst.

// `Resource` ist öffentlich nutzbar (get/list/create …), aber nur in diesem Crate
// implementierbar: Spaltenabbildung (`Col`, `Row`, `RefCol`) bleibt intern.
#![allow(private_interfaces)]

mod api_key;
mod credential;
mod kinds;

pub(crate) use api_key::api_key_from_row;

use aigw_core::resource::{
    Id, Object, ObjectMeta, ResourceKind, ScopeLevel, new_id, validate_name,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::any::AnyRow;
use sqlx::{Any, AssertSqlSafe, Row as _, Transaction};
use time::OffsetDateTime;

use crate::{Store, StoreError};

/// Tabelle einer Ressourcenart.
pub(crate) fn table(kind: ResourceKind) -> &'static str {
    match kind {
        ResourceKind::Organization => "organization",
        ResourceKind::Team => "team",
        ResourceKind::User => "app_user",
        ResourceKind::ApiKey => "api_key",
        ResourceKind::ProviderCredential => "provider_credential",
        ResourceKind::ModelGroup => "model_group",
        ResourceKind::Deployment => "deployment",
        ResourceKind::Price => "price",
        ResourceKind::PluginBinding => "plugin_binding",
        ResourceKind::McpServer => "mcp_server",
        ResourceKind::McpGrant => "mcp_grant",
    }
}

/// Art des Ziels einer Ebene (für polymorphe Verweise).
pub(crate) fn level_kind(level: ScopeLevel) -> Option<ResourceKind> {
    match level {
        ScopeLevel::Global => None,
        ScopeLevel::Organization => Some(ResourceKind::Organization),
        ScopeLevel::Team => Some(ResourceKind::Team),
        ScopeLevel::ApiKey => Some(ResourceKind::ApiKey),
        ScopeLevel::ModelGroup => Some(ResourceKind::ModelGroup),
    }
}

/// Wert einer Spalte.
#[derive(Debug, Clone)]
pub(crate) enum Val {
    Text(Option<String>),
    Int(Option<i64>),
}

/// Spalte beim Schreiben: Wert oder Verweis (Name → ID wird aufgelöst).
#[derive(Debug, Clone)]
pub(crate) enum Col {
    V(Val),
    Ref(Option<(ResourceKind, String)>),
}

impl Col {
    pub(crate) fn text(s: impl Into<String>) -> Self {
        Self::V(Val::Text(Some(s.into())))
    }
    pub(crate) fn opt_text(s: Option<impl Into<String>>) -> Self {
        Self::V(Val::Text(s.map(Into::into)))
    }
    pub(crate) fn int(i: i64) -> Self {
        Self::V(Val::Int(Some(i)))
    }
    pub(crate) fn opt_int(i: Option<i64>) -> Self {
        Self::V(Val::Int(i))
    }
    pub(crate) fn bool(b: bool) -> Self {
        Self::int(i64::from(b))
    }
    pub(crate) fn json<T: Serialize>(v: &T) -> Result<Self, StoreError> {
        Ok(Self::text(serde_json::to_string(v)?))
    }
    pub(crate) fn opt_json<T: Serialize>(v: Option<&T>) -> Result<Self, StoreError> {
        Ok(Self::V(Val::Text(
            v.map(serde_json::to_string).transpose()?,
        )))
    }
    pub(crate) fn reference(kind: ResourceKind, name: Option<&str>) -> Self {
        Self::Ref(name.map(|n| (kind, n.to_owned())))
    }
}

/// Verweisspalte beim Lesen: woher der Name des Ziels kommt.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RefCol {
    /// Fester Zieltyp.
    Fixed(&'static str, ResourceKind),
    /// Zieltyp aus einer Ebenen-Spalte (`level`, `subject_level`).
    ByLevel(&'static str, &'static str),
}

/// Lesesicht auf eine Zeile mit typisierten Zugriffen.
pub(crate) struct Row<'r>(pub(crate) &'r AnyRow);

impl Row<'_> {
    pub(crate) fn opt_text(&self, col: &str) -> Result<Option<String>, StoreError> {
        Ok(self.0.try_get::<Option<String>, _>(col)?)
    }
    pub(crate) fn text(&self, col: &str) -> Result<String, StoreError> {
        self.opt_text(col)?
            .ok_or_else(|| StoreError::Corrupt(format!("Spalte {col} ist NULL")))
    }
    pub(crate) fn opt_int(&self, col: &str) -> Result<Option<i64>, StoreError> {
        Ok(self.0.try_get::<Option<i64>, _>(col)?)
    }
    pub(crate) fn int(&self, col: &str) -> Result<i64, StoreError> {
        self.opt_int(col)?
            .ok_or_else(|| StoreError::Corrupt(format!("Spalte {col} ist NULL")))
    }
    pub(crate) fn bool(&self, col: &str) -> Result<bool, StoreError> {
        Ok(self.int(col)? != 0)
    }
    pub(crate) fn u32(&self, col: &str) -> Result<u32, StoreError> {
        u32::try_from(self.int(col)?)
            .map_err(|_| StoreError::Corrupt(format!("Spalte {col} außerhalb u32")))
    }
    pub(crate) fn opt_u64(&self, col: &str) -> Result<Option<u64>, StoreError> {
        self.opt_int(col)?
            .map(|v| {
                u64::try_from(v).map_err(|_| StoreError::Corrupt(format!("Spalte {col} negativ")))
            })
            .transpose()
    }
    pub(crate) fn json<T: DeserializeOwned>(&self, col: &str) -> Result<T, StoreError> {
        Ok(serde_json::from_str(&self.text(col)?)?)
    }
    pub(crate) fn opt_json<T: DeserializeOwned>(&self, col: &str) -> Result<Option<T>, StoreError> {
        Ok(self
            .opt_text(col)?
            .map(|s| serde_json::from_str(&s))
            .transpose()?)
    }
    pub(crate) fn opt_time(&self, col: &str) -> Result<Option<OffsetDateTime>, StoreError> {
        self.opt_int(col)?.map(from_millis).transpose()
    }
    pub(crate) fn time(&self, col: &str) -> Result<OffsetDateTime, StoreError> {
        from_millis(self.int(col)?)
    }
    /// Name des Verweisziels (über die Unterabfrage `<col>__name`).
    pub(crate) fn ref_name(&self, col: &str) -> Result<Option<String>, StoreError> {
        self.opt_text(&format!("{col}__name"))
    }
    pub(crate) fn id(&self) -> Result<Id, StoreError> {
        parse_id(&self.text("id")?)
    }
    pub(crate) fn meta(&self) -> Result<ObjectMeta, StoreError> {
        Ok(ObjectMeta {
            name: self.text("name")?,
            labels: self.json("labels")?,
            created_at: Some(self.time("created_at")?),
            updated_at: Some(self.time("updated_at")?),
        })
    }
}

pub(crate) fn parse_id(s: &str) -> Result<Id, StoreError> {
    Id::parse_str(s).map_err(|_| StoreError::Corrupt(format!("ungültige ID {s:?}")))
}

pub(crate) fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

pub(crate) fn millis(t: OffsetDateTime) -> i64 {
    i64::try_from(t.unix_timestamp_nanos() / 1_000_000).unwrap_or(i64::MAX)
}

pub(crate) fn from_millis(ms: i64) -> Result<OffsetDateTime, StoreError> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .map_err(|_| StoreError::Corrupt(format!("ungültiger Zeitpunkt {ms}")))
}

/// Auf Millisekunden gekürzt (so wird gespeichert; hält Vergleiche nach dem Lesen stabil).
pub(crate) fn trunc_millis(t: OffsetDateTime) -> OffsetDateTime {
    from_millis(millis(t)).unwrap_or(t)
}

/// Eine per Repository verwaltete Ressourcenart (Spezifikation ↔ Spalten).
pub trait Resource: Serialize + DeserializeOwned + Clone + Send + Sync + Sized + 'static {
    /// Art.
    const KIND: ResourceKind;
    /// Verweisspalten (für die Namensauflösung beim Lesen).
    #[doc(hidden)]
    const REFS: &'static [RefCol] = &[];

    /// Spalten (ohne id, name, labels, created_at, updated_at).
    #[doc(hidden)]
    fn to_columns(&self) -> Result<Vec<(&'static str, Col)>, StoreError>;

    /// Spezifikation aus einer Zeile.
    #[doc(hidden)]
    fn from_row(row: &Row<'_>) -> Result<Self, StoreError>;

    /// Inhaltliche Prüfung vor dem Schreiben.
    fn validate(&self) -> Result<(), StoreError> {
        Ok(())
    }

    /// Namen, die zusätzlich im Alias-Namensraum belegt werden (nur Modellgruppen).
    #[doc(hidden)]
    fn aliases(&self) -> Option<&[String]> {
        None
    }
}

/// Ressourcen, die per generischem `create`/`update` geschrieben werden dürfen
/// (alle außer API-Keys, die Präfix/Hash brauchen; Zugangsdaten sind kein [`Resource`]).
pub trait PlainResource: Resource {}

/// Eine laufende Admin-Transaktion.
pub(crate) struct AdminTx {
    pub(crate) tx: Transaction<'static, Any>,
    pub(crate) version: i64,
    pub(crate) actor: String,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Action {
    Create,
    Update,
    Delete,
    Rotate,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
            Self::Delete => "delete",
            Self::Rotate => "rotate",
        }
    }
}

/// Wandelt Datenbankfehler in verständliche Konflikte um.
pub(crate) fn map_db(e: sqlx::Error, what: &str) -> StoreError {
    if let sqlx::Error::Database(db) = &e {
        if db.is_unique_violation() {
            return StoreError::Conflict(format!("{what}: Name oder Schlüssel bereits vergeben"));
        }
        if db.is_foreign_key_violation() {
            return StoreError::Conflict(format!(
                "{what}: wird noch von anderen Ressourcen verwendet oder Verweis ungültig"
            ));
        }
    }
    StoreError::Db(e)
}

macro_rules! bind_all {
    ($q:expr, $vals:expr) => {{
        let mut q = $q;
        for v in $vals {
            q = match v {
                Val::Text(t) => q.bind(t),
                Val::Int(i) => q.bind(i),
            };
        }
        q
    }};
}

impl AdminTx {
    /// Beginnt eine Transaktion und erhöht `config_version`.
    pub(crate) async fn begin(store: &Store, actor: &str) -> Result<Self, StoreError> {
        let mut tx = store.pool.begin().await?;
        let version: i64 = sqlx::query_scalar(
            "UPDATE config_version SET version = version + 1, updated_at = $1 WHERE id = 1 RETURNING version",
        )
        .bind(millis(now()))
        .fetch_one(&mut *tx)
        .await?;
        Ok(Self {
            tx,
            version,
            actor: actor.to_owned(),
        })
    }

    pub(crate) async fn commit(self) -> Result<i64, StoreError> {
        self.tx.commit().await?;
        Ok(self.version)
    }

    /// Schreibt einen Audit-Eintrag.
    pub(crate) async fn audit<B: Serialize, A: Serialize>(
        &mut self,
        action: Action,
        kind: ResourceKind,
        id: Option<Id>,
        name: &str,
        before: Option<&B>,
        after: Option<&A>,
    ) -> Result<(), StoreError> {
        let before = before.map(serde_json::to_string).transpose()?;
        let after = after.map(serde_json::to_string).transpose()?;
        sqlx::query(
            "INSERT INTO audit_log (id, at, actor, action, kind, object_id, object_name, config_version, before_json, after_json)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(new_id().to_string())
        .bind(millis(now()))
        .bind(self.actor.clone())
        .bind(action.as_str())
        .bind(kind.as_str())
        .bind(id.map(|i| i.to_string()))
        .bind(name.to_owned())
        .bind(self.version)
        .bind(before)
        .bind(after)
        .execute(&mut *self.tx)
        .await?;
        Ok(())
    }

    /// ID einer Ressource über ihren Namen.
    pub(crate) async fn resolve(
        &mut self,
        kind: ResourceKind,
        name: &str,
    ) -> Result<Id, StoreError> {
        let sql = format!("SELECT id FROM {} WHERE name = $1", table(kind));
        let id: Option<String> = sqlx::query_scalar(AssertSqlSafe(sql))
            .bind(name.to_owned())
            .fetch_optional(&mut *self.tx)
            .await?;
        match id {
            Some(id) => parse_id(&id),
            None => Err(StoreError::InvalidReference {
                kind,
                name: name.to_owned(),
            }),
        }
    }

    /// Löst Verweise auf und liefert (Spalte, Wert)-Paare.
    pub(crate) async fn resolve_cols(
        &mut self,
        cols: Vec<(&'static str, Col)>,
    ) -> Result<Vec<(&'static str, Val)>, StoreError> {
        let mut out = Vec::with_capacity(cols.len());
        for (name, col) in cols {
            let v = match col {
                Col::V(v) => v,
                Col::Ref(None) => Val::Text(None),
                Col::Ref(Some((kind, target))) => {
                    Val::Text(Some(self.resolve(kind, &target).await?.to_string()))
                }
            };
            out.push((name, v));
        }
        Ok(out)
    }

    /// Fügt eine Zeile ein.
    pub(crate) async fn insert_row(
        &mut self,
        kind: ResourceKind,
        id: Id,
        meta: &ObjectMeta,
        cols: Vec<(&'static str, Val)>,
    ) -> Result<(), StoreError> {
        let ts = millis(now());
        let mut names = vec!["id", "name", "labels", "created_at", "updated_at"];
        let mut vals = vec![
            Val::Text(Some(id.to_string())),
            Val::Text(Some(meta.name.clone())),
            Val::Text(Some(serde_json::to_string(&meta.labels)?)),
            Val::Int(Some(ts)),
            Val::Int(Some(ts)),
        ];
        for (n, v) in cols {
            names.push(n);
            vals.push(v);
        }
        let placeholders: Vec<String> = (1..=names.len()).map(|i| format!("${i}")).collect();
        let sql = format!(
            "INSERT INTO {} ({}) VALUES ({})",
            table(kind),
            names.join(", "),
            placeholders.join(", ")
        );
        bind_all!(sqlx::query(AssertSqlSafe(sql)), vals)
            .execute(&mut *self.tx)
            .await
            .map_err(|e| map_db(e, &format!("{kind} {:?}", meta.name)))?;
        Ok(())
    }

    /// Aktualisiert eine Zeile (Labels + übergebene Spalten).
    pub(crate) async fn update_row(
        &mut self,
        kind: ResourceKind,
        id: Id,
        meta: &ObjectMeta,
        cols: Vec<(&'static str, Val)>,
    ) -> Result<(), StoreError> {
        let mut sets = vec!["labels = $1".to_owned(), "updated_at = $2".to_owned()];
        let mut vals = vec![
            Val::Text(Some(serde_json::to_string(&meta.labels)?)),
            Val::Int(Some(millis(now()))),
        ];
        for (n, v) in cols {
            vals.push(v);
            sets.push(format!("{n} = ${}", vals.len()));
        }
        vals.push(Val::Text(Some(id.to_string())));
        let sql = format!(
            "UPDATE {} SET {} WHERE id = ${}",
            table(kind),
            sets.join(", "),
            vals.len()
        );
        bind_all!(sqlx::query(AssertSqlSafe(sql)), vals)
            .execute(&mut *self.tx)
            .await
            .map_err(|e| map_db(e, &format!("{kind} {:?}", meta.name)))?;
        Ok(())
    }

    /// Pflegt den Alias-Namensraum einer Modellgruppe.
    pub(crate) async fn write_aliases(
        &mut self,
        id: Id,
        name: &str,
        aliases: &[String],
    ) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM model_alias WHERE model_group_id = $1")
            .bind(id.to_string())
            .execute(&mut *self.tx)
            .await?;
        for alias in std::iter::once(name).chain(aliases.iter().map(String::as_str)) {
            sqlx::query("INSERT INTO model_alias (alias, model_group_id) VALUES ($1, $2)")
                .bind(alias.to_owned())
                .bind(id.to_string())
                .execute(&mut *self.tx)
                .await
                .map_err(|e| map_db(e, &format!("Modellname/Alias {alias:?}")))?;
        }
        Ok(())
    }

    /// Löscht eine Zeile samt polymorpher Bindungen, die auf sie zeigen.
    pub(crate) async fn delete_row(
        &mut self,
        kind: ResourceKind,
        id: Id,
    ) -> Result<(), StoreError> {
        let id_s = id.to_string();
        let level = match kind {
            ResourceKind::Organization => Some(ScopeLevel::Organization),
            ResourceKind::Team => Some(ScopeLevel::Team),
            ResourceKind::ApiKey => Some(ScopeLevel::ApiKey),
            ResourceKind::ModelGroup => Some(ScopeLevel::ModelGroup),
            _ => None,
        };
        if let Some(level) = level {
            sqlx::query("DELETE FROM plugin_binding WHERE level = $1 AND target_id = $2")
                .bind(level.as_str())
                .bind(id_s.clone())
                .execute(&mut *self.tx)
                .await?;
            sqlx::query("DELETE FROM mcp_grant WHERE subject_level = $1 AND subject_id = $2")
                .bind(level.as_str())
                .bind(id_s.clone())
                .execute(&mut *self.tx)
                .await?;
        }
        let sql = format!("DELETE FROM {} WHERE id = $1", table(kind));
        sqlx::query(AssertSqlSafe(sql))
            .bind(id_s)
            .execute(&mut *self.tx)
            .await
            .map_err(|e| map_db(e, &kind.to_string()))?;
        Ok(())
    }
}

/// SELECT-Liste mit Namensauflösung der Verweise.
pub(crate) fn select_sql(kind: ResourceKind, refs: &[RefCol], filter: &str) -> String {
    let mut cols = vec!["t.*".to_owned()];
    for r in refs {
        match *r {
            RefCol::Fixed(col, target) => cols.push(format!(
                "(SELECT r.name FROM {} r WHERE r.id = t.{col}) AS {col}__name",
                table(target)
            )),
            RefCol::ByLevel(col, level_col) => {
                let mut case = format!("CASE t.{level_col}");
                for level in [
                    ScopeLevel::Organization,
                    ScopeLevel::Team,
                    ScopeLevel::ApiKey,
                    ScopeLevel::ModelGroup,
                ] {
                    if let Some(k) = level_kind(level) {
                        case.push_str(&format!(
                            " WHEN '{}' THEN (SELECT r.name FROM {} r WHERE r.id = t.{col})",
                            level.as_str(),
                            table(k)
                        ));
                    }
                }
                case.push_str(&format!(" END AS {col}__name"));
                cols.push(case);
            }
        }
    }
    format!("SELECT {} FROM {} t {filter}", cols.join(", "), table(kind))
}

impl Store {
    /// Aktuelle Konfigurationsversion.
    pub async fn config_version(&self) -> Result<i64, StoreError> {
        Ok(
            sqlx::query_scalar("SELECT version FROM config_version WHERE id = 1")
                .fetch_one(&self.pool)
                .await?,
        )
    }

    pub(crate) async fn fetch_rows(
        &self,
        kind: ResourceKind,
        refs: &[RefCol],
        filter: &str,
        arg: Option<String>,
    ) -> Result<Vec<AnyRow>, StoreError> {
        let sql = select_sql(kind, refs, filter);
        let mut q = sqlx::query(AssertSqlSafe(sql));
        if let Some(a) = arg {
            q = q.bind(a);
        }
        Ok(q.fetch_all(&self.pool).await?)
    }

    pub(crate) async fn fetch_one_row(
        &self,
        kind: ResourceKind,
        refs: &[RefCol],
        name: &str,
    ) -> Result<AnyRow, StoreError> {
        self.fetch_rows(kind, refs, "WHERE t.name = $1", Some(name.to_owned()))
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| StoreError::NotFound {
                kind,
                name: name.to_owned(),
            })
    }

    fn object_from_row<R: Resource>(row: &AnyRow) -> Result<Object<R>, StoreError> {
        let r = Row(row);
        Ok(Object {
            id: r.id()?,
            metadata: r.meta()?,
            spec: R::from_row(&r)?,
        })
    }

    /// Liest eine Ressource über ihren Namen.
    pub async fn get<R: Resource>(&self, name: &str) -> Result<Object<R>, StoreError> {
        let row = self.fetch_one_row(R::KIND, R::REFS, name).await?;
        Self::object_from_row(&row)
    }

    /// Liest eine Ressource über ihre ID.
    pub async fn get_by_id<R: Resource>(&self, id: Id) -> Result<Object<R>, StoreError> {
        self.fetch_rows(R::KIND, R::REFS, "WHERE t.id = $1", Some(id.to_string()))
            .await?
            .first()
            .map(Self::object_from_row)
            .transpose()?
            .ok_or_else(|| StoreError::NotFound {
                kind: R::KIND,
                name: id.to_string(),
            })
    }

    /// Listet alle Ressourcen einer Art (nach Name sortiert).
    pub async fn list<R: Resource>(&self) -> Result<Vec<Object<R>>, StoreError> {
        self.fetch_rows(R::KIND, R::REFS, "ORDER BY t.name", None)
            .await?
            .iter()
            .map(Self::object_from_row)
            .collect()
    }

    /// Legt eine Ressource an.
    pub async fn create<R: PlainResource>(
        &self,
        actor: &str,
        meta: ObjectMeta,
        spec: R,
    ) -> Result<Object<R>, StoreError> {
        validate_name(&meta.name).map_err(StoreError::Invalid)?;
        spec.validate()?;
        let id = new_id();
        let mut tx = AdminTx::begin(self, actor).await?;
        let cols = tx.resolve_cols(spec.to_columns()?).await?;
        tx.insert_row(R::KIND, id, &meta, cols).await?;
        if let Some(aliases) = spec.aliases() {
            tx.write_aliases(id, &meta.name, aliases).await?;
        }
        let created = Object {
            id,
            metadata: meta,
            spec,
        };
        tx.audit(
            Action::Create,
            R::KIND,
            Some(id),
            &created.metadata.name,
            None::<&()>,
            Some(&created),
        )
        .await?;
        tx.commit().await?;
        self.get_by_id(id).await
    }

    /// Ersetzt Spezifikation und Labels einer Ressource (über `meta.name`).
    pub async fn update<R: PlainResource>(
        &self,
        actor: &str,
        meta: ObjectMeta,
        spec: R,
    ) -> Result<Object<R>, StoreError> {
        spec.validate()?;
        let before: Object<R> = self.get(&meta.name).await?;
        let mut tx = AdminTx::begin(self, actor).await?;
        let cols = tx.resolve_cols(spec.to_columns()?).await?;
        tx.update_row(R::KIND, before.id, &meta, cols).await?;
        if let Some(aliases) = spec.aliases() {
            tx.write_aliases(before.id, &meta.name, aliases).await?;
        }
        let after = Object {
            id: before.id,
            metadata: meta,
            spec,
        };
        tx.audit(
            Action::Update,
            R::KIND,
            Some(before.id),
            &after.metadata.name,
            Some(&before),
            Some(&after),
        )
        .await?;
        tx.commit().await?;
        self.get_by_id(before.id).await
    }

    /// Löscht eine Ressource beliebiger Art über ihren Namen.
    ///
    /// Plugin-Bindungen und MCP-Freigaben, die auf das Objekt zeigen, werden mitgelöscht;
    /// andere Verweise (z. B. Deployments einer Modellgruppe) verhindern das Löschen.
    pub async fn delete(
        &self,
        actor: &str,
        kind: ResourceKind,
        name: &str,
    ) -> Result<(), StoreError> {
        let before = self.get_json(kind, name).await?;
        let id = before
            .get("id")
            .and_then(|v| v.as_str())
            .map(parse_id)
            .transpose()?
            .ok_or_else(|| StoreError::Corrupt("Objekt ohne ID".into()))?;
        let mut tx = AdminTx::begin(self, actor).await?;
        tx.delete_row(kind, id).await?;
        tx.audit(
            Action::Delete,
            kind,
            Some(id),
            name,
            Some(&before),
            None::<&()>,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Liest eine Ressource beliebiger Art als JSON (wie die Admin-API sie zeigt).
    pub async fn get_json(
        &self,
        kind: ResourceKind,
        name: &str,
    ) -> Result<serde_json::Value, StoreError> {
        use aigw_core::resource as r;
        let v = match kind {
            ResourceKind::Organization => {
                serde_json::to_value(self.get::<r::OrganizationSpec>(name).await?)
            }
            ResourceKind::Team => serde_json::to_value(self.get::<r::TeamSpec>(name).await?),
            ResourceKind::User => serde_json::to_value(self.get::<r::UserSpec>(name).await?),
            ResourceKind::ApiKey => serde_json::to_value(self.get_api_key(name).await?),
            ResourceKind::ProviderCredential => {
                serde_json::to_value(self.get_credential(name).await?)
            }
            ResourceKind::ModelGroup => {
                serde_json::to_value(self.get::<r::ModelGroupSpec>(name).await?)
            }
            ResourceKind::Deployment => {
                serde_json::to_value(self.get::<r::DeploymentSpec>(name).await?)
            }
            ResourceKind::Price => serde_json::to_value(self.get::<r::PriceSpec>(name).await?),
            ResourceKind::PluginBinding => {
                serde_json::to_value(self.get::<r::PluginBindingSpec>(name).await?)
            }
            ResourceKind::McpServer => {
                serde_json::to_value(self.get::<r::McpServerSpec>(name).await?)
            }
            ResourceKind::McpGrant => {
                serde_json::to_value(self.get::<r::McpGrantSpec>(name).await?)
            }
        };
        Ok(v?)
    }

    /// Letzte Einträge des Audit-Logs (neueste zuerst).
    pub async fn audit_log(&self, limit: i64) -> Result<Vec<AuditEntry>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, at, actor, action, kind, object_id, object_name, config_version, before_json, after_json
             FROM audit_log ORDER BY at DESC, id DESC LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                let r = Row(row);
                Ok(AuditEntry {
                    id: parse_id(&r.text("id")?)?,
                    at: r.time("at")?,
                    actor: r.text("actor")?,
                    action: r.text("action")?,
                    kind: r.text("kind")?,
                    object_id: r.opt_text("object_id")?.map(|s| parse_id(&s)).transpose()?,
                    object_name: r.opt_text("object_name")?,
                    config_version: r.int("config_version")?,
                    before: r.opt_json("before_json")?,
                    after: r.opt_json("after_json")?,
                })
            })
            .collect()
    }
}

/// Eintrag im Audit-Log.
#[derive(Debug, Clone, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AuditEntry {
    /// ID.
    #[schema(value_type = String, format = Uuid)]
    pub id: Id,
    /// Zeitpunkt.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub at: OffsetDateTime,
    /// Wer (Nutzer, Admin-Key, `system`, `gitops`).
    pub actor: String,
    /// `create` | `update` | `delete` | `rotate`.
    pub action: String,
    /// Ressourcenart.
    pub kind: String,
    /// Objekt-ID.
    #[schema(value_type = Option<String>, format = Uuid)]
    pub object_id: Option<Id>,
    /// Objektname.
    pub object_name: Option<String>,
    /// Konfigurationsversion nach der Änderung.
    pub config_version: i64,
    /// Zustand vorher.
    #[schema(value_type = Option<Object>)]
    pub before: Option<serde_json::Value>,
    /// Zustand nachher.
    #[schema(value_type = Option<Object>)]
    pub after: Option<serde_json::Value>,
}
