// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Dienste des Gateways für Plugins, die externe Dienste oder Modellgruppen aufrufen
//! (`content-safety`, AIGW-38).
//!
//! Plugins bekommen bewusst keinen eigenen HTTP-Client und keine Geheimnisse in ihrer
//! Konfiguration: Zugangsdaten werden über den Namen einer `ProviderCredential` zur Laufzeit
//! aus dem aktuellen Snapshot aufgelöst (Rotation wirkt sofort), Ziele unterliegen den
//! SSRF-Regeln aus `aigw_core::upstream` (auch nach der Namensauflösung), und Modellaufrufe
//! laufen über das Routing des Gateways selbst (ohne Plugin-Kette, ohne Client-Limits).

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use aigw_core::ir::{ChatRequest, ChatResponse};
use aigw_core::resource::CredentialData;
use aigw_core::upstream::UpstreamPolicy;

use crate::plugin::{BoxFuture, PluginError};

/// Antwort eines HTTP-Aufrufs.
#[derive(Debug, Clone)]
pub struct HttpReply {
    /// Statuscode.
    pub status: u16,
    /// Body (begrenzt).
    pub body: Vec<u8>,
}

/// HTTP-Aufruf (immer `POST` mit JSON-Body).
#[derive(Debug, Clone)]
pub struct HttpCall {
    /// Ziel-URL (vorab mit [`PluginHost::check_endpoint`] geprüft).
    pub url: String,
    /// Zusätzliche Header (z. B. Authentifizierung; Werte nie loggen).
    pub headers: Vec<(&'static str, String)>,
    /// JSON-Body.
    pub body: Vec<u8>,
    /// Zeitlimit für den ganzen Aufruf.
    pub timeout: Duration,
}

/// Dienste des Gateways für Plugins.
pub trait PluginHost: Send + Sync + fmt::Debug + 'static {
    /// Entschlüsselte Zugangsdaten zum Namen (aktueller Snapshot).
    fn credential(&self, name: &str) -> Option<Arc<CredentialData>>;

    /// Entschlüsseltes Geheimnis (`Secret`, F149) zum Namen (aktueller Snapshot).
    fn secret(&self, _name: &str) -> Option<Arc<CredentialData>> {
        None
    }

    /// Prüft eine Endpunkt-URL gegen die SSRF-Regeln (beim Bau der Bindung).
    fn check_endpoint(&self, url: &str) -> Result<(), String>;

    /// `POST` mit JSON an einen Endpunkt; prüft die aufgelöste Adresse erneut.
    fn post_json(&self, call: HttpCall) -> BoxFuture<'static, Result<HttpReply, PluginError>>;

    /// Chat über eine Modellgruppe des Gateways (ohne Plugins und Client-Limits).
    fn chat(
        &self,
        model_group: &str,
        req: ChatRequest,
    ) -> BoxFuture<'static, Result<ChatResponse, PluginError>>;
}

/// Host ohne Dienste (z. B. für `Registry::validate` und Tests): Endpunkte werden nach den
/// strengen Standardregeln geprüft, Aufrufe schlagen fehl.
#[derive(Debug, Clone, Default)]
pub struct NoHost {
    policy: UpstreamPolicy,
}

impl NoHost {
    /// Host mit eigenen SSRF-Regeln (für die Validierung in Admin-API/GitOps).
    pub fn with_policy(policy: UpstreamPolicy) -> Self {
        Self { policy }
    }
}

impl PluginHost for NoHost {
    fn credential(&self, _name: &str) -> Option<Arc<CredentialData>> {
        None
    }

    fn check_endpoint(&self, url: &str) -> Result<(), String> {
        self.policy.check_endpoint(url)
    }

    fn post_json(&self, _call: HttpCall) -> BoxFuture<'static, Result<HttpReply, PluginError>> {
        Box::pin(async { Err(PluginError::new("kein Gateway-Host für externe Aufrufe")) })
    }

    fn chat(
        &self,
        _model_group: &str,
        _req: ChatRequest,
    ) -> BoxFuture<'static, Result<ChatResponse, PluginError>> {
        Box::pin(async { Err(PluginError::new("kein Gateway-Host für Modellaufrufe")) })
    }
}
