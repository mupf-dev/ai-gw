// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Zusammenbau des Snapshots aus Rohdaten und inkrementelle Änderungen (F16, AIGW-75).
//!
//! * [`SnapshotParts`] sind die Rohdaten einer Version (so, wie sie aus der Datenbank oder dem
//!   Platten-Cache kommen). [`Snapshot::from_parts`] baut daraus alle Indizes;
//!   [`Snapshot::to_parts`] ist das Gegenstück (für den Platten-Cache, F14).
//! * [`SnapshotPatch`] beschreibt die Änderungen zwischen zwei Versionen (nur die geänderten
//!   Objekte, frisch aus der Datenbank). [`Snapshot::apply_patch`] arbeitet sie gezielt in einen
//!   neuen Snapshot ein und hält dabei alle abgeleiteten Indizes konsistent: Key-Kontexte
//!   geänderter Teams/Organisationen, Modellgruppen samt Aliassen und Deployments, aufgelöste
//!   Zugangsdaten in Deployments und MCP-Servern.
//!
//! Invariante (Property-Test in `aigw-store`): `from_parts(DB@v2) == from_parts(DB@v1)
//! .apply_patch(Δ v1→v2)`. Deshalb laufen beide Wege über dieselben Hilfsfunktionen
//! (Filter „aktiv/aktiviert“, Sortierungen, Kontextaufbau).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::resource::{
    ApiKey, ApiKeyState, CredentialData, Deployment, Id, McpGrant, McpServer, ModelGroup, Object,
    Organization, PluginBinding, Price, ProviderCredential, ScopeLevel, Secret, Team,
};

use super::{DeploymentCtx, KeyCtx, KeyHash, McpServerCtx, ModelGroupCtx, Snapshot};
use crate::tools::{ToolPolicy, ToolPolicyIndex};
use crate::wasm::WasmModule;

/// Rohdaten einer Konfigurationsversion.
///
/// Inaktive Keys, deaktivierte Deployments/Plugin-Bindungen/MCP-Server dürfen enthalten sein;
/// [`Snapshot::from_parts`] filtert sie heraus.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SnapshotParts {
    /// `config_version`.
    pub version: i64,
    /// Organisationen.
    pub organizations: Vec<Organization>,
    /// Teams.
    pub teams: Vec<Team>,
    /// API-Keys mit Hash.
    pub keys: Vec<(KeyHash, ApiKey)>,
    /// Zugangsdaten (Metadaten + entschlüsselte Werte).
    pub credentials: Vec<(ProviderCredential, CredentialData)>,
    /// Geheimnisse (`Secret`, F149; Metadaten + entschlüsselte Werte).
    pub secrets: Vec<(Secret, CredentialData)>,
    /// Modellgruppen.
    pub model_groups: Vec<ModelGroup>,
    /// Deployments.
    pub deployments: Vec<Deployment>,
    /// Preise.
    pub prices: Vec<Price>,
    /// Plugin-Bindungen.
    pub plugin_bindings: Vec<PluginBinding>,
    /// MCP-Server.
    pub mcp_servers: Vec<McpServer>,
    /// MCP-Freigaben.
    pub mcp_grants: Vec<McpGrant>,
    /// Tool-Richtlinien (AIGW-87).
    pub tool_policies: Vec<ToolPolicy>,
    /// WASM-Module (AIGW-63).
    pub wasm_modules: Vec<WasmModule>,
}

/// Änderung eines Objekts: neuer Stand (wie in der Datenbank, auch inaktiv/deaktiviert) oder
/// gelöscht (nur noch die ID ist bekannt).
#[derive(Debug, Clone, PartialEq)]
pub enum Change<T> {
    /// Angelegt oder geändert.
    Upsert(T),
    /// Gelöscht.
    Remove(Id),
}

/// Änderungen zwischen der Version des Basis-Snapshots und [`SnapshotPatch::version`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SnapshotPatch {
    /// Zielversion.
    pub version: i64,
    /// Organisationen.
    pub organizations: Vec<Change<Organization>>,
    /// Teams.
    pub teams: Vec<Change<Team>>,
    /// API-Keys (geänderte Keys sowie alle Keys geänderter Teams).
    pub keys: Vec<Change<(KeyHash, ApiKey)>>,
    /// Zugangsdaten.
    pub credentials: Vec<Change<(ProviderCredential, CredentialData)>>,
    /// Geheimnisse (F149).
    pub secrets: Vec<Change<(Secret, CredentialData)>>,
    /// Modellgruppen.
    pub model_groups: Vec<Change<ModelGroup>>,
    /// Deployments.
    pub deployments: Vec<Change<Deployment>>,
    /// Preise.
    pub prices: Vec<Change<Price>>,
    /// Plugin-Bindungen (einzelne Änderungen).
    pub plugin_bindings: Vec<Change<PluginBinding>>,
    /// Alle Plugin-Bindungen neu (ersetzt die Liste; z. B. nach dem Löschen eines Ziels, das
    /// Bindungen ohne eigenen Audit-Eintrag mitlöscht).
    pub replace_plugin_bindings: Option<Vec<PluginBinding>>,
    /// MCP-Server.
    pub mcp_servers: Vec<Change<McpServer>>,
    /// MCP-Freigaben (einzelne Änderungen).
    pub mcp_grants: Vec<Change<McpGrant>>,
    /// Alle MCP-Freigaben neu (wie `replace_plugin_bindings`).
    pub replace_mcp_grants: Option<Vec<McpGrant>>,
    /// Tool-Richtlinien (einzelne Änderungen, AIGW-87).
    pub tool_policies: Vec<Change<ToolPolicy>>,
    /// Alle Tool-Richtlinien neu (wie `replace_plugin_bindings`).
    pub replace_tool_policies: Option<Vec<ToolPolicy>>,
    /// WASM-Module (AIGW-63).
    pub wasm_modules: Vec<Change<WasmModule>>,
}

impl SnapshotPatch {
    /// Anzahl der Einzeländerungen (ohne ersetzte Listen).
    pub fn len(&self) -> usize {
        self.organizations.len()
            + self.teams.len()
            + self.keys.len()
            + self.credentials.len()
            + self.secrets.len()
            + self.model_groups.len()
            + self.deployments.len()
            + self.prices.len()
            + self.plugin_bindings.len()
            + self.mcp_servers.len()
            + self.mcp_grants.len()
            + self.tool_policies.len()
            + self.wasm_modules.len()
    }

    /// Keine Änderungen?
    pub fn is_empty(&self) -> bool {
        self.len() == 0
            && self.replace_plugin_bindings.is_none()
            && self.replace_mcp_grants.is_none()
            && self.replace_tool_policies.is_none()
    }
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

fn sort_bindings(b: &mut [Arc<PluginBinding>]) {
    b.sort_by(|a, b| {
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
}

fn sort_grants(g: &mut [Arc<McpGrant>]) {
    g.sort_by(|a, b| a.metadata.name.cmp(&b.metadata.name));
}

/// Tool-Richtlinien sortieren und den Index neu bauen (AIGW-87).
fn set_tool_policies(snap: &mut Snapshot, mut list: Vec<Arc<ToolPolicy>>) {
    list.sort_by(|a, b| a.metadata.name.cmp(&b.metadata.name));
    snap.tool_index = Arc::new(ToolPolicyIndex::new(list.iter().map(|p| &**p)));
    snap.tool_policies = list;
}

fn sort_deployments(d: &mut [DeploymentCtx]) {
    d.sort_by(|a, b| {
        (a.deployment.spec.priority, &a.deployment.metadata.name)
            .cmp(&(b.deployment.spec.priority, &b.deployment.metadata.name))
    });
}

/// Nur aktive Keys landen im Snapshot; `last_used_at` wird ohne Versionserhöhung geschrieben
/// (Log-Writer) und gehört deshalb nicht in den Snapshot (sonst wäre er nie reproduzierbar).
fn normalize_key(mut key: ApiKey) -> Option<ApiKey> {
    if key.status.state != ApiKeyState::Active {
        return None;
    }
    key.status.last_used_at = None;
    Some(key)
}

fn key_ctx(
    key: ApiKey,
    teams: &HashMap<&str, &Arc<Team>>,
    orgs: &HashMap<&str, &Arc<Organization>>,
) -> KeyCtx {
    let team = key
        .spec
        .team
        .as_deref()
        .and_then(|n| teams.get(n))
        .map(|t| Arc::clone(t));
    // F3: jeder Key hat eine Organisation (mit Team: die des Teams); Keys ohne Team erben
    // damit Limits, Budgets und Plugins der Organisation.
    let organization = orgs
        .get(key.spec.organization.as_str())
        .map(|o| Arc::clone(o));
    let expires_at = key.spec.expires_at;
    KeyCtx {
        key,
        team,
        organization,
        expires_at,
    }
}

fn deployment_ctx(
    deployment: Deployment,
    creds: &HashMap<String, Arc<CredentialData>>,
) -> DeploymentCtx {
    let credential = deployment
        .spec
        .credential
        .as_ref()
        .and_then(|c| creds.get(c))
        .cloned();
    DeploymentCtx {
        deployment,
        credential,
    }
}

/// MCP-Server mit aufgelösten Zugangsdaten: bevorzugt ein `Secret` (F149), sonst (älter) eine
/// `ProviderCredential`.
fn mcp_ctx(
    server: McpServer,
    creds: &HashMap<String, Arc<CredentialData>>,
    secrets: &HashMap<String, Arc<CredentialData>>,
) -> McpServerCtx {
    let credential = match (&server.spec.secret, &server.spec.credential) {
        (Some(n), _) => secrets.get(n).cloned(),
        (None, Some(c)) => creds.get(c).cloned(),
        (None, None) => None,
    };
    McpServerCtx { server, credential }
}

fn insert_group(map: &mut HashMap<String, Arc<ModelGroupCtx>>, ctx: ModelGroupCtx) {
    let ctx = Arc::new(ctx);
    map.insert(ctx.group.metadata.name.clone(), Arc::clone(&ctx));
    for a in &ctx.group.spec.aliases {
        map.insert(a.clone(), Arc::clone(&ctx));
    }
}

fn by_name<T>(map: &HashMap<Id, Arc<Object<T>>>) -> HashMap<&str, &Arc<Object<T>>> {
    map.values()
        .map(|o| (o.metadata.name.as_str(), o))
        .collect()
}

impl Snapshot {
    /// Baut einen Snapshot mit allen Indizes aus Rohdaten.
    pub fn from_parts(parts: SnapshotParts) -> Self {
        let mut snap = Snapshot {
            version: parts.version,
            ..Snapshot::default()
        };
        for o in parts.organizations {
            snap.organizations.insert(o.id, Arc::new(o));
        }
        for t in parts.teams {
            snap.teams.insert(t.id, Arc::new(t));
        }
        {
            let orgs = by_name(&snap.organizations);
            let teams = by_name(&snap.teams);
            snap.keys.reserve(parts.keys.len());
            for (hash, key) in parts.keys {
                if let Some(key) = normalize_key(key) {
                    snap.key_ids.insert(key.id, hash);
                    snap.keys
                        .insert(hash, Arc::new(key_ctx(key, &teams, &orgs)));
                }
            }
        }
        snap.rebuild_workload_index();
        for (info, data) in parts.credentials {
            let name = info.metadata.name.clone();
            snap.credential_data.insert(name.clone(), Arc::new(data));
            snap.credentials.insert(name, info);
        }
        for (info, data) in parts.secrets {
            let name = info.metadata.name.clone();
            snap.secret_data.insert(name.clone(), Arc::new(data));
            snap.secrets.insert(name, info);
        }

        let mut deployments: HashMap<String, Vec<DeploymentCtx>> = HashMap::new();
        for d in parts.deployments {
            if d.spec.enabled {
                deployments
                    .entry(d.spec.model_group.clone())
                    .or_default()
                    .push(deployment_ctx(d, &snap.credential_data));
            }
        }
        for g in parts.model_groups {
            let mut deps = deployments.remove(&g.metadata.name).unwrap_or_default();
            sort_deployments(&mut deps);
            insert_group(
                &mut snap.model_groups,
                ModelGroupCtx {
                    group: g,
                    deployments: deps,
                },
            );
        }

        for p in parts.prices {
            snap.prices
                .entry((p.spec.provider, p.spec.model.clone()))
                .or_default()
                .push(Arc::new(p));
        }
        for list in snap.prices.values_mut() {
            list.sort_by_key(|p| p.spec.valid_from);
        }

        snap.plugin_bindings = parts
            .plugin_bindings
            .into_iter()
            .filter(|b| b.spec.enabled)
            .map(Arc::new)
            .collect();
        sort_bindings(&mut snap.plugin_bindings);

        for s in parts.mcp_servers {
            if s.spec.enabled {
                let ctx = mcp_ctx(s, &snap.credential_data, &snap.secret_data);
                snap.mcp_servers
                    .insert(ctx.server.metadata.name.clone(), Arc::new(ctx));
            }
        }
        snap.mcp_grants = parts.mcp_grants.into_iter().map(Arc::new).collect();
        sort_grants(&mut snap.mcp_grants);
        let tools = parts
            .tool_policies
            .into_iter()
            .filter(|p| p.spec.enabled)
            .map(Arc::new)
            .collect();
        set_tool_policies(&mut snap, tools);
        for m in parts.wasm_modules {
            snap.wasm_modules.insert(m.name.clone(), Arc::new(m));
        }
        snap
    }

    /// Rohdaten dieses Snapshots (nur, was er enthält: aktive Keys, aktivierte Objekte).
    ///
    /// `Snapshot::from_parts(s.to_parts()) == s`. Reihenfolge: nach ID bzw. Hash sortiert.
    pub fn to_parts(&self) -> SnapshotParts {
        fn sorted<T: Clone>(it: impl Iterator<Item = T>, key: impl Fn(&T) -> Id) -> Vec<T> {
            let mut v: Vec<T> = it.collect();
            v.sort_by_key(|x| key(x));
            v
        }
        let groups = self.unique_groups();
        let mut keys: Vec<(KeyHash, ApiKey)> =
            self.keys.iter().map(|(h, c)| (*h, c.key.clone())).collect();
        keys.sort_by_key(|(h, _)| *h);
        SnapshotParts {
            version: self.version,
            organizations: sorted(self.organizations.values().map(|o| (**o).clone()), |o| o.id),
            teams: sorted(self.teams.values().map(|t| (**t).clone()), |t| t.id),
            keys,
            credentials: sorted(
                self.credentials.iter().map(|(name, info)| {
                    let data = self
                        .credential_data
                        .get(name)
                        .map(|d| (**d).clone())
                        .unwrap_or_default();
                    (info.clone(), data)
                }),
                |(i, _)| i.id,
            ),
            secrets: sorted(
                self.secrets.iter().map(|(name, info)| {
                    let data = self
                        .secret_data
                        .get(name)
                        .map(|d| (**d).clone())
                        .unwrap_or_default();
                    (info.clone(), data)
                }),
                |(i, _)| i.id,
            ),
            model_groups: sorted(groups.iter().map(|g| g.group.clone()), |g| g.id),
            deployments: sorted(
                groups
                    .iter()
                    .flat_map(|g| g.deployments.iter().map(|d| d.deployment.clone())),
                |d| d.id,
            ),
            prices: sorted(self.prices.values().flatten().map(|p| (**p).clone()), |p| {
                p.id
            }),
            plugin_bindings: sorted(self.plugin_bindings.iter().map(|b| (**b).clone()), |b| b.id),
            mcp_servers: sorted(self.mcp_servers.values().map(|s| s.server.clone()), |s| {
                s.id
            }),
            mcp_grants: sorted(self.mcp_grants.iter().map(|g| (**g).clone()), |g| g.id),
            tool_policies: sorted(self.tool_policies.iter().map(|p| (**p).clone()), |p| p.id),
            wasm_modules: sorted(self.wasm_modules.values().map(|m| (**m).clone()), |m| m.id),
        }
    }

    /// Jede Modellgruppe genau einmal (die Map enthält sie auch unter ihren Aliassen).
    fn unique_groups(&self) -> Vec<&Arc<ModelGroupCtx>> {
        self.model_groups
            .iter()
            .filter(|(name, ctx)| **name == ctx.group.metadata.name)
            .map(|(_, ctx)| ctx)
            .collect()
    }

    /// Neuer Snapshot = dieser plus `patch` (F16). Unveränderte Teile werden geteilt (`Arc`).
    pub fn apply_patch(&self, patch: SnapshotPatch) -> Snapshot {
        let mut s = Snapshot {
            version: patch.version,
            ..Snapshot::default()
        };

        // Organisationen und Teams (Namen sind unveränderlich; geändert = vorher oder nachher).
        s.organizations = self.organizations.clone();
        let changed_orgs = apply_by_id(&mut s.organizations, patch.organizations, |o| o.id);
        s.teams = self.teams.clone();
        let changed_teams = apply_by_id(&mut s.teams, patch.teams, |t| t.id);

        // Keys: geänderte über den ID-Index ersetzen (ohne alle Keys zu durchlaufen); Kontexte
        // von Keys geänderter Teams/Organisationen neu binden (das durchläuft alle Keys).
        s.keys = self.keys.clone();
        s.key_ids = self.key_ids.clone();
        let patch_has_keys = !patch.keys.is_empty();
        if !patch.keys.is_empty() || !changed_orgs.is_empty() || !changed_teams.is_empty() {
            let orgs = by_name(&s.organizations);
            let teams = by_name(&s.teams);
            for c in &patch.keys {
                let id = match c {
                    Change::Upsert((_, k)) => k.id,
                    Change::Remove(id) => *id,
                };
                if let Some(hash) = s.key_ids.remove(&id) {
                    s.keys.remove(&hash);
                }
            }
            if !changed_orgs.is_empty() || !changed_teams.is_empty() {
                for ctx in s.keys.values_mut() {
                    let affected = changed_orgs.contains(&ctx.key.spec.organization)
                        || ctx
                            .key
                            .spec
                            .team
                            .as_ref()
                            .is_some_and(|t| changed_teams.contains(t));
                    if affected {
                        *ctx = Arc::new(key_ctx(ctx.key.clone(), &teams, &orgs));
                    }
                }
            }
            for c in patch.keys {
                if let Change::Upsert((hash, key)) = c
                    && let Some(key) = normalize_key(key)
                {
                    s.key_ids.insert(key.id, hash);
                    s.keys.insert(hash, Arc::new(key_ctx(key, &teams, &orgs)));
                }
            }
        }
        if patch_has_keys {
            s.rebuild_workload_index();
        } else {
            s.workloads = self.workloads.clone();
        }

        // Zugangsdaten.
        s.credentials = self.credentials.clone();
        s.credential_data = self.credential_data.clone();
        let mut changed_creds: HashSet<String> = HashSet::new();
        for c in patch.credentials {
            match c {
                Change::Upsert((info, data)) => {
                    let name = info.metadata.name.clone();
                    s.credential_data.insert(name.clone(), Arc::new(data));
                    s.credentials.insert(name.clone(), info);
                    changed_creds.insert(name);
                }
                Change::Remove(id) => {
                    if let Some(name) = s
                        .credentials
                        .iter()
                        .find(|(_, i)| i.id == id)
                        .map(|(n, _)| n.clone())
                    {
                        s.credentials.remove(&name);
                        s.credential_data.remove(&name);
                        changed_creds.insert(name);
                    }
                }
            }
        }

        // Geheimnisse (F149).
        s.secrets = self.secrets.clone();
        s.secret_data = self.secret_data.clone();
        let mut changed_secrets: HashSet<String> = HashSet::new();
        for c in patch.secrets {
            match c {
                Change::Upsert((info, data)) => {
                    let name = info.metadata.name.clone();
                    s.secret_data.insert(name.clone(), Arc::new(data));
                    s.secrets.insert(name.clone(), info);
                    changed_secrets.insert(name);
                }
                Change::Remove(id) => {
                    if let Some(name) = s
                        .secrets
                        .iter()
                        .find(|(_, i)| i.id == id)
                        .map(|(n, _)| n.clone())
                    {
                        s.secrets.remove(&name);
                        s.secret_data.remove(&name);
                        changed_secrets.insert(name);
                    }
                }
            }
        }

        // Modellgruppen + Deployments: betroffene Gruppen komplett neu zusammensetzen.
        s.model_groups = self.model_groups.clone();
        if !patch.model_groups.is_empty()
            || !patch.deployments.is_empty()
            || !changed_creds.is_empty()
        {
            let old_groups = self.unique_groups();
            let changed_deps: HashSet<Id> = patch
                .deployments
                .iter()
                .map(|c| match c {
                    Change::Upsert(d) => d.id,
                    Change::Remove(id) => *id,
                })
                .collect();
            let mut affected: HashSet<String> = HashSet::new();
            let mut new_groups: HashMap<String, ModelGroup> = HashMap::new();
            let mut removed_groups: HashSet<Id> = HashSet::new();
            for c in patch.model_groups {
                match c {
                    Change::Upsert(g) => {
                        affected.insert(g.metadata.name.clone());
                        new_groups.insert(g.metadata.name.clone(), g);
                    }
                    Change::Remove(id) => {
                        removed_groups.insert(id);
                    }
                }
            }
            for g in &old_groups {
                let touched = removed_groups.contains(&g.group.id)
                    || g.deployments.iter().any(|d| {
                        changed_deps.contains(&d.deployment.id)
                            || d.deployment
                                .spec
                                .credential
                                .as_ref()
                                .is_some_and(|c| changed_creds.contains(c))
                    });
                if touched {
                    affected.insert(g.group.metadata.name.clone());
                }
            }
            let mut new_deps: HashMap<String, Vec<Deployment>> = HashMap::new();
            for c in patch.deployments {
                if let Change::Upsert(d) = c {
                    affected.insert(d.spec.model_group.clone());
                    if d.spec.enabled {
                        new_deps
                            .entry(d.spec.model_group.clone())
                            .or_default()
                            .push(d);
                    }
                }
            }
            // Erst alle alten Einträge (Name + Aliasse) betroffener Gruppen entfernen, dann neu
            // einfügen – ein Alias kann dabei von einer Gruppe zur anderen wandern.
            let old_by_name: HashMap<&str, &Arc<ModelGroupCtx>> = old_groups
                .iter()
                .map(|g| (g.group.metadata.name.as_str(), *g))
                .collect();
            for name in &affected {
                if let Some(old) = old_by_name.get(name.as_str()) {
                    s.model_groups.remove(&old.group.metadata.name);
                    for a in &old.group.spec.aliases {
                        s.model_groups.remove(a);
                    }
                }
            }
            for name in &affected {
                let old = old_by_name.get(name.as_str());
                let group = match new_groups.remove(name) {
                    Some(g) => g,
                    None => match old {
                        Some(o) if !removed_groups.contains(&o.group.id) => o.group.clone(),
                        _ => continue,
                    },
                };
                let mut deps: Vec<DeploymentCtx> = old
                    .map(|o| {
                        o.deployments
                            .iter()
                            .filter(|d| !changed_deps.contains(&d.deployment.id))
                            .map(|d| deployment_ctx(d.deployment.clone(), &s.credential_data))
                            .collect()
                    })
                    .unwrap_or_default();
                deps.extend(
                    new_deps
                        .remove(name)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|d| deployment_ctx(d, &s.credential_data)),
                );
                sort_deployments(&mut deps);
                insert_group(
                    &mut s.model_groups,
                    ModelGroupCtx {
                        group,
                        deployments: deps,
                    },
                );
            }
        }

        // Preise.
        s.prices = self.prices.clone();
        if !patch.prices.is_empty() {
            let ids: HashSet<Id> = patch
                .prices
                .iter()
                .map(|c| match c {
                    Change::Upsert(p) => p.id,
                    Change::Remove(id) => *id,
                })
                .collect();
            for list in s.prices.values_mut() {
                list.retain(|p| !ids.contains(&p.id));
            }
            for c in patch.prices {
                if let Change::Upsert(p) = c {
                    s.prices
                        .entry((p.spec.provider, p.spec.model.clone()))
                        .or_default()
                        .push(Arc::new(p));
                }
            }
            s.prices.retain(|_, l| !l.is_empty());
            for list in s.prices.values_mut() {
                list.sort_by_key(|p| p.spec.valid_from);
            }
        }

        // Plugin-Bindungen.
        s.plugin_bindings = apply_list(
            &self.plugin_bindings,
            patch.replace_plugin_bindings,
            patch.plugin_bindings,
            |b| b.id,
            |b| b.spec.enabled,
        );
        sort_bindings(&mut s.plugin_bindings);

        // MCP-Server (inkl. neu aufgelöster Zugangsdaten).
        s.mcp_servers = self.mcp_servers.clone();
        if !patch.mcp_servers.is_empty() || !changed_creds.is_empty() || !changed_secrets.is_empty()
        {
            for c in patch.mcp_servers {
                let id = match &c {
                    Change::Upsert(m) => m.id,
                    Change::Remove(id) => *id,
                };
                s.mcp_servers.retain(|_, ctx| ctx.server.id != id);
                if let Change::Upsert(m) = c
                    && m.spec.enabled
                {
                    let ctx = mcp_ctx(m, &s.credential_data, &s.secret_data);
                    s.mcp_servers
                        .insert(ctx.server.metadata.name.clone(), Arc::new(ctx));
                }
            }
            if !changed_creds.is_empty() || !changed_secrets.is_empty() {
                for ctx in s.mcp_servers.values_mut() {
                    let spec = &ctx.server.spec;
                    if spec
                        .credential
                        .as_ref()
                        .is_some_and(|c| changed_creds.contains(c))
                        || spec
                            .secret
                            .as_ref()
                            .is_some_and(|c| changed_secrets.contains(c))
                    {
                        *ctx = Arc::new(mcp_ctx(
                            ctx.server.clone(),
                            &s.credential_data,
                            &s.secret_data,
                        ));
                    }
                }
            }
        }

        // MCP-Freigaben.
        s.mcp_grants = apply_list(
            &self.mcp_grants,
            patch.replace_mcp_grants,
            patch.mcp_grants,
            |g| g.id,
            |_| true,
        );
        sort_grants(&mut s.mcp_grants);

        // Tool-Richtlinien (AIGW-87): Index nur bei Änderungen neu bauen.
        if patch.replace_tool_policies.is_none() && patch.tool_policies.is_empty() {
            s.tool_policies = self.tool_policies.clone();
            s.tool_index = Arc::clone(&self.tool_index);
        } else {
            let list = apply_list(
                &self.tool_policies,
                patch.replace_tool_policies,
                patch.tool_policies,
                |p| p.id,
                |p| p.spec.enabled,
            );
            set_tool_policies(&mut s, list);
        }

        // WASM-Module (AIGW-63): Inhalt bleibt geteilt (`Arc`/`Bytes`).
        s.wasm_modules = self.wasm_modules.clone();
        for c in patch.wasm_modules {
            match c {
                Change::Upsert(m) => {
                    s.wasm_modules.retain(|_, old| old.id != m.id);
                    s.wasm_modules.insert(m.name.clone(), Arc::new(m));
                }
                Change::Remove(id) => s.wasm_modules.retain(|_, old| old.id != id),
            }
        }
        s
    }
}

/// Wendet Änderungen auf eine Map nach ID an und liefert die Namen der geänderten Objekte
/// (vorher und nachher).
fn apply_by_id<T>(
    map: &mut HashMap<Id, Arc<Object<T>>>,
    changes: Vec<Change<Object<T>>>,
    id: impl Fn(&Object<T>) -> Id,
) -> HashSet<String> {
    let mut names = HashSet::new();
    for c in changes {
        match c {
            Change::Upsert(o) => {
                if let Some(old) = map.get(&id(&o)) {
                    names.insert(old.metadata.name.clone());
                }
                names.insert(o.metadata.name.clone());
                map.insert(id(&o), Arc::new(o));
            }
            Change::Remove(i) => {
                if let Some(old) = map.remove(&i) {
                    names.insert(old.metadata.name.clone());
                }
            }
        }
    }
    names
}

/// Liste mit Einzeländerungen bzw. vollständigem Ersatz (unsortiert; der Aufrufer sortiert).
fn apply_list<T>(
    old: &[Arc<T>],
    replace: Option<Vec<T>>,
    changes: Vec<Change<T>>,
    id: impl Fn(&T) -> Id,
    keep: impl Fn(&T) -> bool,
) -> Vec<Arc<T>> {
    let mut out: Vec<Arc<T>> = match replace {
        Some(all) => all.into_iter().filter(|x| keep(x)).map(Arc::new).collect(),
        None => old.to_vec(),
    };
    if changes.is_empty() {
        return out;
    }
    let ids: HashSet<Id> = changes
        .iter()
        .map(|c| match c {
            Change::Upsert(x) => id(x),
            Change::Remove(i) => *i,
        })
        .collect();
    out.retain(|x| !ids.contains(&id(x)));
    for c in changes {
        if let Change::Upsert(x) = c
            && keep(&x)
        {
            out.push(Arc::new(x));
        }
    }
    out
}
