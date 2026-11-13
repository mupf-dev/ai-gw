// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Routing-Tabelle aus dem Konfigurations-Snapshot (AIGW-24) mit Neuaufbau bei jeder neuen
//! `config_version` (Hot Reload, §14).
//!
//! Der Hot Path liest die Tabelle lock-frei (`ArcSwap`). Laufende Anfragen behalten ihre
//! [`GroupRoute`] bis zum Ende. Der Laufzeitzustand der Deployments ([`DeploymentState`])
//! hängt an der Deployment-ID und wird beim Neuaufbau übernommen, solange sich das Deployment
//! nicht wesentlich geändert hat.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use aigw_core::ir::ModelInfo;
use aigw_core::resource::Id;
use aigw_core::snapshot::{DeploymentCtx, ModelGroupCtx, Snapshot, SnapshotHandle};
use aigw_core::types::ProviderKind;
use aigw_providers::{Registry, Upstream, UpstreamClient, with_group_defaults};
use arc_swap::ArcSwap;
use time::OffsetDateTime;

use super::state::{DeploymentState, fingerprint};
use super::{BreakerState, GroupPolicy, GroupRoute, MemberSpec, health};
use crate::mock::MockProvider;
use crate::provider::ChatProvider;
use crate::routes::ModelRoutes;

/// Unveränderliche Routing-Tabelle einer Konfigurationsversion.
#[derive(Debug, Default)]
pub struct RouteTable {
    version: i64,
    /// Name und Alias → Route.
    by_name: HashMap<String, Arc<GroupRoute>>,
    /// Jede Gruppe einmal.
    groups: Vec<Arc<GroupRoute>>,
    models: Vec<ModelInfo>,
}

impl RouteTable {
    /// `config_version`, aus der die Tabelle gebaut wurde.
    pub fn version(&self) -> i64 {
        self.version
    }

    /// Route zu einem Modellnamen (Name oder Alias).
    pub fn get(&self, model: &str) -> Option<&Arc<GroupRoute>> {
        self.by_name.get(model)
    }

    /// Alle Modellgruppen (je einmal).
    pub fn unique_groups(&self) -> &[Arc<GroupRoute>] {
        &self.groups
    }
}

/// Routing aus dem Snapshot; implementiert [`ModelRoutes`].
pub struct SnapshotRoutes {
    table: ArcSwap<RouteTable>,
    states: Mutex<HashMap<Id, Arc<DeploymentState>>>,
    registry: Registry,
    client: UpstreamClient,
    /// Deployments mit `provider: mock` aufnehmen (`admin.allow_mock`, F85).
    allow_mock: bool,
}

/// Zustand eines Deployments für Kennzahlen (`aigw_deployment_breaker_state`, F86).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentStatus {
    /// Modellgruppe.
    pub model_group: String,
    /// Name des Deployments.
    pub deployment: String,
    /// Circuit Breaker.
    pub breaker: BreakerState,
    /// Offene Anfragen dieser Instanz.
    pub in_flight: u32,
}

impl fmt::Debug for SnapshotRoutes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let t = self.table.load();
        f.debug_struct("SnapshotRoutes")
            .field("version", &t.version)
            .field("groups", &t.groups.len())
            .field("registry", &self.registry)
            .finish_non_exhaustive()
    }
}

impl SnapshotRoutes {
    /// Leere Tabelle; Adapter aus `registry`, HTTP über `client`.
    pub fn new(registry: Registry, client: UpstreamClient) -> Self {
        Self {
            table: ArcSwap::from_pointee(RouteTable::default()),
            states: Mutex::new(HashMap::new()),
            registry,
            client,
            allow_mock: true,
        }
    }

    /// Deployments mit `provider: mock` zulassen (Standard) oder auslassen
    /// (`admin.allow_mock: false`, F85).
    #[must_use]
    pub fn with_mock(mut self, allow: bool) -> Self {
        self.allow_mock = allow;
        self
    }

    /// Zustand aller Deployments der aktuellen Tabelle (für `/metrics`, F86).
    pub fn deployment_status(&self) -> Vec<DeploymentStatus> {
        let table = self.table.load();
        let mut out = Vec::new();
        for g in &table.groups {
            for m in g.members() {
                out.push(DeploymentStatus {
                    model_group: g.name().to_owned(),
                    deployment: m.provider().describe().deployment.to_owned(),
                    breaker: m.state().breaker_state(),
                    in_flight: m.state().in_flight(),
                });
            }
        }
        out
    }

    /// Mit allen eingebauten Adaptern und dem Standard-HTTP-Client mit strengen SSRF-Regeln.
    pub fn with_defaults() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Self::with_policy(aigw_core::upstream::UpstreamPolicy::strict())
    }

    /// Mit allen eingebauten Adaptern und einem HTTP-Client mit den SSRF-Regeln `policy`
    /// (Betrieb: `admin.allow_*_upstreams`; geprüft auch nach der Namensauflösung, AIGW-72).
    pub fn with_policy(
        policy: aigw_core::upstream::UpstreamPolicy,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self::new(
            Registry::with_builtin(),
            UpstreamClient::with_policy(policy)?,
        ))
    }

    /// Aktuelle Tabelle.
    pub fn table(&self) -> Arc<RouteTable> {
        self.table.load_full()
    }

    /// Version der aktuellen Tabelle.
    pub fn version(&self) -> i64 {
        self.table.load().version
    }

    /// Laufzeitzustand eines Deployments (für Tests und spätere Statusanzeigen).
    pub fn deployment_state(&self, id: Id) -> Option<Arc<DeploymentState>> {
        self.states
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
            .cloned()
    }

    /// Baut die Tabelle aus `snap` neu und veröffentlicht sie. Deployments, deren Adapter
    /// sich nicht bauen lässt (unbekannter Anbieter, fehlende Zugangsdaten …), werden
    /// ausgelassen und gemeldet; die übrigen bleiben erreichbar.
    pub fn rebuild(&self, snap: &Snapshot) {
        let now = OffsetDateTime::now_utc();
        let mut states = self.states.lock().unwrap_or_else(PoisonError::into_inner);
        let mut live: HashSet<Id> = HashSet::new();
        let mut seen: HashSet<Id> = HashSet::new();
        let mut table = RouteTable {
            version: snap.version,
            ..RouteTable::default()
        };
        let mut deployments = 0usize;
        let mut groups: Vec<&Arc<ModelGroupCtx>> = snap
            .model_groups
            .values()
            .filter(|g| seen.insert(g.group.id))
            .collect();
        groups.sort_by(|a, b| a.group.metadata.name.cmp(&b.group.metadata.name));
        for g in groups {
            let name = &g.group.metadata.name;
            let policy = GroupPolicy::from_settings(&g.group.spec.settings).unwrap_or_else(|e| {
                tracing::error!(
                    model_group = %name,
                    error = %e,
                    "Routing-Einstellungen ungültig – Standardwerte aktiv"
                );
                GroupPolicy::default()
            });
            let mut members = Vec::new();
            for d in g.deployments.iter().filter(|d| d.deployment.spec.enabled) {
                // Von der Modellgruppe geerbte Einstellungen (z. B. `defaultMaxTokens`).
                let effective = with_group_defaults(&g.group.spec.settings, d);
                let d = &*effective;
                let provider = match self.build_provider(d) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(
                            model_group = %name,
                            deployment = %d.deployment.metadata.name,
                            error = %e,
                            "Deployment nicht nutzbar – ausgelassen"
                        );
                        continue;
                    }
                };
                let fp = fingerprint(d);
                let id = d.deployment.id;
                let state = match states.get(&id) {
                    Some(s) if s.fingerprint == fp => Arc::clone(s),
                    _ => {
                        let s = Arc::new(DeploymentState::new(fp));
                        states.insert(id, Arc::clone(&s));
                        s
                    }
                };
                live.insert(id);
                let spec = &d.deployment.spec;
                let cost = snap
                    .price_at(spec.provider, &spec.provider_model, now)
                    .map(|p| {
                        let t = &p.spec.per_million_tokens;
                        let nanos = |a: Option<aigw_core::resource::Amount>| {
                            u64::try_from(a.map_or(0, |a| a.nanos())).unwrap_or(0)
                        };
                        nanos(t.input).saturating_add(nanos(t.output))
                    });
                members.push(MemberSpec {
                    provider,
                    state: Some(state),
                    weight: spec.weight,
                    priority: spec.priority,
                    capacity: spec.capacity,
                    cost,
                });
            }
            if members.is_empty() {
                tracing::warn!(model_group = %name, "Modellgruppe ohne nutzbares Deployment");
            }
            deployments += members.len();
            let route = Arc::new(GroupRoute::new(
                name.clone(),
                g.group.spec.routing,
                policy,
                g.group.spec.fallbacks.clone(),
                members,
            ));
            let created = g
                .group
                .metadata
                .created_at
                .map_or(0, OffsetDateTime::unix_timestamp);
            for n in std::iter::once(name).chain(g.group.spec.aliases.iter()) {
                table.by_name.insert(n.clone(), Arc::clone(&route));
                table.models.push(ModelInfo {
                    id: n.clone(),
                    display_name: None,
                    created,
                    owned_by: aigw_core::NAME.to_owned(),
                });
            }
            table.groups.push(route);
        }
        states.retain(|id, _| live.contains(id));
        drop(states);
        table.models.sort_by(|a, b| a.id.cmp(&b.id));
        tracing::info!(
            config_version = table.version,
            model_groups = table.groups.len(),
            deployments,
            "Routing-Tabelle aufgebaut"
        );
        self.table.store(Arc::new(table));
    }

    fn build_provider(&self, d: &DeploymentCtx) -> Result<Arc<dyn ChatProvider>, String> {
        let kind = d.deployment.spec.provider;
        if kind == ProviderKind::Mock {
            if !self.allow_mock {
                return Err("provider: mock ist gesperrt (admin.allow_mock: false)".into());
            }
            return Ok(Arc::new(MockProvider::for_deployment(d)));
        }
        if !self.registry.supports(kind) {
            return Err(format!(
                "kein Adapter für Anbieter {} registriert",
                kind.as_str()
            ));
        }
        Upstream::from_deployment(d, &self.registry, &self.client)
            .map(|u| Arc::new(u) as Arc<dyn ChatProvider>)
            .map_err(|e| e.to_string())
    }

    /// Baut die Tabelle aus dem aktuellen Snapshot und startet eine Task, die sie bei jeder
    /// neuen `config_version` neu baut und fällige Health Checks auslöst (Takt 1 s). Die Task
    /// endet, wenn das zurückgegebene [`RoutesTask`] fallen gelassen wird.
    pub fn spawn(self: &Arc<Self>, handle: SnapshotHandle) -> RoutesTask {
        let mut rx = handle.subscribe();
        self.rebuild(&handle.load());
        let routes = Arc::clone(self);
        let task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    changed = rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        let snap = handle.load();
                        if snap.version != routes.version() {
                            routes.rebuild(&snap);
                        }
                    }
                    _ = tick.tick() => health::run_due(&routes.table.load()),
                }
            }
        });
        RoutesTask(task)
    }
}

impl ModelRoutes for SnapshotRoutes {
    fn route(&self, model: &str) -> Option<Arc<GroupRoute>> {
        self.table.load().by_name.get(model).cloned()
    }

    fn models(&self) -> Vec<ModelInfo> {
        self.table.load().models.clone()
    }
}

/// Hintergrund-Task von [`SnapshotRoutes::spawn`]; wird beim Fallenlassen beendet.
#[derive(Debug)]
pub struct RoutesTask(tokio::task::JoinHandle<()>);

impl Drop for RoutesTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}
