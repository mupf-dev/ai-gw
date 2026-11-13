// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Anwenden eines Plans in **einer** Transaktion.
//!
//! Reihenfolge: alle Anlagen/Änderungen in Abhängigkeitsreihenfolge ([`ResourceKind::ALL`]),
//! danach alle Löschungen in umgekehrter Reihenfolge. Aliasse geänderter und gelöschter
//! Modellgruppen werden vorab freigegeben, damit Aliasse zwischen Gruppen wandern können.
//! Schlägt ein Schritt fehl, wird die Transaktion zurückgerollt – es bleibt nichts zurück.

use aigw_core::resource::{
    ApiKey, ApiKeySpec, ApiKeyState, ApiKeyStatus, Manifest, Object, ObjectMeta,
    ProviderCredential, ProviderCredentialInfo, ProviderCredentialSpec, ResourceKind, ResourceSpec,
    SecretRef, new_id,
};
use serde::Serialize;
use utoipa::ToSchema;

use super::document::Document;
use super::plan::{Plan, Prepared, Resolved, Step, prepare};
use super::secrets::SecretSource;
use super::state::{self, Current, Extra};
use super::{ApplyOptions, GitopsError};
use crate::crypto::Keyring;
use crate::db::{Q, Val};
use crate::keys::{hash_api_key, parse_prefix};
use crate::repo::credential::{refs_of, seal_spec, sealed_cols};
use crate::repo::{Action, AdminTx, Col};
use crate::{PlainResource, Resource, Store, StoreError};

/// Ergebnis von [`apply`].
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApplyReport {
    /// Plan (bei `applied` genau das, was geschrieben wurde).
    pub plan: Plan,
    /// Wurde geschrieben? (`false` bei Trockenlauf oder ohne Änderungen)
    pub applied: bool,
    /// `config_version` danach.
    pub config_version: i64,
}

/// Berechnet den Plan (Trockenlauf) nach vollständiger Validierung.
pub async fn plan(
    store: &Store,
    keyring: &Keyring,
    docs: &[Document],
    opts: &ApplyOptions,
    secrets: &dyn SecretSource,
) -> Result<Plan, GitopsError> {
    let state = state::load(store, Some(keyring)).await?;
    Ok(prepare(&state, docs, opts, secrets)?.plan)
}

/// Validiert, plant und wendet an (außer bei `opts.dry_run`). Alles oder nichts.
pub async fn apply(
    store: &Store,
    keyring: &Keyring,
    docs: &[Document],
    opts: &ApplyOptions,
    secrets: &dyn SecretSource,
) -> Result<ApplyReport, GitopsError> {
    let state = state::load(store, Some(keyring)).await?;
    let Prepared { plan, steps } = prepare(&state, docs, opts, secrets)?;
    if opts.dry_run || !plan.has_changes() {
        return Ok(ApplyReport {
            plan,
            applied: false,
            config_version: state.version,
        });
    }

    let mut tx = AdminTx::begin(store, &opts.actor).await?;
    if tx.version != state.version + 1 {
        // Zwischen Lesen und Schreiben hat jemand anderes geändert: Plan wäre veraltet.
        // `tx` wird ohne Commit verworfen (Rollback).
        return Err(GitopsError::Concurrent {
            planned: state.version,
            found: tx.version - 1,
        });
    }

    // Aliasse geänderter und gelöschter Modellgruppen freigeben (werden neu geschrieben).
    for step in &steps {
        let id = match step {
            Step::Upsert {
                existing: Some(c),
                manifest:
                    Manifest {
                        spec: ResourceSpec::ModelGroup(_),
                        ..
                    },
                ..
            } => c.id,
            Step::Delete {
                kind: ResourceKind::ModelGroup,
                current,
                ..
            } => current.id,
            _ => continue,
        };
        Q::new("DELETE FROM model_alias WHERE model_group_id = $1")
            .bind(Val::uuid(id))
            .execute(&mut tx.tx)
            .await
            .map_err(StoreError::from)?;
    }

    for step in steps {
        match step {
            Step::Upsert {
                manifest,
                existing,
                resolved,
            } => upsert(&mut tx, keyring, manifest, existing.as_ref(), resolved).await?,
            Step::Delete {
                kind,
                name,
                current,
            } => {
                if kind == ResourceKind::User {
                    tx.end_user_sessions(current.id, &name, "user-deleted")
                        .await?;
                }
                tx.delete_row(kind, current.id).await?;
                tx.audit(
                    Action::Delete,
                    kind,
                    Some(current.id),
                    &name,
                    Some(&current.audit),
                    None::<&()>,
                )
                .await?;
            }
        }
    }
    let version = tx.commit().await?;
    Ok(ApplyReport {
        plan,
        applied: true,
        config_version: version,
    })
}

async fn upsert(
    tx: &mut AdminTx,
    keyring: &Keyring,
    manifest: Manifest,
    existing: Option<&Current>,
    resolved: Resolved,
) -> Result<(), StoreError> {
    let meta = manifest.metadata;
    match manifest.spec {
        ResourceSpec::Organization(s) => upsert_plain(tx, existing, meta, s).await,
        ResourceSpec::Team(s) => upsert_plain(tx, existing, meta, s).await,
        ResourceSpec::User(s) => upsert_plain(tx, existing, meta, s).await,
        ResourceSpec::ModelGroup(s) => upsert_plain(tx, existing, meta, s).await,
        ResourceSpec::Deployment(s) => upsert_plain(tx, existing, meta, s).await,
        ResourceSpec::Price(s) => upsert_plain(tx, existing, meta, s).await,
        ResourceSpec::PluginBinding(s) => upsert_plain(tx, existing, meta, s).await,
        ResourceSpec::McpServer(s) => upsert_plain(tx, existing, meta, s).await,
        ResourceSpec::McpGrant(s) => upsert_plain(tx, existing, meta, s).await,
        ResourceSpec::ProviderCredential(s) => {
            let Resolved::Credential(values) = resolved else {
                return Err(StoreError::Invalid(format!(
                    "Zugangsdaten {:?}: Werte nicht aufgelöst",
                    meta.name
                )));
            };
            upsert_credential(tx, keyring, existing, meta, s, values).await
        }
        ResourceSpec::ApiKey(s) => {
            let secret = match resolved {
                Resolved::ApiKey(s) => s,
                _ => None,
            };
            upsert_api_key(tx, existing, meta, s, secret).await
        }
    }
}

async fn write_row(
    tx: &mut AdminTx,
    kind: ResourceKind,
    existing: Option<&Current>,
    meta: &ObjectMeta,
    cols: Vec<(&'static str, Val)>,
) -> Result<(aigw_core::resource::Id, Action), StoreError> {
    Ok(match existing {
        Some(c) => {
            tx.update_row(kind, c.id, meta, cols).await?;
            (c.id, Action::Update)
        }
        None => {
            let id = new_id();
            tx.insert_row(kind, id, meta, cols).await?;
            (id, Action::Create)
        }
    })
}

async fn audit<A: Serialize>(
    tx: &mut AdminTx,
    action: Action,
    kind: ResourceKind,
    id: aigw_core::resource::Id,
    name: &str,
    existing: Option<&Current>,
    after: &A,
) -> Result<(), StoreError> {
    tx.audit(
        action,
        kind,
        Some(id),
        name,
        existing.map(|c| &c.audit),
        Some(after),
    )
    .await
}

async fn upsert_plain<R: PlainResource>(
    tx: &mut AdminTx,
    existing: Option<&Current>,
    meta: ObjectMeta,
    spec: R,
) -> Result<(), StoreError> {
    let cols = tx.resolve_cols(spec.to_columns()?).await?;
    let (id, action) = write_row(tx, R::KIND, existing, &meta, cols).await?;
    if let Some(aliases) = spec.aliases() {
        tx.write_aliases(id, &meta.name, aliases).await?;
    }
    let name = meta.name.clone();
    let after = Object {
        id,
        metadata: meta,
        spec,
    };
    audit(tx, action, R::KIND, id, &name, existing, &after).await?;
    // Rollenänderung per GitOps beendet die Sessions des Nutzers (wie über die Admin-API).
    if R::KIND == ResourceKind::User
        && let Some(c) = existing
        && crate::repo::audit::user_change_ends_sessions(
            &c.audit["spec"],
            &serde_json::to_value(&after.spec)?,
        )
    {
        tx.end_user_sessions(id, &name, "roles-changed").await?;
    }
    Ok(())
}

async fn upsert_credential(
    tx: &mut AdminTx,
    keyring: &Keyring,
    existing: Option<&Current>,
    meta: ObjectMeta,
    spec: ProviderCredentialSpec,
    values: std::collections::BTreeMap<String, aigw_core::resource::SecretString>,
) -> Result<(), StoreError> {
    let kind = ResourceKind::ProviderCredential;
    let id = existing.map_or_else(new_id, |c| c.id);
    // Bereits aufgelöste Werte; `seal_spec` verschlüsselt sie gebunden an die ID.
    let plain = ProviderCredentialSpec {
        provider: spec.provider,
        data: values
            .iter()
            .map(|(k, v)| (k.clone(), SecretRef::value(v.expose())))
            .collect(),
    };
    let (fields, sealed) = seal_spec(keyring, id, &plain)?;
    let key_id = sealed.key_id.clone();
    // F54: valueFrom-Verweise des Manifests mitspeichern (originalgetreuer Export).
    let cols = sealed_cols(plain.provider, &fields, &refs_of(&spec), sealed)?;
    let action = match existing {
        Some(_) => {
            tx.update_row(kind, id, &meta, cols).await?;
            Action::Update
        }
        None => {
            tx.insert_row(kind, id, &meta, cols).await?;
            Action::Create
        }
    };
    let name = meta.name.clone();
    let after = ProviderCredential {
        id,
        metadata: meta,
        spec: ProviderCredentialInfo {
            provider: spec.provider,
            fields,
            key_id,
        },
    };
    audit(tx, action, kind, id, &name, existing, &after).await
}

async fn upsert_api_key(
    tx: &mut AdminTx,
    existing: Option<&Current>,
    meta: ObjectMeta,
    spec: ApiKeySpec,
    secret: Option<aigw_core::resource::SecretString>,
) -> Result<(), StoreError> {
    let kind = ResourceKind::ApiKey;
    tx.check_key_scope(&spec).await?;
    let mut cols = spec.to_columns()?;
    let mut status = match existing.map(|c| &c.extra) {
        Some(Extra::ApiKey { status, .. }) => status.clone(),
        _ => ApiKeyStatus {
            prefix: String::new(),
            state: ApiKeyState::Active,
            last_used_at: None,
        },
    };
    match &secret {
        Some(s) => {
            let prefix = parse_prefix(s.expose())
                .ok_or_else(|| StoreError::Invalid("ungültiges Key-Format".into()))?
                .to_owned();
            cols.push(("prefix", Col::text(prefix.clone())));
            cols.push(("key_hash", Col::text(hash_api_key(s.expose()).to_hex())));
            status.prefix = prefix;
        }
        None if existing.is_none() => {
            return Err(StoreError::Invalid(format!(
                "API-Key {:?}: neuer Key braucht spec.secret.valueFrom",
                meta.name
            )));
        }
        None => {}
    }
    if existing.is_none() {
        cols.push(("state", Col::text(ApiKeyState::Active.as_str())));
    }
    let cols = tx.resolve_cols(cols).await?;
    let (id, action) = write_row(tx, kind, existing, &meta, cols).await?;
    let name = meta.name.clone();
    let after = ApiKey {
        id,
        metadata: meta,
        spec,
        status,
    };
    audit(tx, action, kind, id, &name, existing, &after).await
}
