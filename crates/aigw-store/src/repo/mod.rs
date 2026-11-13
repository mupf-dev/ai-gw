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
pub mod audit;
pub mod audit_chain;
pub(crate) mod credential;
mod extras;
mod kinds;
pub mod list;
pub(crate) mod secret;

pub use extras::WriteExtras;

pub(crate) use api_key::api_key_from_row;
pub use audit::AuditEntry;

use aigw_core::resource::{
    Id, Object, ObjectMeta, ResourceKind, ScopeLevel, new_id, validate_name,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use time::OffsetDateTime;

pub(crate) use crate::db::Val;
use crate::db::{DbRow, Q, Tx};
use crate::{Store, StoreError};

/// Tabelle einer Ressourcenart.
pub(crate) fn table(kind: ResourceKind) -> &'static str {
    match kind {
        ResourceKind::Organization => "organization",
        ResourceKind::Team => "team",
        ResourceKind::User => "app_user",
        ResourceKind::ApiKey => "api_key",
        ResourceKind::ProviderCredential => "provider_credential",
        ResourceKind::Secret => "secret",
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
        Self::V(Val::Bool(Some(b)))
    }
    pub(crate) fn opt_bool(b: Option<bool>) -> Self {
        Self::V(Val::Bool(b))
    }
    pub(crate) fn opt_time(t: Option<OffsetDateTime>) -> Self {
        Self::V(Val::opt_time(t))
    }
    pub(crate) fn time(t: OffsetDateTime) -> Self {
        Self::V(Val::time(t))
    }
    pub(crate) fn json<T: Serialize>(v: &T) -> Result<Self, StoreError> {
        Ok(Self::V(Val::json(v)?))
    }
    pub(crate) fn opt_json<T: Serialize>(v: Option<&T>) -> Result<Self, StoreError> {
        Ok(Self::V(Val::Json(v.map(serde_json::to_value).transpose()?)))
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
pub(crate) struct Row<'r>(pub(crate) &'r DbRow);

impl Row<'_> {
    pub(crate) fn opt_text(&self, col: &str) -> Result<Option<String>, StoreError> {
        self.0.opt_text(col)
    }
    pub(crate) fn text(&self, col: &str) -> Result<String, StoreError> {
        self.0.text(col)
    }
    pub(crate) fn opt_int(&self, col: &str) -> Result<Option<i64>, StoreError> {
        self.0.opt_i64(col)
    }
    pub(crate) fn int(&self, col: &str) -> Result<i64, StoreError> {
        self.0.i64(col)
    }
    pub(crate) fn bool(&self, col: &str) -> Result<bool, StoreError> {
        self.0.bool(col)
    }
    pub(crate) fn opt_bool(&self, col: &str) -> Result<Option<bool>, StoreError> {
        self.0.opt_bool(col)
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
        self.0.json(col)
    }
    pub(crate) fn opt_json<T: DeserializeOwned>(&self, col: &str) -> Result<Option<T>, StoreError> {
        self.0.opt_json(col)
    }
    pub(crate) fn opt_time(&self, col: &str) -> Result<Option<OffsetDateTime>, StoreError> {
        self.0.opt_time(col)
    }
    pub(crate) fn time(&self, col: &str) -> Result<OffsetDateTime, StoreError> {
        self.0.time(col)
    }
    pub(crate) fn uuid(&self, col: &str) -> Result<Id, StoreError> {
        self.0.uuid(col)
    }
    pub(crate) fn opt_uuid(&self, col: &str) -> Result<Option<Id>, StoreError> {
        self.0.opt_uuid(col)
    }
    /// Name des Verweisziels (über die Unterabfrage `<col>__name`).
    pub(crate) fn ref_name(&self, col: &str) -> Result<Option<String>, StoreError> {
        self.opt_text(&format!("{col}__name"))
    }
    pub(crate) fn id(&self) -> Result<Id, StoreError> {
        self.uuid("id")
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
    /// Weitere Textspalten, in denen die Listensuche (`?q=`) neben dem Namen sucht (F42).
    #[doc(hidden)]
    const SEARCH: &'static [&'static str] = &[];

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

/// Erwarteter Stand eines Objekts beim Ändern/Löschen (optimistische Sperre, F38).
///
/// Die Admin-API prüft `If-Match` gegen den ETag des gelesenen Objekts und übergibt dessen
/// `updatedAt` als [`Expect::Version`]. Der Store vergleicht ihn **im** `UPDATE`/`DELETE`
/// (`WHERE id = … AND updated_at = …`) – damit gewinnt auch zwischen mehreren Instanzen genau
/// eine Änderung; die andere scheitert mit [`StoreError::PreconditionFailed`]. `updated_at`
/// steigt dafür je Objekt streng monoton (mindestens +1 ms je Änderung).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Expect {
    /// Ohne Prüfung (`If-Match: *`, interne Schreibvorgänge, GitOps mit eigener Versionsprüfung).
    #[default]
    Any,
    /// Nur, wenn `updated_at` noch diesen Wert hat.
    Version(OffsetDateTime),
}

impl Expect {
    pub(crate) fn version(self) -> Option<OffsetDateTime> {
        match self {
            Self::Any => None,
            Self::Version(t) => Some(trunc_millis(t)),
        }
    }
}

/// Ressourcen, die per generischem `create`/`update` geschrieben werden dürfen
/// (alle außer API-Keys, die Präfix/Hash brauchen; Zugangsdaten sind kein [`Resource`]).
pub trait PlainResource: Resource {}

/// Eine laufende Admin-Transaktion.
pub(crate) struct AdminTx {
    pub(crate) tx: Tx,
    pub(crate) version: i64,
    pub(crate) actor: String,
    /// Für die Änderungsbenachrichtigung nach dem Commit (§14).
    store: Store,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Action {
    Create,
    Update,
    Delete,
    Rotate,
    /// API-Key widerrufen (endgültig, F37).
    Revoke,
    /// Auf neue Grenzen gekürzt (AIGW-67, `?cascade=clamp`).
    Clamp,
    /// Grenzen bewusst überschritten (AIGW-67, `?force=true`).
    Force,
    /// Verweis auf gelöschte Modellgruppe/entfernten Alias entfernt (F66, `?cascade=remove`).
    CascadeRemove,
    /// API-Key vorübergehend gesperrt (F37).
    Suspend,
    /// Gesperrten API-Key wieder aktiviert (F37).
    Resume,
}

impl Action {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
            Self::Delete => "delete",
            Self::Rotate => "rotate",
            Self::Revoke => "revoke",
            Self::Clamp => "clamp",
            Self::Force => "force",
            Self::CascadeRemove => "cascade-remove",
            Self::Suspend => "suspend",
            Self::Resume => "resume",
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

impl AdminTx {
    /// Beginnt eine Transaktion und erhöht `config_version`.
    pub(crate) async fn begin(store: &Store, actor: &str) -> Result<Self, StoreError> {
        let mut tx = store.pool.begin().await?;
        let version = Q::new(
            "UPDATE config_version SET version = version + 1, updated_at = $1 WHERE id = 1 RETURNING version",
        )
        .bind(Val::time(now()))
        .fetch_i64(&mut tx)
        .await?;
        Ok(Self {
            tx,
            version,
            actor: actor.to_owned(),
            store: store.clone(),
        })
    }

    /// Schließt die Transaktion ab und meldet die neue Version (§14).
    ///
    /// Postgres: `NOTIFY` in derselben Transaktion – die Datenbank stellt sie erst nach dem
    /// Commit (und nur bei Erfolg) allen lauschenden Instanzen zu. Zusätzlich wird der Reloader
    /// im eigenen Prozess direkt benachrichtigt (bei SQLite der einzige Weg).
    pub(crate) async fn commit(mut self) -> Result<i64, StoreError> {
        if self.store.backend() == crate::Backend::Postgres {
            // Die Version ist eine Zahl aus der Datenbank – kein Einschleusen möglich.
            Q::new(format!(
                "NOTIFY {}, '{}'",
                crate::reload::NOTIFY_CHANNEL,
                self.version
            ))
            .execute(&mut self.tx)
            .await?;
        }
        self.tx.commit().await?;
        self.store.notify_committed(self.version);
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
        // F115: Arten im Audit-Log einheitlich kebab-case (`api-key`, `model-group` …).
        self.audit_kind(action, kind.audit_name(), id, name, before, after)
            .await
    }

    /// Schreibt einen Audit-Eintrag für eine Art außerhalb von [`ResourceKind`]
    /// (z. B. `admin-api-key`).
    pub(crate) async fn audit_kind<B: Serialize, A: Serialize>(
        &mut self,
        action: Action,
        kind: &str,
        id: Option<Id>,
        name: &str,
        before: Option<&B>,
        after: Option<&A>,
    ) -> Result<(), StoreError> {
        let before = before.map(serde_json::to_value).transpose()?;
        let after = after.map(serde_json::to_value).transpose()?;
        // Geltungsbereich vorher/nachher (für org-/team-admins, AIGW-66).
        let scope_before = match &before {
            Some(v) => self.scope_of(kind, name, v).await?,
            None => audit::Scope::default(),
        };
        let scope_after = match &after {
            Some(v) => self.scope_of(kind, name, v).await?,
            None => scope_before.clone(),
        };
        // F116: über die Hash-Kette anhängen.
        let mut row =
            audit_chain::NewAuditRow::new(&self.actor, action.as_str(), kind, self.version);
        row.object_id = id;
        row.object_name = Some(name.to_owned());
        row.before = before;
        row.after = after;
        row.organization = scope_after.organization;
        row.team = scope_after.team;
        row.organization_before = scope_before.organization;
        row.team_before = scope_before.team;
        audit_chain::append(&mut self.tx, &row).await
    }

    /// ID einer Ressource über ihren Namen.
    pub(crate) async fn resolve(
        &mut self,
        kind: ResourceKind,
        name: &str,
    ) -> Result<Id, StoreError> {
        let sql = format!("SELECT id FROM {} WHERE name = $1", table(kind));
        let row = Q::new(sql)
            .bind(Val::text(name))
            .fetch_optional(&mut self.tx)
            .await?;
        match row {
            Some(r) => r.uuid("id"),
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
                Col::Ref(None) => Val::Uuid(None),
                Col::Ref(Some((kind, target))) => Val::uuid(self.resolve(kind, &target).await?),
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
        let ts = now();
        let mut names = vec!["id", "name", "labels", "created_at", "updated_at"];
        let mut vals = vec![
            Val::uuid(id),
            Val::text(meta.name.clone()),
            Val::json(&meta.labels)?,
            Val::time(ts),
            Val::time(ts),
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
        Q { sql, args: vals }
            .execute(&mut self.tx)
            .await
            .map_err(|e| map_db(e, &format!("{kind} {:?}", meta.name)))?;
        Ok(())
    }

    /// Aktualisiert eine Zeile (Labels + übergebene Spalten), ohne Versionsprüfung.
    pub(crate) async fn update_row(
        &mut self,
        kind: ResourceKind,
        id: Id,
        meta: &ObjectMeta,
        cols: Vec<(&'static str, Val)>,
    ) -> Result<(), StoreError> {
        self.update_row_expect(kind, id, meta, cols, Expect::Any)
            .await
    }

    /// `updated_at` einer Zeile innerhalb der Transaktion (für Versionsprüfung und
    /// monotonen Zeitstempel).
    pub(crate) async fn row_version(
        &mut self,
        table_name: &str,
        id: Id,
    ) -> Result<Option<OffsetDateTime>, StoreError> {
        let sql = format!("SELECT updated_at FROM {table_name} WHERE id = $1");
        Q::new(sql)
            .bind(Val::uuid(id))
            .fetch_optional(&mut self.tx)
            .await?
            .map(|r| r.time("updated_at"))
            .transpose()
    }

    /// Neuer Zeitstempel einer Zeile: jetzt, mindestens aber 1 ms nach `previous` (macht
    /// `updated_at` je Objekt streng monoton – Grundlage von [`Expect::Version`]).
    pub(crate) fn next_version(previous: Option<OffsetDateTime>) -> OffsetDateTime {
        let t = trunc_millis(now());
        match previous {
            Some(p) if t <= p => p + time::Duration::milliseconds(1),
            _ => t,
        }
    }

    /// Wie [`AdminTx::update_row`], mit Versionsvergleich im `UPDATE` (F38).
    pub(crate) async fn update_row_expect(
        &mut self,
        kind: ResourceKind,
        id: Id,
        meta: &ObjectMeta,
        cols: Vec<(&'static str, Val)>,
        expect: Expect,
    ) -> Result<(), StoreError> {
        let previous = self.row_version(table(kind), id).await?;
        let mut sets = vec!["labels = $1".to_owned(), "updated_at = $2".to_owned()];
        let mut vals = vec![
            Val::json(&meta.labels)?,
            Val::time(Self::next_version(previous)),
        ];
        for (n, v) in cols {
            vals.push(v);
            sets.push(format!("{n} = ${}", vals.len()));
        }
        vals.push(Val::uuid(id));
        let mut sql = format!(
            "UPDATE {} SET {} WHERE id = ${}",
            table(kind),
            sets.join(", "),
            vals.len()
        );
        if let Some(v) = expect.version() {
            vals.push(Val::time(v));
            sql.push_str(&format!(" AND updated_at = ${}", vals.len()));
        }
        let n = Q { sql, args: vals }
            .execute(&mut self.tx)
            .await
            .map_err(|e| map_db(e, &format!("{kind} {:?}", meta.name)))?;
        if n == 0 {
            return Err(match (expect, previous) {
                (_, None) => StoreError::NotFound {
                    kind,
                    name: meta.name.clone(),
                },
                (Expect::Version(_), Some(_)) => StoreError::PreconditionFailed {
                    kind: kind.to_string(),
                    name: meta.name.clone(),
                },
                (Expect::Any, Some(_)) => {
                    StoreError::Corrupt(format!("{kind} {:?}: UPDATE ohne Wirkung", meta.name))
                }
            });
        }
        Ok(())
    }

    /// Prüft den erwarteten Stand einer Zeile in dieser Transaktion (für Schreibvorgänge, die
    /// die Zeile nicht über [`AdminTx::update_row_expect`] ändern).
    pub(crate) async fn check_expect(
        &mut self,
        table_name: &str,
        kind: &str,
        name: &str,
        id: Id,
        expect: Expect,
    ) -> Result<(), StoreError> {
        let Some(v) = expect.version() else {
            return Ok(());
        };
        if self.row_version(table_name, id).await? == Some(v) {
            Ok(())
        } else {
            Err(StoreError::PreconditionFailed {
                kind: kind.to_owned(),
                name: name.to_owned(),
            })
        }
    }

    /// Pflegt den Alias-Namensraum einer Modellgruppe.
    pub(crate) async fn write_aliases(
        &mut self,
        id: Id,
        name: &str,
        aliases: &[String],
    ) -> Result<(), StoreError> {
        Q::new("DELETE FROM model_alias WHERE model_group_id = $1")
            .bind(Val::uuid(id))
            .execute(&mut self.tx)
            .await?;
        for alias in std::iter::once(name).chain(aliases.iter().map(String::as_str)) {
            Q::new("INSERT INTO model_alias (alias, model_group_id) VALUES ($1, $2)")
                .bind(Val::text(alias))
                .bind(Val::uuid(id))
                .execute(&mut self.tx)
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
        self.delete_row_expect(kind, id, "", Expect::Any).await
    }

    /// Wie [`AdminTx::delete_row`], mit Versionsvergleich im `DELETE` (F38).
    pub(crate) async fn delete_row_expect(
        &mut self,
        kind: ResourceKind,
        id: Id,
        name: &str,
        expect: Expect,
    ) -> Result<(), StoreError> {
        // Vorab (in der Transaktion), damit abhängige Bindungen nur bei passendem Stand fallen.
        self.check_expect(table(kind), kind.as_str(), name, id, expect)
            .await?;
        let level = match kind {
            ResourceKind::Organization => Some(ScopeLevel::Organization),
            ResourceKind::Team => Some(ScopeLevel::Team),
            ResourceKind::ApiKey => Some(ScopeLevel::ApiKey),
            ResourceKind::ModelGroup => Some(ScopeLevel::ModelGroup),
            _ => None,
        };
        if let Some(level) = level {
            Q::new("DELETE FROM plugin_binding WHERE level = $1 AND target_id = $2")
                .bind(Val::text(level.as_str()))
                .bind(Val::uuid(id))
                .execute(&mut self.tx)
                .await?;
            Q::new("DELETE FROM mcp_grant WHERE subject_level = $1 AND subject_id = $2")
                .bind(Val::text(level.as_str()))
                .bind(Val::uuid(id))
                .execute(&mut self.tx)
                .await?;
        }
        let mut q =
            Q::new(format!("DELETE FROM {} WHERE id = $1", table(kind))).bind(Val::uuid(id));
        if let Some(v) = expect.version() {
            q.sql.push_str(" AND updated_at = $2");
            q = q.bind(Val::time(v));
        }
        let n = q
            .execute(&mut self.tx)
            .await
            .map_err(|e| map_db(e, &kind.to_string()))?;
        if n == 0 && expect.version().is_some() {
            return Err(StoreError::PreconditionFailed {
                kind: kind.to_string(),
                name: name.to_owned(),
            });
        }
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
        Q::new("SELECT version FROM config_version WHERE id = 1")
            .fetch_i64(&self.pool)
            .await
    }

    pub(crate) async fn fetch_rows(
        &self,
        kind: ResourceKind,
        refs: &[RefCol],
        filter: &str,
        arg: Option<Val>,
    ) -> Result<Vec<DbRow>, StoreError> {
        let mut q = Q::new(select_sql(kind, refs, filter));
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
    ) -> Result<DbRow, StoreError> {
        self.fetch_rows(kind, refs, "WHERE t.name = $1", Some(Val::text(name)))
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| StoreError::NotFound {
                kind,
                name: name.to_owned(),
            })
    }

    pub(crate) fn object_from_row<R: Resource>(row: &DbRow) -> Result<Object<R>, StoreError> {
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
        self.fetch_rows(R::KIND, R::REFS, "WHERE t.id = $1", Some(Val::uuid(id)))
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
        self.create_with(actor, meta, spec, &WriteExtras::default())
            .await
    }

    /// Wie [`Store::create`], mit zusätzlichen Schreibvorgängen in derselben Transaktion.
    pub async fn create_with<R: PlainResource>(
        &self,
        actor: &str,
        meta: ObjectMeta,
        spec: R,
        extras: &WriteExtras,
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
        tx.apply_extras(extras, R::KIND, id, &created.metadata.name)
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
        self.update_with(actor, meta, spec, &WriteExtras::default())
            .await
    }

    /// Wie [`Store::update`], mit zusätzlichen Schreibvorgängen in derselben Transaktion.
    pub async fn update_with<R: PlainResource>(
        &self,
        actor: &str,
        meta: ObjectMeta,
        spec: R,
        extras: &WriteExtras,
    ) -> Result<Object<R>, StoreError> {
        spec.validate()?;
        let before: Object<R> = self.get(&meta.name).await?;
        let mut tx = AdminTx::begin(self, actor).await?;
        let cols = tx.resolve_cols(spec.to_columns()?).await?;
        tx.update_row_expect(R::KIND, before.id, &meta, cols, extras.expect)
            .await?;
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
        if R::KIND == ResourceKind::User
            && audit::user_change_ends_sessions(
                &serde_json::to_value(&before.spec)?,
                &serde_json::to_value(&after.spec)?,
            )
        {
            tx.end_user_sessions(before.id, &after.metadata.name, "roles-changed")
                .await?;
        }
        tx.apply_extras(extras, R::KIND, before.id, &after.metadata.name)
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
        self.delete_expect(actor, kind, name, Expect::Any).await
    }

    /// Wie [`Store::delete`], nur bei erwartetem Stand (F38, `If-Match`).
    pub async fn delete_expect(
        &self,
        actor: &str,
        kind: ResourceKind,
        name: &str,
        expect: Expect,
    ) -> Result<(), StoreError> {
        self.delete_with(actor, kind, name, expect, &WriteExtras::default())
            .await
    }

    /// Wie [`Store::delete_expect`], mit zusätzlichen Schreibvorgängen in derselben
    /// Transaktion (z. B. `?cascade=remove`, F66) – vor dem Löschen ausgeführt.
    pub async fn delete_with(
        &self,
        actor: &str,
        kind: ResourceKind,
        name: &str,
        expect: Expect,
        extras: &WriteExtras,
    ) -> Result<(), StoreError> {
        let before = self.get_json(kind, name).await?;
        let id = before
            .get("id")
            .and_then(|v| v.as_str())
            .map(parse_id)
            .transpose()?
            .ok_or_else(|| StoreError::Corrupt("Objekt ohne ID".into()))?;
        let mut tx = AdminTx::begin(self, actor).await?;
        tx.check_expect(table(kind), kind.as_str(), name, id, expect)
            .await?;
        tx.apply_extras(extras, kind, id, name).await?;
        if kind == ResourceKind::User {
            // Der Fremdschlüssel löscht die Sessions ohnehin (ON DELETE CASCADE); ausdrücklich,
            // damit es nicht von der Fremdschlüssel-Durchsetzung abhängt und im Audit-Log steht.
            tx.end_user_sessions(id, name, "user-deleted").await?;
        }
        tx.delete_row_expect(kind, id, name, expect).await?;
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
            ResourceKind::Secret => serde_json::to_value(self.get_secret(name).await?),
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
}
