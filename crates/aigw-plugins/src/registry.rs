// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Registry: Plugin-Name → Fabrik, die aus der (validierten) Konfiguration einer Bindung eine
//! Plugin-Instanz baut.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::binding::FailMode;
use crate::builtin;
use crate::host::{NoHost, PluginHost};
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

    /// Standard-Fehlermodus (Sicherheits-Plugins: `closed`).
    fn default_fail_mode(&self) -> FailMode {
        FailMode::Closed
    }

    /// Standard-Zeitlimit asynchroner Haken.
    fn default_timeout(&self) -> Duration {
        DEFAULT_TIMEOUT
    }
}

/// Bekannte Plugins.
#[derive(Clone, Default)]
pub struct Registry {
    factories: BTreeMap<&'static str, Arc<dyn PluginFactory>>,
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
        r
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

    /// Prüft eine Bindungs-Konfiguration (Kettensteuerung + Plugin-Konfiguration), ohne sie
    /// zu verwenden – z. B. für Admin-API und `aigw config validate`.
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
}
