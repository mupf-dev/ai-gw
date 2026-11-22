// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Plugin `cache`: schaltet den Antwort-Cache (AIGW-39/40) für eine Ebene ein und legt die
//! Richtlinie fest.
//!
//! Das Plugin selbst hat **keine Haken** (es kostet in der Kette nichts). Nachschlagen und
//! Speichern übernimmt die Pipeline in `aigw-proxy` als eigener Schritt **nach** allen
//! Pre-Plugins (Architektur §3, Schritt 6); sie liest die wirksame Richtlinie mit
//! [`policy_of`] aus der Kette. Begründung (siehe `docs/konzept/14-cache.md`): Ein
//! Cache-Treffer als Plugin-`Respond` würde nachfolgende Pre-Plugins (Guardrails) und alle
//! Ausgabe-Plugins überspringen, und der Schlüssel hinge von der `order` ab.
//!
//! ```yaml
//! plugin: cache
//! config:
//!   inherit: override          # Kettensteuerung: ersetzt eine geerbte cache-Bindung
//!   mode: semantic             # exact (Standard) | semantic (exakt, dann semantisch)
//!   scope: team                # key | team (Standard) | organization | global (Warnung!)
//!   ttlSecs: 3600
//!   clientControl: true        # Cache-Control: no-cache/no-store, x-aigw-cache-ttl
//!   clientMaxTtlSecs: 86400    # Obergrenze für x-aigw-cache-ttl
//!   cacheNonZeroTemperature: false   # Standard: nur temperature == 0 wird gecacht
//!   cacheToolCalls: false      # Antworten mit Tool-Aufrufen speichern?
//!   semantic:
//!     threshold: 0.95          # Kosinus-Ähnlichkeit
//!     thresholdByGroup: {gpt-4o: 0.97}
//!     embeddingModel: text-embedding-3-small   # Modellgruppe des Gateways
//!     maxChars: 8000           # längere Nutzernachrichten nur exakt
//! ```
//!
//! Mehrere `cache`-Bindungen in einer Kette (`inherit: append`): es gilt die **letzte** in
//! Ausführungsreihenfolge (innerste Ebene bzw. höchste `order`).

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use crate::chain::Chain;
use crate::plugin::{Phases, Plugin};
use crate::registry::PluginFactory;

/// Name des Plugins.
pub const NAME: &str = "cache";

/// Modus.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CacheMode {
    /// Nur exakte Treffer.
    #[default]
    Exact,
    /// Erst exakt, dann semantisch (Embedding der letzten Nutzernachricht).
    Semantic,
}

/// Scope der Cache-Einträge: Antworten werden nur innerhalb dieses Bereichs geteilt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CacheScope {
    /// Je API-Key.
    Key,
    /// Je Team (Standard; Keys ohne Team: je Key).
    #[default]
    Team,
    /// Je Organisation (Keys ohne Organisation: je Team bzw. Key).
    Organization,
    /// Über alle Mandanten hinweg – nur für nicht vertrauliche Inhalte (Warnung im Log).
    Global,
}

/// Richtlinie des semantischen Caches.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct SemanticPolicy {
    /// Mindest-Kosinus-Ähnlichkeit für einen Treffer (0 < t ≤ 1).
    pub threshold: f32,
    /// Abweichende Schwellen je Modellgruppe (kanonischer Name).
    pub threshold_by_group: BTreeMap<String, f32>,
    /// Embedding-Modellgruppe (Standard: `cache.semantic.embedding_model`).
    pub embedding_model: Option<String>,
    /// Längere letzte Nutzernachrichten werden nur exakt gecacht.
    pub max_chars: usize,
}

impl Default for SemanticPolicy {
    fn default() -> Self {
        Self {
            threshold: 0.95,
            threshold_by_group: BTreeMap::new(),
            embedding_model: None,
            max_chars: 8000,
        }
    }
}

impl SemanticPolicy {
    /// Schwelle für eine Modellgruppe.
    pub fn threshold_for(&self, group: &str) -> f32 {
        self.threshold_by_group
            .get(group)
            .copied()
            .unwrap_or(self.threshold)
    }
}

/// Wirksame Cache-Richtlinie einer Bindung.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct CachePolicy {
    /// Modus.
    pub mode: CacheMode,
    /// Scope.
    pub scope: CacheScope,
    /// Lebensdauer neuer Einträge (Sekunden).
    pub ttl_secs: u64,
    /// Client darf über `Cache-Control` und `x-aigw-cache-ttl` steuern.
    pub client_control: bool,
    /// Obergrenze für `x-aigw-cache-ttl` (Sekunden).
    pub client_max_ttl_secs: u64,
    /// Auch Anfragen mit `temperature > 0` (oder ohne Angabe) cachen.
    pub cache_non_zero_temperature: bool,
    /// Antworten mit Tool-Aufrufen speichern.
    pub cache_tool_calls: bool,
    /// Semantischer Cache.
    pub semantic: SemanticPolicy,
}

impl Default for CachePolicy {
    fn default() -> Self {
        Self {
            mode: CacheMode::Exact,
            scope: CacheScope::Team,
            ttl_secs: 3600,
            client_control: true,
            client_max_ttl_secs: 86_400,
            cache_non_zero_temperature: false,
            cache_tool_calls: false,
            semantic: SemanticPolicy::default(),
        }
    }
}

impl CachePolicy {
    fn validate(&self) -> Result<(), String> {
        if self.ttl_secs == 0 {
            return Err("ttlSecs muss größer als 0 sein".into());
        }
        let ok = |t: f32| t.is_finite() && t > 0.0 && t <= 1.0;
        if !ok(self.semantic.threshold) {
            return Err(format!(
                "semantic.threshold muss in (0, 1] liegen: {}",
                self.semantic.threshold
            ));
        }
        for (g, t) in &self.semantic.threshold_by_group {
            if !ok(*t) {
                return Err(format!(
                    "semantic.thresholdByGroup.{g} muss in (0, 1] liegen: {t}"
                ));
            }
        }
        if self.semantic.embedding_model.as_deref() == Some("") {
            return Err("semantic.embeddingModel darf nicht leer sein".into());
        }
        Ok(())
    }
}

/// Instanz einer `cache`-Bindung (nur Träger der Richtlinie).
#[derive(Debug)]
pub struct CachePlugin {
    policy: Arc<CachePolicy>,
}

impl CachePlugin {
    /// Plugin mit dieser Richtlinie.
    pub fn new(policy: CachePolicy) -> Self {
        Self {
            policy: Arc::new(policy),
        }
    }

    /// Richtlinie.
    pub fn policy(&self) -> &Arc<CachePolicy> {
        &self.policy
    }
}

impl Plugin for CachePlugin {
    fn name(&self) -> &'static str {
        NAME
    }

    fn phases(&self) -> Phases {
        Phases::default()
    }
}

/// Wirksame Cache-Richtlinie einer Kette: die letzte `cache`-Bindung in
/// Ausführungsreihenfolge, `None` ohne Bindung.
pub fn policy_of(chain: &Chain) -> Option<Arc<CachePolicy>> {
    chain.entries().iter().rev().find_map(|e| {
        if e.plugin_name() != NAME {
            return None;
        }
        let any: &dyn std::any::Any = e.plugin().as_ref();
        any.downcast_ref::<CachePlugin>()
            .map(|p| Arc::clone(&p.policy))
    })
}

/// Fabrik von `cache`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Factory;

impl PluginFactory for Factory {
    fn name(&self) -> &'static str {
        NAME
    }

    fn build(&self, config: &Value) -> Result<Arc<dyn Plugin>, String> {
        let policy: CachePolicy = if config.is_null() {
            CachePolicy::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| e.to_string())?
        };
        policy.validate()?;
        if policy.scope == CacheScope::Global {
            tracing::warn!(
                "Plugin cache mit scope: global – gecachte Antworten werden über Mandanten \
                 hinweg geteilt; nur für nicht vertrauliche Inhalte verwenden"
            );
        }
        Ok(Arc::new(CachePlugin::new(policy)))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use serde_json::json;

    use super::*;
    use crate::chain::Entry;

    #[test]
    fn defaults_and_validation() {
        let p = Factory.build(&json!({})).unwrap();
        let chain = Chain::new(vec![Arc::new(Entry::new("c", p))]);
        let pol = policy_of(&chain).unwrap();
        assert_eq!(pol.scope, CacheScope::Team);
        assert_eq!(pol.mode, CacheMode::Exact);
        assert!(!pol.cache_non_zero_temperature);
        assert!(Factory.build(&json!({"ttlSecs": 0})).is_err());
        assert!(
            Factory
                .build(&json!({"semantic": {"threshold": 1.5}}))
                .is_err()
        );
        assert!(Factory.build(&json!({"unknown": 1})).is_err());
        assert!(Factory.build(&json!({"scope": "everyone"})).is_err());
    }

    #[test]
    fn last_binding_wins_and_thresholds_per_group() {
        let a = Factory.build(&json!({"scope": "key"})).unwrap();
        let b = Factory
            .build(&json!({"mode": "semantic", "semantic": {"threshold": 0.9, "thresholdByGroup": {"g": 0.99}}}))
            .unwrap();
        let chain = Chain::new(vec![
            Arc::new(Entry::new("a", a).with_order(1)),
            Arc::new(Entry::new("b", b).with_order(2)),
        ]);
        let pol = policy_of(&chain).unwrap();
        assert_eq!(pol.mode, CacheMode::Semantic);
        assert_eq!(pol.scope, CacheScope::Team);
        assert!((pol.semantic.threshold_for("g") - 0.99).abs() < 1e-6);
        assert!((pol.semantic.threshold_for("x") - 0.9).abs() < 1e-6);
        assert!(policy_of(&Chain::new(Vec::new())).is_none());
    }
}
