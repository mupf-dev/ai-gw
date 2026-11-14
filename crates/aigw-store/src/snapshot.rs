// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Aufbau des Konfigurations-Snapshots aus der Datenbank: vollständig ([`load_snapshot`]) oder
//! inkrementell über das Änderungsjournal ([`load_incremental`], F16, AIGW-75).
//!
//! **Änderungsjournal = `audit_log`.** Jede Konfigurationsänderung läuft als Transaktion, die
//! `config_version` erhöht und in derselben Transaktion je geändertem Objekt einen Audit-Eintrag
//! mit `config_version`, `kind` und `object_id` schreibt. Damit ist das Audit-Log ein lückenloses
//! Journal aller Änderungen; eine eigene `change_log`-Tabelle würde dieselben Daten doppelt
//! schreiben. Zusätzlich nötig ist nur ein Index auf `audit_log.config_version` (Migration 0002).
//!
//! Inkrementell heißt: Für die Versionen `(aktiv, neu]` werden die geänderten Objekt-IDs aus dem
//! Journal gelesen, genau diese Zeilen frisch geladen und per [`Snapshot::apply_patch`]
//! eingearbeitet. Abhängigkeiten ohne eigenen Audit-Eintrag werden mitgeladen:
//!
//! * Team in eine andere Organisation umgezogen → alle Keys des Teams (`ON UPDATE CASCADE`
//!   ändert deren Organisation mit). Sonstige Team-/Organisationsänderungen bindet
//!   [`Snapshot::apply_patch`] im Speicher neu.
//! * Organisation, Team, Key, Modellgruppe oder MCP-Server gelöscht → Plugin-Bindungen und
//!   MCP-Freigaben komplett neu (das Repository bzw. `ON DELETE CASCADE` löschen sie mit).
//!
//! Rückfall auf den vollständigen Neubau ([`Fallback`]): Lücke im Journal (eine Version ohne
//! Audit-Eintrag, z. B. nach Aufräumen des Audit-Logs), Eintrag ohne Objekt-ID, mehr als
//! `max_changes` Einträge, Version rückwärts (Datenbank zurückgespielt), Dauerkonflikt.
//! Einträge, die keine Ressourcenart sind (Admin-Keys, Sessions …), betreffen den Snapshot
//! nicht und werden übersprungen.

use std::collections::{HashMap, HashSet};

use aigw_core::resource::{
    ApiKeySpec, DeploymentSpec, Id, McpGrantSpec, McpServerSpec, ModelGroupSpec, Object,
    OrganizationSpec, PluginBindingSpec, PriceSpec, ResourceKind, TeamSpec,
};
use aigw_core::snapshot::{Change, Snapshot, SnapshotParts, SnapshotPatch};
use aigw_core::tools::ToolPolicySpec;

use crate::crypto::Keyring;
use crate::db::{DbRow, Q, Val};
use crate::repo::credential::decrypt_row;
use crate::repo::{Resource, api_key_from_row, select_sql};
use crate::{Store, StoreError};

/// Höchstzahl Versuche, wenn sich die Konfiguration während des Ladens ändert.
const MAX_ATTEMPTS: usize = 5;

/// Höchstzahl IDs je `IN (…)`-Abfrage.
const ID_CHUNK: usize = 500;

/// Lädt einen vollständigen, in sich konsistenten Snapshot.
///
/// Konsistenz: Die Version wird vor und nach dem Lesen geprüft; ändert sie sich dazwischen,
/// wird neu geladen (optimistisch, ohne lange Lesetransaktion).
pub async fn load_snapshot(store: &Store, keyring: &Keyring) -> Result<Snapshot, StoreError> {
    Ok(Snapshot::from_parts(load_parts(store, keyring).await?))
}

/// Wie [`load_snapshot`], liefert aber die Rohdaten (für Tests und den Platten-Cache).
pub async fn load_parts(store: &Store, keyring: &Keyring) -> Result<SnapshotParts, StoreError> {
    for _ in 0..MAX_ATTEMPTS {
        let version = store.config_version().await?;
        let parts = read_all(store, keyring, version).await?;
        if store.config_version().await? == version {
            return Ok(parts);
        }
        tracing::debug!(
            version,
            "Konfiguration während des Snapshot-Aufbaus geändert, erneut"
        );
    }
    Err(StoreError::Conflict(
        "Konfiguration ändert sich zu schnell für einen konsistenten Snapshot".into(),
    ))
}

async fn read_all(
    store: &Store,
    keyring: &Keyring,
    version: i64,
) -> Result<SnapshotParts, StoreError> {
    let keys = store
        .fetch_rows(ResourceKind::ApiKey, ApiKeySpec::REFS, "", None)
        .await?
        .iter()
        .map(|row| api_key_from_row(row).map(|(k, h)| (h, k)))
        .collect::<Result<Vec<_>, _>>()?;
    let credentials = store
        .fetch_rows(ResourceKind::ProviderCredential, &[], "", None)
        .await?
        .iter()
        .map(|row| decrypt_row(keyring, row))
        .collect::<Result<Vec<_>, _>>()?;
    let secrets = store
        .fetch_rows(ResourceKind::Secret, &[], "", None)
        .await?
        .iter()
        .map(|row| crate::repo::secret::decrypt_row(keyring, row))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(SnapshotParts {
        version,
        organizations: store.list::<OrganizationSpec>().await?,
        teams: store.list::<TeamSpec>().await?,
        keys,
        credentials,
        secrets,
        model_groups: store.list::<ModelGroupSpec>().await?,
        deployments: store.list::<DeploymentSpec>().await?,
        prices: store.list::<PriceSpec>().await?,
        plugin_bindings: store.list::<PluginBindingSpec>().await?,
        mcp_servers: store.list::<McpServerSpec>().await?,
        mcp_grants: store.list::<McpGrantSpec>().await?,
        tool_policies: store.list::<ToolPolicySpec>().await?,
    })
}

/// Grund für den vollständigen Neubau statt des inkrementellen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fallback {
    /// Mindestens eine Version ohne Audit-Eintrag (Journal unvollständig).
    Gap,
    /// Mehr Änderungen als erlaubt.
    TooMany(usize),
    /// Audit-Eintrag ohne Objekt-ID.
    MissingId,
    /// Datenbank-Version kleiner als die des aktiven Snapshots (zurückgespielt?).
    VersionBackwards,
    /// Konfiguration änderte sich während jedes Versuchs.
    Busy,
}

impl Fallback {
    /// Kurzname für Logs und Metriken.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Gap => "gap",
            Self::TooMany(_) => "too_many",
            Self::MissingId => "missing_id",
            Self::VersionBackwards => "version_backwards",
            Self::Busy => "busy",
        }
    }
}

/// Ergebnis von [`load_incremental`].
#[derive(Debug)]
pub enum Incremental {
    /// Die Datenbank hat dieselbe Version wie der Basis-Snapshot.
    UpToDate,
    /// Neuer Snapshot aus Basis + Änderungen.
    Built {
        /// Neuer Snapshot.
        snapshot: Box<Snapshot>,
        /// Anzahl gelesener Journal-Einträge.
        changes: usize,
    },
    /// Inkrementell nicht möglich – vollständig neu bauen.
    NeedFull(Fallback),
}

/// Geänderte IDs je Art aus dem Journal.
#[derive(Debug, Default)]
struct Changed {
    ids: HashMap<ResourceKind, HashSet<Id>>,
}

impl Changed {
    fn of(&self, kind: ResourceKind) -> Vec<Id> {
        let mut v: Vec<Id> = self
            .ids
            .get(&kind)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default();
        v.sort();
        v
    }
}

/// Baut den Snapshot inkrementell aus `base` und dem Änderungsjournal seit `base.version`.
pub async fn load_incremental(
    store: &Store,
    keyring: &Keyring,
    base: &Snapshot,
    max_changes: usize,
) -> Result<Incremental, StoreError> {
    for _ in 0..MAX_ATTEMPTS {
        let latest = store.config_version().await?;
        if latest == base.version {
            return Ok(Incremental::UpToDate);
        }
        if latest < base.version {
            return Ok(Incremental::NeedFull(Fallback::VersionBackwards));
        }
        let (changed, entries) =
            match read_journal(store, base.version, latest, max_changes).await? {
                Ok(c) => c,
                Err(f) => return Ok(Incremental::NeedFull(f)),
            };
        let patch = fetch_patch(store, keyring, base, &changed, latest).await?;
        if store.config_version().await? == latest {
            return Ok(Incremental::Built {
                snapshot: Box::new(base.apply_patch(patch)),
                changes: entries,
            });
        }
        tracing::debug!(
            latest,
            "Konfiguration während des inkrementellen Neubaus geändert, erneut"
        );
    }
    Ok(Incremental::NeedFull(Fallback::Busy))
}

/// Liest das Journal `(from, to]`; `Err(Fallback)`, wenn inkrementell nicht möglich ist.
async fn read_journal(
    store: &Store,
    from: i64,
    to: i64,
    max_changes: usize,
) -> Result<Result<(Changed, usize), Fallback>, StoreError> {
    let limit = i64::try_from(max_changes)
        .unwrap_or(i64::MAX)
        .saturating_add(1);
    let rows = Q::new(
        "SELECT config_version, kind, object_id FROM audit_log \
         WHERE config_version > $1 AND config_version <= $2 \
         ORDER BY config_version LIMIT $3",
    )
    .bind(Val::int(from))
    .bind(Val::int(to))
    .bind(Val::int(limit))
    .fetch_all(&store.pool)
    .await?;
    if rows.len() > max_changes {
        return Ok(Err(Fallback::TooMany(rows.len())));
    }
    let mut versions = HashSet::new();
    let mut changed = Changed::default();
    for row in &rows {
        versions.insert(row.i64("config_version")?);
        let kind = row.text("kind")?;
        let Some(kind) = snapshot_kind(&kind) else {
            continue;
        };
        let Some(id) = row.opt_uuid("object_id")? else {
            return Ok(Err(Fallback::MissingId));
        };
        changed.ids.entry(kind).or_default().insert(id);
    }
    let expected = usize::try_from(to - from).unwrap_or(usize::MAX);
    if versions.len() != expected {
        return Ok(Err(Fallback::Gap));
    }
    Ok(Ok((changed, rows.len())))
}

/// Art eines Journal-Eintrags mit Bezug zum Snapshot.
///
/// Keine Ressourcenart (`admin-api-key`, `admin-session`, `bootstrap-token` …) und Nutzer
/// gehören nicht zum Snapshot. Der `match` ist erschöpfend: Eine neue Ressourcenart muss hier
/// (und im Patch) bewusst eingeordnet werden.
fn snapshot_kind(s: &str) -> Option<ResourceKind> {
    // F115: Arten im Audit-Log kebab-case (`api-key`); ältere Einträge in YAML-Schreibweise.
    let kind = ResourceKind::parse_any(s)?;
    match kind {
        ResourceKind::User => None,
        ResourceKind::Organization
        | ResourceKind::Team
        | ResourceKind::ApiKey
        | ResourceKind::ProviderCredential
        | ResourceKind::Secret
        | ResourceKind::ModelGroup
        | ResourceKind::Deployment
        | ResourceKind::Price
        | ResourceKind::PluginBinding
        | ResourceKind::McpServer
        | ResourceKind::McpGrant
        | ResourceKind::ToolPolicy => Some(kind),
    }
}

async fn fetch_ids(
    store: &Store,
    kind: ResourceKind,
    refs: &[crate::repo::RefCol],
    column: &str,
    ids: &[Id],
) -> Result<Vec<DbRow>, StoreError> {
    let mut out = Vec::new();
    for chunk in ids.chunks(ID_CHUNK) {
        let mut q = Q::new(String::new());
        let list = q.list(chunk.iter().map(|id| Val::uuid(*id)));
        q.sql = select_sql(kind, refs, &format!("WHERE t.{column} IN ({list})"));
        out.extend(q.fetch_all(&store.pool).await?);
    }
    Ok(out)
}

/// Lädt Objekte einer Art nach ID; nicht mehr vorhandene werden zu [`Change::Remove`].
async fn changes_of<R: Resource>(
    store: &Store,
    changed: &Changed,
) -> Result<Vec<Change<Object<R>>>, StoreError> {
    let ids = changed.of(R::KIND);
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(ids.len());
    let mut found = HashSet::new();
    for row in fetch_ids(store, R::KIND, R::REFS, "id", &ids).await? {
        let obj: Object<R> = Store::object_from_row(&row)?;
        found.insert(obj.id);
        out.push(Change::Upsert(obj));
    }
    out.extend(
        ids.into_iter()
            .filter(|id| !found.contains(id))
            .map(Change::Remove),
    );
    Ok(out)
}

async fn fetch_patch(
    store: &Store,
    keyring: &Keyring,
    base: &Snapshot,
    changed: &Changed,
    version: i64,
) -> Result<SnapshotPatch, StoreError> {
    let mut patch = SnapshotPatch {
        version,
        organizations: changes_of::<OrganizationSpec>(store, changed).await?,
        teams: changes_of::<TeamSpec>(store, changed).await?,
        model_groups: changes_of::<ModelGroupSpec>(store, changed).await?,
        deployments: changes_of::<DeploymentSpec>(store, changed).await?,
        prices: changes_of::<PriceSpec>(store, changed).await?,
        plugin_bindings: changes_of::<PluginBindingSpec>(store, changed).await?,
        mcp_servers: changes_of::<McpServerSpec>(store, changed).await?,
        mcp_grants: changes_of::<McpGrantSpec>(store, changed).await?,
        tool_policies: changes_of::<ToolPolicySpec>(store, changed).await?,
        ..SnapshotPatch::default()
    };

    // Keys: geänderte plus alle Keys von Teams, die in eine andere Organisation umgezogen sind
    // (`ON UPDATE CASCADE` ändert deren Organisation ohne eigenen Audit-Eintrag). Andere
    // Team-/Organisationsänderungen bindet `apply_patch` im Speicher neu.
    let key_ids = changed.of(ResourceKind::ApiKey);
    let team_ids: Vec<Id> = patch
        .teams
        .iter()
        .filter_map(|c| match c {
            Change::Upsert(t) => base
                .teams
                .get(&t.id)
                .filter(|old| old.spec.organization != t.spec.organization)
                .map(|_| t.id),
            Change::Remove(_) => None,
        })
        .collect();
    let mut seen = HashSet::new();
    let mut rows = fetch_ids(
        store,
        ResourceKind::ApiKey,
        ApiKeySpec::REFS,
        "id",
        &key_ids,
    )
    .await?;
    rows.extend(
        fetch_ids(
            store,
            ResourceKind::ApiKey,
            ApiKeySpec::REFS,
            "team_id",
            &team_ids,
        )
        .await?,
    );
    for row in &rows {
        let (key, hash) = api_key_from_row(row)?;
        if seen.insert(key.id) {
            patch.keys.push(Change::Upsert((hash, key)));
        }
    }
    patch.keys.extend(
        key_ids
            .into_iter()
            .filter(|id| !seen.contains(id))
            .map(Change::Remove),
    );

    // Zugangsdaten (nur die geänderten entschlüsseln).
    let cred_ids = changed.of(ResourceKind::ProviderCredential);
    let mut found = HashSet::new();
    for row in fetch_ids(
        store,
        ResourceKind::ProviderCredential,
        &[],
        "id",
        &cred_ids,
    )
    .await?
    {
        let (info, data) = decrypt_row(keyring, &row)?;
        found.insert(info.id);
        patch.credentials.push(Change::Upsert((info, data)));
    }
    patch.credentials.extend(
        cred_ids
            .into_iter()
            .filter(|id| !found.contains(id))
            .map(Change::Remove),
    );

    // Geheimnisse (F149, nur die geänderten entschlüsseln).
    let secret_ids = changed.of(ResourceKind::Secret);
    let mut found = HashSet::new();
    for row in fetch_ids(store, ResourceKind::Secret, &[], "id", &secret_ids).await? {
        let (info, data) = crate::repo::secret::decrypt_row(keyring, &row)?;
        found.insert(info.id);
        patch.secrets.push(Change::Upsert((info, data)));
    }
    patch.secrets.extend(
        secret_ids
            .into_iter()
            .filter(|id| !found.contains(id))
            .map(Change::Remove),
    );

    // Gelöschte Ziele löschen Bindungen/Freigaben ohne eigenen Audit-Eintrag mit.
    let target_deleted = patch
        .organizations
        .iter()
        .any(|c| matches!(c, Change::Remove(_)))
        || patch.teams.iter().any(|c| matches!(c, Change::Remove(_)))
        || patch.keys.iter().any(|c| matches!(c, Change::Remove(_)))
        || patch
            .model_groups
            .iter()
            .any(|c| matches!(c, Change::Remove(_)))
        || patch
            .mcp_servers
            .iter()
            .any(|c| matches!(c, Change::Remove(_)));
    if target_deleted {
        patch.replace_plugin_bindings = Some(store.list::<PluginBindingSpec>().await?);
        patch.replace_mcp_grants = Some(store.list::<McpGrantSpec>().await?);
        patch.replace_tool_policies = Some(store.list::<ToolPolicySpec>().await?);
    }
    Ok(patch)
}
