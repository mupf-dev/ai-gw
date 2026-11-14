// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Registry: Plugin-Name → Fabrik, die aus der (validierten) Konfiguration einer Bindung eine
//! Plugin-Instanz baut.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use aigw_core::upstream::UpstreamPolicy;

use crate::binding::{FailMode, OutputRejectMode};
use crate::builtin;
use crate::host::{NoHost, PluginHost};
use crate::metrics::PluginMetrics;
use crate::plugin::Plugin;

/// Standard-Zeitlimit asynchroner Haken, wenn weder Bindung noch Fabrik eines vorgeben.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(1);

/// Fabrik eines Plugins.
pub trait PluginFactory: Send + Sync + 'static {
    /// Name, unter dem Bindungen das Plugin ansprechen.
    fn name(&self) -> &'static str;

    /// Baut eine Instanz. `config` ist die Plugin-Konfiguration ohne die reservierten
    /// Schlüssel der Kettensteuerung; unbekannte Felder sind ein Fehler
    /// (`deny_unknown_fields`).
    fn build(&self, config: &Value) -> Result<Arc<dyn Plugin>, String>;

    /// Baut eine Instanz für den Snapshot `snap` (beim Aufbau der Ketten). Standard:
    /// [`PluginFactory::build`]. Plugins, die Objekte des Snapshots brauchen (z. B. WASM-Module,
    /// AIGW-63), überschreiben das.
    fn build_in(&self, ctx: &BuildCtx<'_>, config: &Value) -> Result<Arc<dyn Plugin>, String> {
        let _ = ctx;
        self.build(config)
    }

    /// Fingerabdruck der Snapshot-Objekte, von denen eine Instanz abhängt (z. B. SHA-256 des
    /// WASM-Moduls). Ändert er sich bei gleicher Konfiguration, baut der Reload die Bindung
    /// neu. Standard: leer (nur die Konfiguration zählt).
    fn fingerprint(&self, snap: &aigw_core::snapshot::Snapshot, config: &Value) -> String {
        let _ = (snap, config);
        String::new()
    }

    /// Standard-Fehlermodus (F148: `closed`; Ausnahme `webhook`: `open`). Je Bindung über
    /// `failMode` überschreibbar.
    fn default_fail_mode(&self) -> FailMode {
        FailMode::Closed
    }

    /// Standard-Zeitlimit asynchroner Haken.
    fn default_timeout(&self) -> Duration {
        DEFAULT_TIMEOUT
    }
}

/// Umgebung von [`PluginFactory::build_in`] beim Aufbau der Ketten.
#[derive(Debug, Clone, Copy)]
pub struct BuildCtx<'a> {
    /// Snapshot, aus dem die Ketten gebaut werden.
    pub snapshot: &'a aigw_core::snapshot::Snapshot,
    /// Wirksames Zeitlimit der Bindung (`timeoutMs` bzw. Standard der Plugin-Art).
    pub timeout: Duration,
}

/// Wirksame Kettensteuerung einer Bindung (für Admin-API/UI, F148).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectiveControl {
    /// Fehlermodus.
    pub fail_mode: FailMode,
    /// `true` = Standard der Plugin-Art (kein `failMode` in der Bindung).
    pub fail_mode_default: bool,
    /// Zeitlimit asynchroner Haken.
    pub timeout: Duration,
    /// Wirkung einer Ausgabe-Ablehnung.
    pub output_reject: OutputRejectMode,
}

/// Bekannte Plugins.
#[derive(Clone, Default)]
pub struct Registry {
    factories: BTreeMap<&'static str, Arc<dyn PluginFactory>>,
    metrics: Arc<PluginMetrics>,
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.factories.keys()).finish()
    }
}

impl Registry {
    /// Leere Registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registry mit den eingebauten Plugins (`header`, `request-transform`, `webhook`, `cache`,
    /// `pii`, `prompt-filter`, `content-safety`). `content-safety` hat hier keinen Gateway-Host
    /// (Konfiguration prüfbar, Aufrufe schlagen fehl) – im Betrieb
    /// [`Registry::with_builtin_host`].
    pub fn with_builtin() -> Self {
        Self::with_builtin_host(Arc::new(NoHost::default()))
    }

    /// Wie [`Registry::with_builtin`], `content-safety` mit den Diensten des Gateways
    /// (Zugangsdaten, HTTP mit SSRF-Regeln, Modellgruppen).
    pub fn with_builtin_host(host: Arc<dyn PluginHost>) -> Self {
        let mut r = Self::new();
        r.register(Arc::new(builtin::cache::Factory));
        r.register(Arc::new(builtin::header::Factory));
        r.register(Arc::new(builtin::request_transform::Factory));
        r.register(Arc::new(builtin::webhook::Factory::default()));
        r.register(Arc::new(builtin::pii::Factory));
        r.register(Arc::new(builtin::prompt_filter::Factory));
        r.register(Arc::new(builtin::content_safety::Factory::new(host)));
        // Nur Konfigurationsprüfung; die Laufzeit (`aigw-wasm`) ersetzt die Fabrik im Server.
        r.register(Arc::new(builtin::wasm::Factory));
        r
    }

    /// Registry nur zum **Prüfen** von Bindungen (Admin-API, GitOps-Plan, F118/F165): die
    /// eingebauten Plugins mit den SSRF-Regeln `policy` (aus `admin.allow_*_upstreams`), ohne
    /// Gateway-Dienste (Aufrufe schlagen fehl; Credential-Referenzen werden erst zur Laufzeit
    /// aufgelöst).
    pub fn for_validation(policy: &UpstreamPolicy) -> Self {
        let mut r = Self::with_builtin_host(Arc::new(NoHost::with_policy(policy.clone())));
        r.register(Arc::new(builtin::webhook::Factory::with_policy(
            policy.clone(),
        )));
        r
    }

    /// Fehlermodus-Zähler aller mit dieser Registry gebauten Ketten
    /// (`aigw_plugin_failmode_triggered_total`).
    pub fn metrics(&self) -> &Arc<PluginMetrics> {
        &self.metrics
    }

    /// Registriert (oder ersetzt) eine Fabrik.
    pub fn register(&mut self, factory: Arc<dyn PluginFactory>) {
        self.factories.insert(factory.name(), factory);
    }

    /// Fabrik zu einem Plugin-Namen.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn PluginFactory>> {
        self.factories.get(name)
    }

    /// Namen aller registrierten Plugins.
    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.factories.keys().copied()
    }

    /// Prüft eine Bindung (Steuerfelder + Plugin-Konfiguration, F117), ohne sie zu verwenden –
    /// z. B. für Admin-API und `aigw config validate`.
    pub fn validate_spec(
        &self,
        spec: &aigw_core::resource::PluginBindingSpec,
    ) -> Result<(), String> {
        self.validate(&spec.plugin, &spec.merged_config())
    }

    /// Wirksame Kettensteuerung einer Bindung (Steuerfelder, F117).
    pub fn effective_spec(
        &self,
        spec: &aigw_core::resource::PluginBindingSpec,
    ) -> Result<EffectiveControl, String> {
        self.effective(&spec.plugin, &spec.merged_config())
    }

    /// Prüft eine Bindungs-Konfiguration in der Form vor F117 (Steuerschlüssel in `config`).
    pub fn validate(
        &self,
        plugin: &str,
        config: &serde_json::Map<String, Value>,
    ) -> Result<(), String> {
        let factory = self
            .get(plugin)
            .ok_or_else(|| format!("unbekanntes Plugin {plugin:?}"))?;
        let split = crate::binding::split(config);
        split.control?;
        factory.build(&split.plugin_config).map(drop)
    }

    /// Wirksame Kettensteuerung einer Bindung (Standardwerte der Plugin-Art eingesetzt).
    /// Fehler bei unbekanntem Plugin oder ungültigen reservierten Schlüsseln.
    pub fn effective(
        &self,
        plugin: &str,
        config: &serde_json::Map<String, Value>,
    ) -> Result<EffectiveControl, String> {
        let factory = self
            .get(plugin)
            .ok_or_else(|| format!("unbekanntes Plugin {plugin:?}"))?;
        let c = crate::binding::split(config).control?;
        Ok(EffectiveControl {
            fail_mode: c.fail_mode.unwrap_or_else(|| factory.default_fail_mode()),
            fail_mode_default: c.fail_mode.is_none(),
            timeout: c
                .timeout
                .unwrap_or_else(|| factory.default_timeout())
                .min(crate::binding::MAX_TIMEOUT),
            output_reject: c.output_reject,
        })
    }
}
