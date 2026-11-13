// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Modellname (Alias) → Deployment.
//!
//! Vorläufig: [`StaticRoutes`] bildet jeden Modellnamen auf genau ein Deployment ab. Der
//! Snapshot-Halter (AIGW-10) und das Routing mit Strategien, Retries und Fallbacks
//! (AIGW-24/25) ersetzen das über eine eigene [`ModelRoutes`]-Implementierung;
//! [`StaticRoutes::from_model_groups`] baut die Tabelle bereits aus den Snapshot-Typen.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use aigw_core::ir::ModelInfo;
use aigw_core::snapshot::ModelGroupCtx;
use aigw_providers::{ConfigError, Registry, Upstream, UpstreamClient};

use crate::provider::ChatProvider;

/// Auflösung von Modellnamen.
pub trait ModelRoutes: Send + Sync + 'static {
    /// Deployment für den angefragten Modellnamen (`None` = unbekannt).
    fn resolve(&self, model: &str) -> Option<Arc<dyn ChatProvider>>;
    /// Öffentliche Modellliste (`GET /v1/models`).
    fn models(&self) -> Vec<ModelInfo>;
}

/// Feste Tabelle Modellname → Deployment.
#[derive(Default)]
pub struct StaticRoutes {
    routes: HashMap<String, Arc<dyn ChatProvider>>,
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
        if !self.models.iter().any(|m| m.id == model) {
            self.models.push(ModelInfo {
                id: model.clone(),
                display_name: None,
                created: 0,
                owned_by: aigw_core::NAME.to_owned(),
            });
        }
        self.routes.insert(model, provider);
    }

    /// Deployment für alle nicht eingetragenen Modellnamen (z. B. der Mock-Anbieter, solange
    /// das Snapshot-Routing fehlt). Ohne Fallback sind unbekannte Namen `404`.
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

    /// Baut die Tabelle aus Modellgruppen des Snapshots: je Gruppe (Name und alle Aliasse)
    /// das erste aktive Deployment (niedrigste Priorität) mit unterstütztem Anbieter.
    /// Gruppen ohne nutzbares Deployment werden ausgelassen und gemeldet.
    pub fn from_model_groups<'a>(
        groups: impl IntoIterator<Item = &'a ModelGroupCtx>,
        registry: &Registry,
        client: &UpstreamClient,
    ) -> Result<Self, ConfigError> {
        let mut routes = Self::new();
        for g in groups {
            let Some(dep) = g
                .deployments
                .iter()
                .filter(|d| d.deployment.spec.enabled)
                .find(|d| registry.supports(d.deployment.spec.provider))
            else {
                tracing::warn!(
                    model_group = %g.group.metadata.name,
                    "Modellgruppe ohne unterstütztes Deployment – nicht erreichbar"
                );
                continue;
            };
            let upstream: Arc<dyn ChatProvider> =
                Arc::new(Upstream::from_deployment(dep, registry, client)?);
            routes.insert(g.group.metadata.name.clone(), upstream.clone());
            for alias in &g.group.spec.aliases {
                routes.insert(alias.clone(), upstream.clone());
            }
        }
        Ok(routes)
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
    fn resolve(&self, model: &str) -> Option<Arc<dyn ChatProvider>> {
        self.routes.get(model).or(self.fallback.as_ref()).cloned()
    }

    fn models(&self) -> Vec<ModelInfo> {
        self.models.clone()
    }
}
