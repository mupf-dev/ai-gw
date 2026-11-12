// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Anbieter-Adapter (Stub).
//!
//! Jeder Upstream-Anbieter implementiert [`Provider`]. Die Formatübersetzung
//! (Tools, Vision, Streaming-Events, Usage) folgt mit dem Architekturkonzept.

use std::collections::HashMap;
use std::sync::Arc;

use aigw_core::types::ProviderKind;

/// Ein konfigurierter Upstream-Anbieter.
pub trait Provider: Send + Sync + std::fmt::Debug {
    /// Eindeutiger Name der Anbieter-Instanz aus der Konfiguration.
    fn name(&self) -> &str;
    /// Art des Anbieters.
    fn kind(&self) -> ProviderKind;
}

/// Verzeichnis der konfigurierten Anbieter (liegt im RAM, E4).
#[derive(Debug, Default, Clone)]
pub struct Registry {
    providers: HashMap<String, Arc<dyn Provider>>,
}

impl Registry {
    /// Registriert einen Anbieter; ein gleichnamiger wird ersetzt.
    pub fn insert(&mut self, provider: Arc<dyn Provider>) {
        self.providers.insert(provider.name().to_owned(), provider);
    }

    /// Sucht einen Anbieter über seinen Namen.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Provider>> {
        self.providers.get(name)
    }

    /// Anzahl der registrierten Anbieter.
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// `true`, wenn kein Anbieter registriert ist.
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Dummy;

    impl Provider for Dummy {
        fn name(&self) -> &str {
            "dummy"
        }
        fn kind(&self) -> ProviderKind {
            ProviderKind::OpenAiCompatible
        }
    }

    #[test]
    fn registry_finds_provider_by_name() {
        let mut reg = Registry::default();
        assert!(reg.is_empty());
        reg.insert(Arc::new(Dummy));
        assert_eq!(reg.len(), 1);
        assert_eq!(
            reg.get("dummy").map(|p| p.kind()),
            Some(ProviderKind::OpenAiCompatible)
        );
        assert!(reg.get("other").is_none());
    }
}
