// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Ist-Zustand aus der Datenbank (für Export und Diff).

use std::collections::BTreeMap;

use aigw_core::resource::{
    ApiKeySpec, ApiKeyStatus, DeploymentSpec, Id, Manifest, McpGrantSpec, McpServerSpec,
    ModelGroupSpec, Object, ObjectMeta, OrganizationSpec, PluginBindingSpec, PriceSpec,
    ProviderCredentialSpec, ResourceKind, ResourceSpec, SecretRef, SecretString, TeamSpec,
    UserSpec,
};
use aigw_core::snapshot::KeyHash;

use super::document::{normalize, render_yaml, secret_env_name};
use super::{ApiKeyMode, GitopsError};
use crate::crypto::Keyring;
use crate::repo::api_key_from_row;
use crate::{Resource, Store, StoreError};

/// Vom Server verwaltete Zusatzdaten einer bestehenden Ressource.
#[derive(Debug, Clone)]
pub(crate) enum Extra {
    None,
    /// Entschlüsselte Werte (nur mit Keyring geladen) und gespeicherte `valueFrom`-Verweise
    /// (F54).
    Credential(
        Option<BTreeMap<String, SecretString>>,
        crate::repo::credential::CredentialRefs,
    ),
    /// Hash und Status eines Keys.
    ApiKey {
        hash: KeyHash,
        status: ApiKeyStatus,
    },
}

/// Eine bestehende Ressource.
#[derive(Debug, Clone)]
pub(crate) struct Current {
    pub(crate) id: Id,
    /// Als Manifest (ohne Server-Felder; Zugangsdaten mit Platzhalter-Verweisen).
    pub(crate) manifest: Manifest,
    /// Lesesicht wie in der Admin-API (für `before` im Audit-Log).
    pub(crate) audit: serde_json::Value,
    pub(crate) extra: Extra,
}

/// Alle Ressourcen mit der Version, zu der sie gelesen wurden.
#[derive(Debug, Clone, Default)]
pub(crate) struct State {
    pub(crate) version: i64,
    pub(crate) objects: BTreeMap<ResourceKind, BTreeMap<String, Current>>,
}

impl State {
    pub(crate) fn get(&self, kind: ResourceKind, name: &str) -> Option<&Current> {
        self.objects.get(&kind).and_then(|m| m.get(name))
    }

    pub(crate) fn of(&self, kind: ResourceKind) -> impl Iterator<Item = (&String, &Current)> {
        self.objects.get(&kind).into_iter().flatten()
    }
}

fn plain_meta(m: &ObjectMeta) -> ObjectMeta {
    ObjectMeta {
        name: m.name.clone(),
        labels: m.labels.clone(),
        created_at: None,
        updated_at: None,
    }
}

async fn plain<R: Resource>(
    store: &Store,
    out: &mut BTreeMap<String, Current>,
    wrap: fn(R) -> ResourceSpec,
) -> Result<(), StoreError> {
    for obj in store.list::<R>().await? {
        let audit = serde_json::to_value(&obj)?;
        let Object { id, metadata, spec } = obj;
        let mut manifest = Manifest {
            metadata: plain_meta(&metadata),
            spec: wrap(spec),
        };
        normalize(&mut manifest);
        out.insert(
            metadata.name,
            Current {
                id,
                manifest,
                audit,
                extra: Extra::None,
            },
        );
    }
    Ok(())
}

async fn load_once(store: &Store, keyring: Option<&Keyring>) -> Result<State, StoreError> {
    let version = store.config_version().await?;
    let mut objects: BTreeMap<ResourceKind, BTreeMap<String, Current>> = BTreeMap::new();
    for kind in ResourceKind::ALL {
        let out = objects.entry(kind).or_default();
        match kind {
            ResourceKind::Organization => {
                plain::<OrganizationSpec>(store, out, ResourceSpec::Organization).await?;
            }
            ResourceKind::Team => plain::<TeamSpec>(store, out, ResourceSpec::Team).await?,
            ResourceKind::User => plain::<UserSpec>(store, out, ResourceSpec::User).await?,
            ResourceKind::ModelGroup => {
                plain::<ModelGroupSpec>(store, out, ResourceSpec::ModelGroup).await?;
            }
            ResourceKind::Deployment => {
                plain::<DeploymentSpec>(store, out, ResourceSpec::Deployment).await?;
            }
            ResourceKind::Price => plain::<PriceSpec>(store, out, ResourceSpec::Price).await?,
            ResourceKind::PluginBinding => {
                plain::<PluginBindingSpec>(store, out, ResourceSpec::PluginBinding).await?;
            }
            ResourceKind::McpServer => {
                plain::<McpServerSpec>(store, out, ResourceSpec::McpServer).await?;
            }
            ResourceKind::McpGrant => {
                plain::<McpGrantSpec>(store, out, ResourceSpec::McpGrant).await?;
            }
            ResourceKind::ApiKey => {
                let rows = store
                    .fetch_rows(ResourceKind::ApiKey, ApiKeySpec::REFS, "", None)
                    .await?;
                for row in &rows {
                    let (key, hash) = api_key_from_row(row)?;
                    let mut manifest = Manifest {
                        metadata: plain_meta(&key.metadata),
                        spec: ResourceSpec::ApiKey(key.spec.clone()),
                    };
                    normalize(&mut manifest);
                    out.insert(
                        key.metadata.name.clone(),
                        Current {
                            id: key.id,
                            manifest,
                            audit: serde_json::to_value(&key)?,
                            extra: Extra::ApiKey {
                                hash,
                                status: key.status.clone(),
                            },
                        },
                    );
                }
            }
            ResourceKind::ProviderCredential => {
                let mut plain_values = match keyring {
                    Some(k) => Some(store.decrypt_credentials(k).await?),
                    None => None,
                };
                let mut all_refs = store.credential_refs().await?;
                for c in store.list_credentials().await? {
                    let name = c.metadata.name.clone();
                    let refs = all_refs.remove(&name).unwrap_or_default();
                    // F54: gespeicherter Verweis, sonst Platzhalter `AIGW_SECRET_…`.
                    let data = c
                        .spec
                        .fields
                        .iter()
                        .map(|f| {
                            let r = match refs.get(f) {
                                Some(v) => SecretRef {
                                    value: None,
                                    value_from: Some(v.clone()),
                                },
                                None => SecretRef::env(secret_env_name(&name, f)),
                            };
                            (f.clone(), r)
                        })
                        .collect();
                    let values = plain_values
                        .as_mut()
                        .and_then(|m| m.remove(&name))
                        .map(|(_, data)| data.0);
                    out.insert(
                        name,
                        Current {
                            id: c.id,
                            manifest: Manifest {
                                metadata: plain_meta(&c.metadata),
                                spec: ResourceSpec::ProviderCredential(ProviderCredentialSpec {
                                    provider: c.spec.provider,
                                    data,
                                }),
                            },
                            audit: serde_json::to_value(&c)?,
                            extra: Extra::Credential(values, refs),
                        },
                    );
                }
            }
            // F149: wie Zugangsdaten (Verweise originalgetreu, sonst Platzhalter).
            ResourceKind::Secret => {
                let mut plain_values = match keyring {
                    Some(k) => Some(store.decrypt_secrets(k).await?),
                    None => None,
                };
                let mut all_refs = store.secret_refs().await?;
                for s in store.list_secrets().await? {
                    let name = s.metadata.name.clone();
                    let refs = all_refs.remove(&name).unwrap_or_default();
                    let data = s
                        .spec
                        .fields
                        .iter()
                        .map(|f| {
                            let r = match refs.get(f) {
                                Some(v) => SecretRef {
                                    value: None,
                                    value_from: Some(v.clone()),
                                },
                                None => SecretRef::env(secret_env_name(&name, f)),
                            };
                            (f.clone(), r)
                        })
                        .collect();
                    let values = plain_values
                        .as_mut()
                        .and_then(|m| m.remove(&name))
                        .map(|(_, data)| data.0);
                    out.insert(
                        name,
                        Current {
                            id: s.id,
                            manifest: Manifest {
                                metadata: plain_meta(&s.metadata),
                                spec: ResourceSpec::Secret(aigw_core::resource::SecretSpec {
                                    description: s.spec.description.clone(),
                                    data,
                                }),
                            },
                            audit: serde_json::to_value(&s)?,
                            extra: Extra::Credential(values, refs),
                        },
                    );
                }
            }
        }
    }
    Ok(State { version, objects })
}

/// Liest den Ist-Zustand konsistent (Version vor und nach dem Lesen gleich, sonst neu).
pub(crate) async fn load(store: &Store, keyring: Option<&Keyring>) -> Result<State, StoreError> {
    let mut attempt = 0;
    loop {
        let state = load_once(store, keyring).await?;
        let after = store.config_version().await?;
        attempt += 1;
        if after == state.version || attempt >= 5 {
            return Ok(state);
        }
    }
}

/// Auswahl für einen Teil-Export (AIGW-55): Arten, Name, Organisation/Team.
///
/// - `kinds`: nur diese Arten (leer = alle).
/// - `name`: nur Ressourcen mit genau diesem Namen (z. B. eine Modellgruppe).
/// - `organization`: die Organisation selbst, ihre Teams, ihre API-Keys sowie
///   Plugin-Bindungen und MCP-Freigaben auf Ebene der Organisation, ihrer Teams und Keys.
///   Katalog-Ressourcen (Modellgruppen, Deployments, Zugangsdaten, Preise …) und Nutzer
///   gehören zu keiner Organisation und fallen dann weg.
/// - `team`: wie `organization`, aber nur das Team, seine Keys und deren Bindungen/Freigaben.
///
/// Der Teil-Export ist zum Ansehen und für `apply` **ohne** `prune` gedacht – mit `prune`
/// würde alles außerhalb der Auswahl gelöscht.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportFilter {
    /// Nur diese Arten (leer = alle).
    pub kinds: Vec<ResourceKind>,
    /// Nur Ressourcen mit diesem Namen.
    pub name: Option<String>,
    /// Nur Ressourcen dieser Organisation.
    pub organization: Option<String>,
    /// Nur Ressourcen dieses Teams.
    pub team: Option<String>,
}

impl ExportFilter {
    /// `true`, wenn nichts eingeschränkt ist.
    pub fn is_all(&self) -> bool {
        self.kinds.is_empty()
            && self.name.is_none()
            && self.organization.is_none()
            && self.team.is_none()
    }
}

/// Teams und Keys einer Auswahl nach Organisation/Team (für Bindungen und Freigaben).
struct ScopeSet {
    organization: Option<String>,
    teams: std::collections::BTreeSet<String>,
    keys: std::collections::BTreeSet<String>,
}

impl ScopeSet {
    fn build(state: &State, f: &ExportFilter) -> Option<Self> {
        if f.organization.is_none() && f.team.is_none() {
            return None;
        }
        let team_ok = |name: &str, spec: &TeamSpec| {
            f.team.as_deref().is_none_or(|t| t == name)
                && f.organization
                    .as_deref()
                    .is_none_or(|o| o == spec.organization)
        };
        let teams: std::collections::BTreeSet<String> = state
            .of(ResourceKind::Team)
            .filter_map(|(n, c)| match &c.manifest.spec {
                ResourceSpec::Team(t) if team_ok(n, t) => Some(n.clone()),
                _ => None,
            })
            .collect();
        let keys = state
            .of(ResourceKind::ApiKey)
            .filter_map(|(n, c)| match &c.manifest.spec {
                ResourceSpec::ApiKey(k) => {
                    let ok = match &f.team {
                        Some(_) => k.team.as_ref().is_some_and(|t| teams.contains(t)),
                        None => f.organization.as_deref() == Some(k.organization.as_str()),
                    };
                    ok.then(|| n.clone())
                }
                _ => None,
            })
            .collect();
        Some(Self {
            organization: if f.team.is_none() {
                f.organization.clone()
            } else {
                None
            },
            teams,
            keys,
        })
    }

    fn covers(&self, scope: &aigw_core::resource::Scope) -> bool {
        use aigw_core::resource::ScopeLevel;
        let t = scope.target.as_deref().unwrap_or_default();
        match scope.level {
            ScopeLevel::Organization => self.organization.as_deref() == Some(t),
            ScopeLevel::Team => self.teams.contains(t),
            ScopeLevel::ApiKey => self.keys.contains(t),
            ScopeLevel::Global | ScopeLevel::ModelGroup => false,
        }
    }

    fn contains(&self, name: &str, spec: &ResourceSpec) -> bool {
        match spec {
            ResourceSpec::Organization(_) => self.organization.as_deref() == Some(name),
            ResourceSpec::Team(_) => self.teams.contains(name),
            ResourceSpec::ApiKey(_) => self.keys.contains(name),
            ResourceSpec::PluginBinding(b) => self.covers(&b.scope),
            ResourceSpec::McpGrant(g) => self.covers(&g.subject),
            _ => false,
        }
    }
}

/// Exportiert alle Ressourcen als YAML (deterministisch: Abhängigkeitsreihenfolge, je Art
/// nach Name; ohne IDs, Zeitstempel, Hashes und Key-Zustand). Zugangsdaten erscheinen mit
/// ihren gespeicherten `valueFrom`-Verweisen (F54), Felder ohne Verweis mit Platzhaltern
/// `valueFrom: {env: AIGW_SECRET_…}`; API-Keys ohne Geheimnis.
pub async fn export_yaml(store: &Store, api_keys: ApiKeyMode) -> Result<String, GitopsError> {
    export_yaml_filtered(store, api_keys, &ExportFilter::default()).await
}

/// Wie [`export_yaml`], aber nur die Auswahl `filter` (AIGW-55).
pub async fn export_yaml_filtered(
    store: &Store,
    api_keys: ApiKeyMode,
    filter: &ExportFilter,
) -> Result<String, GitopsError> {
    let state = load(store, None).await?;
    let scope = ScopeSet::build(&state, filter);
    let mut docs = Vec::new();
    let mut placeholders = Vec::new();
    let mut keys = 0usize;
    for kind in ResourceKind::ALL {
        if kind == ResourceKind::ApiKey && api_keys == ApiKeyMode::Ignore {
            continue;
        }
        if !filter.kinds.is_empty() && !filter.kinds.contains(&kind) {
            continue;
        }
        for (name, cur) in state.of(kind) {
            if filter.name.as_deref().is_some_and(|n| n != name) {
                continue;
            }
            if let Some(s) = &scope
                && !s.contains(name, &cur.manifest.spec)
            {
                continue;
            }
            let data = match &cur.manifest.spec {
                ResourceSpec::ProviderCredential(c) => Some(&c.data),
                ResourceSpec::Secret(s) => Some(&s.data),
                _ => None,
            };
            if let Some(data) = data {
                for (field, r) in data {
                    // Nur Platzhalter auflisten; gespeicherte Verweise (F54) stehen original im
                    // Export.
                    if let Some(aigw_core::resource::ValueFrom::Env(env)) = &r.value_from
                        && *env == secret_env_name(name, field)
                    {
                        placeholders.push(format!("  {env}  ({name}.{field})"));
                    }
                }
            }
            if kind == ResourceKind::ApiKey {
                keys += 1;
            }
            docs.push(cur.manifest.clone());
        }
    }
    let mut notes = Vec::new();
    if !filter.is_all() {
        notes.push(
            "Teil-Export (Auswahl) – nur ohne --prune anwenden, sonst wird alles außerhalb \
             der Auswahl gelöscht."
                .to_owned(),
        );
    }
    if !placeholders.is_empty() {
        notes.push(
            "Zugangsdaten und Geheimnisse stehen nur als Verweis im Export. Vor `apply` diese \
             Variablen setzen \
             oder die Verweise anpassen (valueFrom: {env: …} bzw. {file: …}):"
                .to_owned(),
        );
        notes.extend(placeholders);
    }
    if keys > 0 {
        notes.push(
            "API-Keys ohne Klartext: bestehende Keys behalten ihr Geheimnis; neue Keys brauchen \
             spec.secret.valueFrom, sonst werden sie übersprungen."
                .to_owned(),
        );
    }
    render_yaml(&docs, &notes)
}
