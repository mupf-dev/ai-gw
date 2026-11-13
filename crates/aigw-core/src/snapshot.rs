// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Unveränderlicher Konfigurations-Snapshot für den Hot Path (§14).
//!
//! Wird aus der Datenbank aufgebaut (`aigw_store::load_snapshot`) und enthält vorberechnete
//! Indizes, damit Auth und Routing ohne DB-Zugriff in O(1) laufen. Den jeweils aktuellen
//! Snapshot hält [`SnapshotHandle`] (`ArcSwap`, lock-frei); neu gebaut wird er vom Reloader in
//! `aigw_store::reload`.

mod handle;

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use time::OffsetDateTime;

use crate::resource::{
    ApiKey, CredentialData, Deployment, Id, McpGrant, McpServer, ModelGroup, Organization,
    PluginBinding, Price, ProviderCredential, Team,
};
use crate::types::ProviderKind;

pub use handle::SnapshotHandle;

/// SHA-256 eines API-Keys (Schlüssel des Key-Index).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeyHash(pub [u8; 32]);

impl KeyHash {
    /// Hex-Darstellung (so steht der Hash in der Datenbank).
    pub fn to_hex(&self) -> String {
        use fmt::Write as _;
        let mut s = String::with_capacity(64);
        for b in self.0 {
            let _ = write!(s, "{b:02x}");
        }
        s
    }

    /// Gegenstück zu [`KeyHash::to_hex`].
    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 64 || !s.is_ascii() {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, o) in out.iter_mut().enumerate() {
            *o = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
        }
        Some(Self(out))
    }
}

impl fmt::Debug for KeyHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Nur ein Ausschnitt – der volle Hash ist zwar kein Geheimnis, aber unnötig in Logs.
        write!(f, "KeyHash({}…)", &self.to_hex()[..12])
    }
}

/// Alles, was der Hot Path zu einem authentifizierten Key braucht.
#[derive(Debug, Clone)]
pub struct KeyCtx {
    /// Der Key (Spezifikation + Status).
    pub key: ApiKey,
    /// Team, falls zugeordnet.
    pub team: Option<Arc<Team>>,
    /// Organisation (über das Team).
    pub organization: Option<Arc<Organization>>,
    /// Ablauf (Kopie aus `key.spec`, muss pro Anfrage geprüft werden, da Snapshots altern).
    pub expires_at: Option<OffsetDateTime>,
}

impl KeyCtx {
    /// Ist der Key zum Zeitpunkt `now` gültig?
    pub fn is_valid_at(&self, now: OffsetDateTime) -> bool {
        self.expires_at.is_none_or(|e| now < e)
    }

    /// Darf der Key die Modellgruppe `group` (kanonischer Name) nutzen?
    ///
    /// Leere Freigabelisten bedeuten „alle“; Key- und Team-Liste müssen beide passen.
    pub fn allows_model(&self, group: &ModelGroupCtx) -> bool {
        let matches = |list: &[String]| list.is_empty() || list.iter().any(|m| group.answers_to(m));
        matches(&self.key.spec.allowed_models)
            && self
                .team
                .as_ref()
                .is_none_or(|t| matches(&t.spec.allowed_models))
    }
}

/// Deployment mit aufgelösten Zugangsdaten.
#[derive(Debug, Clone)]
pub struct DeploymentCtx {
    /// Deployment.
    pub deployment: Deployment,
    /// Entschlüsselte Zugangsdaten (falls zugeordnet).
    pub credential: Option<Arc<CredentialData>>,
}

/// Modellgruppe mit ihren aktiven Deployments (nach Priorität, dann Name sortiert).
#[derive(Debug, Clone)]
pub struct ModelGroupCtx {
    /// Modellgruppe.
    pub group: ModelGroup,
    /// Aktive Deployments.
    pub deployments: Vec<DeploymentCtx>,
}

impl ModelGroupCtx {
    /// Hört die Gruppe auf `name` (Name oder Alias)?
    pub fn answers_to(&self, name: &str) -> bool {
        self.group.metadata.name == name || self.group.spec.aliases.iter().any(|a| a == name)
    }
}

/// Vollständiger Konfigurationsstand einer Version.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// `config_version`, aus der der Snapshot gebaut wurde.
    pub version: i64,
    /// Key-Hash → Kontext (nur aktive Keys).
    pub keys: HashMap<KeyHash, Arc<KeyCtx>>,
    /// Name **und** Alias → Modellgruppe.
    pub model_groups: HashMap<String, Arc<ModelGroupCtx>>,
    /// Organisationen nach ID.
    pub organizations: HashMap<Id, Arc<Organization>>,
    /// Teams nach ID.
    pub teams: HashMap<Id, Arc<Team>>,
    /// Zugangsdaten (Metadaten) nach Name.
    pub credentials: HashMap<String, ProviderCredential>,
    /// Entschlüsselte Zugangsdaten nach Name (nur im RAM; z. B. Webhook-Geheimnisse über eine
    /// Credential-Referenz, AIGW-61).
    pub credential_data: HashMap<String, Arc<CredentialData>>,
    /// Preise je (Anbieter, Anbieter-Modell), aufsteigend nach `validFrom`.
    /// Einträge ohne Anbieter stehen unter `None`.
    pub prices: HashMap<(Option<ProviderKind>, String), Vec<Arc<Price>>>,
    /// Plugin-Bindungen, sortiert nach Ebene und `order`.
    pub plugin_bindings: Vec<Arc<PluginBinding>>,
    /// MCP-Server nach Name.
    pub mcp_servers: HashMap<String, Arc<McpServer>>,
    /// MCP-Freigaben.
    pub mcp_grants: Vec<Arc<McpGrant>>,
}

impl Snapshot {
    /// Schlägt einen Key über seinen Hash nach.
    pub fn key(&self, hash: &KeyHash) -> Option<&Arc<KeyCtx>> {
        self.keys.get(hash)
    }

    /// Löst einen Modellnamen (Name oder Alias) auf.
    pub fn model_group(&self, name: &str) -> Option<&Arc<ModelGroupCtx>> {
        self.model_groups.get(name)
    }

    /// Zum Zeitpunkt `at` gültiger Preis eines Anbieter-Modells (spezifisch vor generisch).
    pub fn price_at(
        &self,
        provider: ProviderKind,
        model: &str,
        at: OffsetDateTime,
    ) -> Option<&Arc<Price>> {
        let pick = |p: Option<ProviderKind>| {
            self.prices
                .get(&(p, model.to_owned()))?
                .iter()
                .rev()
                .find(|pr| pr.spec.valid_from <= at)
        };
        pick(Some(provider)).or_else(|| pick(None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_hash_hex_roundtrip() {
        let h = KeyHash([0xab; 32]);
        let hex = h.to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(KeyHash::from_hex(&hex), Some(h));
        assert_eq!(KeyHash::from_hex("zz"), None);
        assert!(format!("{h:?}").starts_with("KeyHash(abababababab"));
    }
}
