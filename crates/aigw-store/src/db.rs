// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Datenbankzugriff über **getrennte Treiber** (F7, AIGW-73): sqlx `Sqlite` und `Postgres`
//! mit nativen Typen statt des `Any`-Treibers.
//!
//! **Enum-Dispatch** statt Trait-Objekt oder Generics: Es gibt genau zwei Backends, die Menge
//! ist geschlossen. [`Pool`], [`Tx`] und [`DbRow`] sind Enums; eine Abfrage ([`Q`]) besteht aus
//! SQL-Text und typisierten Werten ([`Val`]) und wird erst beim Ausführen an den jeweiligen
//! Treiber gebunden. So bleibt `Store` ein konkreter Typ (Aufrufer in Admin, GitOps, Analytics,
//! Observe, Billing, Events, Cache und Audit ändern sich nicht), es gibt keine Generics, die
//! sich bis in die Aufrufer ziehen, und kein `async-trait`/`dyn` im Datenpfad. Der weitaus
//! größte Teil des SQL ist für beide Dialekte gleich (`$n`-Platzhalter, `ON CONFLICT`,
//! `RETURNING`); wo nicht, wählt der Aufrufer per [`crate::Backend`] den Text.
//!
//! Abbildung der Werte:
//!
//! | [`Val`] | Postgres | SQLite |
//! |---|---|---|
//! | `Uuid` | `UUID` | `TEXT` (Bindestrich-Form) |
//! | `Ms` (Zeitpunkt, Unix-ms) | `TIMESTAMPTZ` | `INTEGER` (Unix-ms) |
//! | `Json` | `JSONB` | `TEXT` |
//! | `Bool` | `BOOLEAN` | `INTEGER` 0/1 |
//! | `Bytes` | `BYTEA` | `BLOB` |
//! | `Dim` (Rollup-Schlüssel) | `UUID`, „kein Wert“ = Null-UUID | `TEXT`, „kein Wert“ = `''` |
//! | `Int`, `Text` | `BIGINT`, `TEXT` | `INTEGER`, `TEXT` |

use serde::de::DeserializeOwned;
use sqlx::postgres::{PgArguments, PgPool, PgRow};
use sqlx::query::Query;
use sqlx::sqlite::{SqliteArguments, SqlitePool, SqliteRow};
use sqlx::types::Json;
use sqlx::{AssertSqlSafe, Column as _, Postgres, Row as _, Sqlite, Transaction, TypeInfo as _};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{Backend, StoreError};

/// Verbindungspool des gewählten Backends.
#[derive(Debug, Clone)]
pub(crate) enum Pool {
    Sqlite(SqlitePool),
    Postgres(PgPool),
}

/// Laufende Transaktion.
#[derive(Debug)]
pub(crate) enum Tx {
    Sqlite(Transaction<'static, Sqlite>),
    Postgres(Transaction<'static, Postgres>),
}

impl Pool {
    pub(crate) fn backend(&self) -> Backend {
        match self {
            Self::Sqlite(_) => Backend::Sqlite,
            Self::Postgres(_) => Backend::Postgres,
        }
    }

    pub(crate) async fn begin(&self) -> Result<Tx, sqlx::Error> {
        Ok(match self {
            Self::Sqlite(p) => Tx::Sqlite(p.begin().await?),
            Self::Postgres(p) => Tx::Postgres(p.begin().await?),
        })
    }

    pub(crate) async fn close(&self) {
        match self {
            Self::Sqlite(p) => p.close().await,
            Self::Postgres(p) => p.close().await,
        }
    }
}

impl Tx {
    pub(crate) async fn commit(self) -> Result<(), sqlx::Error> {
        match self {
            Self::Sqlite(t) => t.commit().await,
            Self::Postgres(t) => t.commit().await,
        }
    }
}

/// Ein gebundener Wert (siehe Tabelle im Modulkommentar).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Val {
    Text(Option<String>),
    Int(Option<i64>),
    Bool(Option<bool>),
    Uuid(Option<Uuid>),
    /// Zeitpunkt in Unix-Millisekunden.
    Ms(Option<i64>),
    Json(Option<serde_json::Value>),
    Bytes(Option<Vec<u8>>),
    /// Schlüsselspalte eines Rollups (ID oder „kein Wert“).
    Dim(Option<Uuid>),
}

impl Val {
    pub(crate) fn text(s: impl Into<String>) -> Self {
        Self::Text(Some(s.into()))
    }
    pub(crate) fn int(i: i64) -> Self {
        Self::Int(Some(i))
    }
    pub(crate) fn uuid(u: Uuid) -> Self {
        Self::Uuid(Some(u))
    }
    pub(crate) fn ms(ms: i64) -> Self {
        Self::Ms(Some(ms))
    }
    pub(crate) fn time(t: OffsetDateTime) -> Self {
        Self::Ms(Some(crate::repo::millis(t)))
    }
    pub(crate) fn opt_time(t: Option<OffsetDateTime>) -> Self {
        Self::Ms(t.map(crate::repo::millis))
    }
    pub(crate) fn bool(b: bool) -> Self {
        Self::Bool(Some(b))
    }
    pub(crate) fn json<T: serde::Serialize>(v: &T) -> Result<Self, StoreError> {
        Ok(Self::Json(Some(serde_json::to_value(v)?)))
    }

    /// UUID aus Text (z. B. IDs aus `RequestLogRow`); ungültiger Text ist ein Fehler.
    pub(crate) fn uuid_text(s: Option<&str>, what: &str) -> Result<Self, StoreError> {
        s.map(|s| parse_uuid(s, what)).transpose().map(Self::Uuid)
    }

    /// Rollup-Schlüssel aus Text (`""` = kein Wert).
    pub(crate) fn dim_text(s: &str, what: &str) -> Result<Self, StoreError> {
        if s.is_empty() {
            Ok(Self::Dim(None))
        } else {
            parse_uuid(s, what).map(|u| Self::Dim(Some(u)))
        }
    }
}

pub(crate) fn parse_uuid(s: &str, what: &str) -> Result<Uuid, StoreError> {
    Uuid::parse_str(s).map_err(|_| StoreError::Invalid(format!("{what}: keine UUID: {s:?}")))
}

/// Unix-ms → Zeitpunkt; außerhalb des darstellbaren Bereichs auf die Grenzen gesättigt
/// (z. B. `i64::MAX` als „unbegrenzt“).
fn ms_to_time(ms: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).unwrap_or(if ms < 0 {
        // ≈ 3734 v. Chr. (Postgres reicht bis 4713 v. Chr.).
        OffsetDateTime::from_unix_timestamp(-180_000_000_000).unwrap_or(OffsetDateTime::UNIX_EPOCH)
    } else {
        time::PrimitiveDateTime::MAX.assume_utc()
    })
}

fn bind_sqlite<'q>(
    q: Query<'q, Sqlite, SqliteArguments>,
    v: Val,
) -> Query<'q, Sqlite, SqliteArguments> {
    match v {
        Val::Text(t) => q.bind(t),
        Val::Int(i) | Val::Ms(i) => q.bind(i),
        Val::Bool(b) => q.bind(b.map(i64::from)),
        Val::Uuid(u) => q.bind(u.map(|u| u.hyphenated().to_string())),
        Val::Json(j) => q.bind(j.map(|j| j.to_string())),
        Val::Bytes(b) => q.bind(b),
        Val::Dim(d) => q.bind(d.map_or_else(String::new, |u| u.hyphenated().to_string())),
    }
}

fn bind_pg<'q>(q: Query<'q, Postgres, PgArguments>, v: Val) -> Query<'q, Postgres, PgArguments> {
    match v {
        Val::Text(t) => q.bind(t),
        Val::Int(i) => q.bind(i),
        Val::Ms(i) => q.bind(i.map(ms_to_time)),
        Val::Bool(b) => q.bind(b),
        Val::Uuid(u) => q.bind(u),
        Val::Json(j) => q.bind(j.map(Json)),
        Val::Bytes(b) => q.bind(b),
        Val::Dim(d) => q.bind(d.unwrap_or(Uuid::nil())),
    }
}

/// Ausführungsziel: Pool oder Transaktion.
pub(crate) enum Ex<'a> {
    Pool(&'a Pool),
    Tx(&'a mut Tx),
}

impl<'a> From<&'a Pool> for Ex<'a> {
    fn from(p: &'a Pool) -> Self {
        Self::Pool(p)
    }
}

impl<'a> From<&'a mut Tx> for Ex<'a> {
    fn from(t: &'a mut Tx) -> Self {
        Self::Tx(t)
    }
}

/// Eine Abfrage: SQL-Text plus Werte in Bindungsreihenfolge.
#[derive(Debug, Clone, Default)]
pub(crate) struct Q {
    pub(crate) sql: String,
    pub(crate) args: Vec<Val>,
}

macro_rules! prepared {
    ($db:ty, $bind:ident, $sql:expr, $args:expr) => {{
        let mut q = sqlx::query::<$db>(AssertSqlSafe($sql));
        for v in $args {
            q = $bind(q, v);
        }
        q
    }};
}

macro_rules! run {
    ($self:ident, $ex:ident, |$q:ident, $e:ident| $body:expr) => {{
        let Q { sql, args } = $self;
        match $ex.into() {
            Ex::Pool(Pool::Sqlite(p)) => {
                let $q = prepared!(Sqlite, bind_sqlite, sql, args);
                let $e = p;
                $body
            }
            Ex::Pool(Pool::Postgres(p)) => {
                let $q = prepared!(Postgres, bind_pg, sql, args);
                let $e = p;
                $body
            }
            Ex::Tx(Tx::Sqlite(t)) => {
                let $q = prepared!(Sqlite, bind_sqlite, sql, args);
                let $e = &mut **t;
                $body
            }
            Ex::Tx(Tx::Postgres(t)) => {
                let $q = prepared!(Postgres, bind_pg, sql, args);
                let $e = &mut **t;
                $body
            }
        }
    }};
}

impl Q {
    pub(crate) fn new(sql: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            args: Vec::new(),
        }
    }

    /// Hängt einen Wert an (Reihenfolge = Platzhalter `$1`, `$2` …).
    pub(crate) fn bind(mut self, v: Val) -> Self {
        self.args.push(v);
        self
    }

    /// Fügt einen Wert hinzu und liefert seinen Platzhalter (`$n`) für dynamisches SQL.
    pub(crate) fn arg(&mut self, v: Val) -> String {
        self.args.push(v);
        format!("${}", self.args.len())
    }

    /// Platzhalterliste für `IN (…)`.
    pub(crate) fn list(&mut self, vs: impl IntoIterator<Item = Val>) -> String {
        vs.into_iter()
            .map(|v| self.arg(v))
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub(crate) async fn execute<'a>(self, ex: impl Into<Ex<'a>>) -> Result<u64, sqlx::Error> {
        run!(self, ex, |q, e| Ok(q.execute(e).await?.rows_affected()))
    }

    pub(crate) async fn fetch_all<'a>(
        self,
        ex: impl Into<Ex<'a>>,
    ) -> Result<Vec<DbRow>, sqlx::Error> {
        run!(self, ex, |q, e| Ok(q
            .fetch_all(e)
            .await?
            .into_iter()
            .map(DbRow::from)
            .collect()))
    }

    pub(crate) async fn fetch_optional<'a>(
        self,
        ex: impl Into<Ex<'a>>,
    ) -> Result<Option<DbRow>, sqlx::Error> {
        run!(self, ex, |q, e| Ok(q
            .fetch_optional(e)
            .await?
            .map(DbRow::from)))
    }

    pub(crate) async fn fetch_one<'a>(self, ex: impl Into<Ex<'a>>) -> Result<DbRow, sqlx::Error> {
        run!(self, ex, |q, e| Ok(DbRow::from(q.fetch_one(e).await?)))
    }

    /// Erste Spalte der ersten Zeile als Zahl (z. B. `COUNT(*)`, `RETURNING version`).
    pub(crate) async fn fetch_i64<'a>(self, ex: impl Into<Ex<'a>>) -> Result<i64, StoreError> {
        self.fetch_one(ex).await?.i64_at(0)
    }
}

/// Eine Ergebniszeile.
#[derive(Debug)]
pub(crate) enum DbRow {
    Sqlite(SqliteRow),
    Postgres(PgRow),
}

impl From<SqliteRow> for DbRow {
    fn from(r: SqliteRow) -> Self {
        Self::Sqlite(r)
    }
}

impl From<PgRow> for DbRow {
    fn from(r: PgRow) -> Self {
        Self::Postgres(r)
    }
}

fn null(col: &str) -> StoreError {
    StoreError::Corrupt(format!("Spalte {col} ist NULL"))
}

macro_rules! get {
    ($self:ident, $t:ty, $col:expr) => {
        match $self {
            DbRow::Sqlite(r) => r.try_get::<$t, _>($col)?,
            DbRow::Postgres(r) => r.try_get::<$t, _>($col)?,
        }
    };
}

impl DbRow {
    pub(crate) fn opt_text(&self, col: &str) -> Result<Option<String>, StoreError> {
        Ok(get!(self, Option<String>, col))
    }

    pub(crate) fn text(&self, col: &str) -> Result<String, StoreError> {
        self.opt_text(col)?.ok_or_else(|| null(col))
    }

    pub(crate) fn opt_i64(&self, col: &str) -> Result<Option<i64>, StoreError> {
        Ok(get!(self, Option<i64>, col))
    }

    pub(crate) fn i64(&self, col: &str) -> Result<i64, StoreError> {
        self.opt_i64(col)?.ok_or_else(|| null(col))
    }

    fn i64_at(&self, idx: usize) -> Result<i64, StoreError> {
        Ok(match self {
            DbRow::Sqlite(r) => r.try_get::<i64, _>(idx)?,
            DbRow::Postgres(r) => r.try_get::<i64, _>(idx)?,
        })
    }

    pub(crate) fn bool(&self, col: &str) -> Result<bool, StoreError> {
        match self {
            DbRow::Sqlite(r) => Ok(r.try_get::<i64, _>(col)? != 0),
            DbRow::Postgres(r) => Ok(r.try_get::<bool, _>(col)?),
        }
    }

    pub(crate) fn opt_bool(&self, col: &str) -> Result<Option<bool>, StoreError> {
        match self {
            DbRow::Sqlite(r) => Ok(r.try_get::<Option<i64>, _>(col)?.map(|v| v != 0)),
            DbRow::Postgres(r) => Ok(r.try_get::<Option<bool>, _>(col)?),
        }
    }

    pub(crate) fn opt_uuid(&self, col: &str) -> Result<Option<Uuid>, StoreError> {
        match self {
            DbRow::Sqlite(r) => r
                .try_get::<Option<String>, _>(col)?
                .map(|s| {
                    Uuid::parse_str(&s)
                        .map_err(|_| StoreError::Corrupt(format!("Spalte {col}: keine UUID {s:?}")))
                })
                .transpose(),
            DbRow::Postgres(r) => Ok(r.try_get::<Option<Uuid>, _>(col)?),
        }
    }

    pub(crate) fn uuid(&self, col: &str) -> Result<Uuid, StoreError> {
        self.opt_uuid(col)?.ok_or_else(|| null(col))
    }

    /// UUID-Spalte als Text (für Strukturen mit Text-IDs).
    pub(crate) fn opt_uuid_text(&self, col: &str) -> Result<Option<String>, StoreError> {
        Ok(self.opt_uuid(col)?.map(|u| u.hyphenated().to_string()))
    }

    /// Zeitpunkt in Unix-ms.
    pub(crate) fn opt_ms(&self, col: &str) -> Result<Option<i64>, StoreError> {
        match self {
            DbRow::Sqlite(r) => Ok(r.try_get::<Option<i64>, _>(col)?),
            DbRow::Postgres(r) => Ok(r
                .try_get::<Option<OffsetDateTime>, _>(col)?
                .map(crate::repo::millis)),
        }
    }

    pub(crate) fn ms(&self, col: &str) -> Result<i64, StoreError> {
        self.opt_ms(col)?.ok_or_else(|| null(col))
    }

    pub(crate) fn opt_time(&self, col: &str) -> Result<Option<OffsetDateTime>, StoreError> {
        self.opt_ms(col)?.map(crate::repo::from_millis).transpose()
    }

    pub(crate) fn time(&self, col: &str) -> Result<OffsetDateTime, StoreError> {
        self.opt_time(col)?.ok_or_else(|| null(col))
    }

    pub(crate) fn opt_json_value(
        &self,
        col: &str,
    ) -> Result<Option<serde_json::Value>, StoreError> {
        match self {
            DbRow::Sqlite(r) => Ok(r
                .try_get::<Option<String>, _>(col)?
                .map(|s| serde_json::from_str(&s))
                .transpose()?),
            DbRow::Postgres(r) => Ok(r
                .try_get::<Option<Json<serde_json::Value>>, _>(col)?
                .map(|j| j.0)),
        }
    }

    pub(crate) fn opt_json<T: DeserializeOwned>(&self, col: &str) -> Result<Option<T>, StoreError> {
        Ok(self
            .opt_json_value(col)?
            .map(serde_json::from_value)
            .transpose()?)
    }

    pub(crate) fn json<T: DeserializeOwned>(&self, col: &str) -> Result<T, StoreError> {
        self.opt_json(col)?.ok_or_else(|| null(col))
    }

    pub(crate) fn bytes(&self, col: &str) -> Result<Vec<u8>, StoreError> {
        get!(self, Option<Vec<u8>>, col).ok_or_else(|| null(col))
    }

    /// Rollup-Schlüssel oder Textspalte als Text (`""` = kein Wert). Postgres-UUID-Spalten
    /// werden erkannt (Null-UUID → `""`), Textspalten unverändert gelesen.
    pub(crate) fn dim(&self, col: &str) -> Result<String, StoreError> {
        match self {
            DbRow::Sqlite(r) => Ok(r.try_get::<Option<String>, _>(col)?.unwrap_or_default()),
            DbRow::Postgres(r) => {
                let is_uuid = r
                    .columns()
                    .iter()
                    .find(|c| c.name() == col)
                    .is_some_and(|c| c.type_info().name() == "UUID");
                if is_uuid {
                    Ok(r.try_get::<Option<Uuid>, _>(col)?
                        .filter(|u| !u.is_nil())
                        .map(|u| u.hyphenated().to_string())
                        .unwrap_or_default())
                } else {
                    Ok(r.try_get::<Option<String>, _>(col)?.unwrap_or_default())
                }
            }
        }
    }
}
