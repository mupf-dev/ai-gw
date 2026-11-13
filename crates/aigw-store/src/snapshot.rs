// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Aufbau des Konfigurations-Snapshots aus der Datenbank.

use std::collections::HashMap;
use std::sync::Arc;

use aigw_core::resource::{
    ApiKeySpec, ApiKeyState, DeploymentSpec, McpGrantSpec, McpServerSpec, ModelGroupSpec,
    OrganizationSpec, PluginBindingSpec, PriceSpec, ResourceKind, ScopeLevel, TeamSpec,
};
use aigw_core::snapshot::{DeploymentCtx, KeyCtx, McpServerCtx, ModelGroupCtx, Snapshot};

use crate::crypto::Keyring;
use crate::repo::Resource as _;
use crate::repo::api_key_from_row;
use crate::{Store, StoreError};

/// Höchstzahl Versuche, wenn sich die Konfiguration während des Ladens ändert.
const MAX_ATTEMPTS: usize = 5;

/// Lädt einen vollständigen, in sich konsistenten Snapshot.
///
/// Konsistenz: Die Version wird vor und nach dem Lesen geprüft; ändert sie sich dazwischen,
/// wird neu geladen (optimistisch, ohne lange Lesetransaktion).
pub async fn load_snapshot(store: &Store, keyring: &Keyring) -> Result<Snapshot, StoreError> {
    for _ in 0..MAX_ATTEMPTS {
        let version = store.config_version().await?;
        let snap = build(store, keyring, version).await?;
        if store.config_version().await? == version {
            return Ok(snap);
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

fn level_rank(l: ScopeLevel) -> u8 {
    match l {
        ScopeLevel::Global => 0,
        ScopeLevel::Organization => 1,
        ScopeLevel::Team => 2,
        ScopeLevel::ModelGroup => 3,
        ScopeLevel::ApiKey => 4,
    }
}

async fn build(store: &Store, keyring: &Keyring, version: i64) -> Result<Snapshot, StoreError> {
    let mut snap = Snapshot {
        version,
        ..Snapshot::default()
    };

    // Mandanten
    let mut orgs_by_name = HashMap::new();
    for o in store.list::<OrganizationSpec>().await? {
        let o = Arc::new(o);
        orgs_by_name.insert(o.metadata.name.clone(), Arc::clone(&o));
        snap.organizations.insert(o.id, o);
    }
    let mut teams_by_name = HashMap::new();
    for t in store.list::<TeamSpec>().await? {
        let t = Arc::new(t);
        teams_by_name.insert(t.metadata.name.clone(), Arc::clone(&t));
        snap.teams.insert(t.id, t);
    }

    // Keys (nur aktive; Ablauf prüft der Hot Path pro Anfrage)
    let rows = store
        .fetch_rows(
            ResourceKind::ApiKey,
            ApiKeySpec::REFS,
            "WHERE t.state = $1",
            Some(ApiKeyState::Active.as_str().to_owned()),
        )
        .await?;
    for row in &rows {
        let (key, hash) = api_key_from_row(row)?;
        let team = key
            .spec
            .team
            .as_ref()
            .and_then(|n| teams_by_name.get(n))
            .cloned();
        let organization = team
            .as_ref()
            .and_then(|t| orgs_by_name.get(&t.spec.organization))
            .cloned();
        let expires_at = key.spec.expires_at;
        snap.keys.insert(
            hash,
            Arc::new(KeyCtx {
                key,
                team,
                organization,
                expires_at,
            }),
        );
    }

    // Zugangsdaten
    let mut creds = HashMap::new();
    for (name, (info, data)) in store.decrypt_credentials(keyring).await? {
        creds.insert(name.clone(), Arc::new(data));
        snap.credentials.insert(name, info);
    }
    snap.credential_data.clone_from(&creds);

    // Modellgruppen + Deployments
    let mut deployments: HashMap<String, Vec<DeploymentCtx>> = HashMap::new();
    for d in store.list::<DeploymentSpec>().await? {
        if !d.spec.enabled {
            continue;
        }
        let credential = d
            .spec
            .credential
            .as_ref()
            .and_then(|c| creds.get(c))
            .cloned();
        deployments
            .entry(d.spec.model_group.clone())
            .or_default()
            .push(DeploymentCtx {
                deployment: d,
                credential,
            });
    }
    for g in store.list::<ModelGroupSpec>().await? {
        let mut deps = deployments.remove(&g.metadata.name).unwrap_or_default();
        deps.sort_by(|a, b| {
            (a.deployment.spec.priority, &a.deployment.metadata.name)
                .cmp(&(b.deployment.spec.priority, &b.deployment.metadata.name))
        });
        let ctx = Arc::new(ModelGroupCtx {
            group: g,
            deployments: deps,
        });
        snap.model_groups
            .insert(ctx.group.metadata.name.clone(), Arc::clone(&ctx));
        for a in &ctx.group.spec.aliases {
            snap.model_groups.insert(a.clone(), Arc::clone(&ctx));
        }
    }

    // Preise
    for p in store.list::<PriceSpec>().await? {
        snap.prices
            .entry((p.spec.provider, p.spec.model.clone()))
            .or_default()
            .push(Arc::new(p));
    }
    for list in snap.prices.values_mut() {
        list.sort_by_key(|p| p.spec.valid_from);
    }

    // Plugins
    let mut bindings: Vec<_> = store
        .list::<PluginBindingSpec>()
        .await?
        .into_iter()
        .filter(|b| b.spec.enabled)
        .map(Arc::new)
        .collect();
    bindings.sort_by(|a, b| {
        (
            level_rank(a.spec.scope.level),
            a.spec.order,
            &a.metadata.name,
        )
            .cmp(&(
                level_rank(b.spec.scope.level),
                b.spec.order,
                &b.metadata.name,
            ))
    });
    snap.plugin_bindings = bindings;

    // MCP
    for s in store.list::<McpServerSpec>().await? {
        if s.spec.enabled {
            let credential = s
                .spec
                .credential
                .as_ref()
                .and_then(|c| creds.get(c))
                .cloned();
            snap.mcp_servers.insert(
                s.metadata.name.clone(),
                Arc::new(McpServerCtx {
                    server: s,
                    credential,
                }),
            );
        }
    }
    snap.mcp_grants = store
        .list::<McpGrantSpec>()
        .await?
        .into_iter()
        .map(Arc::new)
        .collect();

    Ok(snap)
}
