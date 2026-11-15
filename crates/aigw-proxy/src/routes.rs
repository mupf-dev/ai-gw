// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Modellname (Alias) → Route einer Modellgruppe.
//!
//! - [`SnapshotRoutes`](crate::routing::SnapshotRoutes) – der Normalfall: Routen aus dem
//!   Konfigurations-Snapshot mit Strategien, Prioritätsgruppen, Retries, Fallbacks und Circuit
//!   Breaker (AIGW-24/25/26), Neuaufbau bei jeder neuen `config_version`.
//! - [`StaticRoutes`] – feste Tabelle Modellname → genau ein Deployment, ohne Retries
//!   (Tests, Benchmarks, eingebauter Mock).

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use aigw_core::ir::ModelInfo;
use aigw_core::snapshot::{ModelGroupCtx, Snapshot};

use crate::provider::ChatProvider;
use crate::routing::GroupRoute;

/// Auflösung von Modellnamen.
pub trait ModelRoutes: Send + Sync + 'static {
    /// Route für den angefragten Modellnamen (Name oder Alias; `None` = unbekannt).
    fn route(&self, model: &str) -> Option<Arc<GroupRoute>>;
    /// Route zur freigegebenen Modellgruppe `group` aus dem Snapshot `snap` der Anfrage
    /// (S-31): Freigabe und Route stammen aus **derselben** Konfigurationsversion – ein
    /// zwischenzeitlich umgehängter Alias kann nicht auf eine andere Gruppe zeigen als die
    /// geprüfte. Standard (ohne Snapshot-Bezug, z. B. [`StaticRoutes`]): [`Self::route`].
    fn route_for(
        &self,
        model: &str,
        snap: Option<&Snapshot>,
        group: Option<&ModelGroupCtx>,
    ) -> Option<Arc<GroupRoute>> {
        let _ = (snap, group);
        self.route(model)
    }
    /// Öffentliche Modellliste (`GET /v1/models`, wenn der Key keine eigene liefert).
    fn models(&self) -> Vec<ModelInfo>;
}

/// Feste Tabelle Modellname → Deployment (je Name genau eines, ohne Retries/Breaker).
#[derive(Default)]
pub struct StaticRoutes {
    routes: HashMap<String, Arc<GroupRoute>>,
    models: Vec<ModelInfo>,
    fallback: Option<Arc<dyn ChatProvider>>,
}

impl fmt::Debug for StaticRoutes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut names: Vec<&str> = self.routes.keys().map(String::as_str).collect();
        names.sort_unstable();
        f.debug_struct("StaticRoutes")
            .field("models", &names)
            .finish()
    }
}

impl StaticRoutes {
    /// Leere Tabelle.
    pub fn new() -> Self {
        Self::default()
    }

    /// Ordnet `model` ein Deployment zu (ein gleichnamiger Eintrag wird ersetzt).
    pub fn insert(&mut self, model: impl Into<String>, provider: Arc<dyn ChatProvider>) {
        let model = model.into();
        self.insert_route(model.clone(), Arc::new(GroupRoute::single(model, provider)));
    }

    /// Ordnet `model` eine vollständige Route zu (mehrere Deployments, Retries, Fallbacks).
    pub fn insert_route(&mut self, model: impl Into<String>, route: Arc<GroupRoute>) {
        let model = model.into();
        if !self.models.iter().any(|m| m.id == model) {
            self.models.push(ModelInfo {
                id: model.clone(),
                display_name: None,
                created: 0,
                owned_by: aigw_core::NAME.to_owned(),
            });
        }
        self.routes.insert(model, route);
    }

    /// Deployment für alle nicht eingetragenen Modellnamen (z. B. der Mock-Anbieter in
    /// Tests). Ohne Fallback sind unbekannte Namen `404`.
    pub fn set_fallback(&mut self, provider: Arc<dyn ChatProvider>) {
        self.fallback = Some(provider);
    }

    /// Ergänzt Anzeigenamen/Erstellungszeit eines Modells für `GET /v1/models`.
    pub fn set_model_info(&mut self, info: ModelInfo) {
        match self.models.iter_mut().find(|m| m.id == info.id) {
            Some(m) => *m = info,
            None => self.models.push(info),
        }
    }

    /// Anzahl der Modellnamen.
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// `true`, wenn leer.
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

impl ModelRoutes for StaticRoutes {
    fn route(&self, model: &str) -> Option<Arc<GroupRoute>> {
        if let Some(r) = self.routes.get(model) {
            return Some(Arc::clone(r));
        }
        self.fallback
            .as_ref()
            .map(|p| Arc::new(GroupRoute::single(model, Arc::clone(p))))
    }

    fn models(&self) -> Vec<ModelInfo> {
        self.models.clone()
    }
}
