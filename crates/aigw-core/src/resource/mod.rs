// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Ressourcen-Modell (Quelle der Wahrheit für Datenbank, Admin-API und GitOps-YAML).
//!
//! Jede Ressource besteht aus [`ObjectMeta`] (Name, Labels, vom Server gepflegte Felder) und
//! einer `spec`. Gelesene Ressourcen haben zusätzlich eine [`Id`] (UUIDv7). In GitOps-YAML
//! erscheint dieselbe Struktur Kubernetes-artig als [`Manifest`]:
//!
//! ```yaml
//! apiVersion: aigw/v1alpha1
//! kind: ModelGroup
//! metadata:
//!   name: gpt-4o
//! spec:
//!   aliases: [gpt-4o-latest]
//! ```

pub mod money;
pub mod secret;
pub mod spec;

mod manifest;

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use utoipa::ToSchema;

pub use manifest::{API_VERSION, Manifest, ManifestError, ResourceSpec};
pub use money::Amount;
pub use secret::{CredentialData, SecretRef, SecretString, ValueFrom};
pub use spec::*;

/// ID einer Ressource (UUIDv7, zeitlich sortierbar).
pub type Id = uuid::Uuid;

/// Erzeugt eine neue ID (UUIDv7).
pub fn new_id() -> Id {
    uuid::Uuid::now_v7()
}

/// Art einer Ressource (`kind` in YAML).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, ToSchema,
)]
pub enum ResourceKind {
    /// [`OrganizationSpec`]
    Organization,
    /// [`TeamSpec`]
    Team,
    /// [`UserSpec`]
    User,
    /// [`ApiKeySpec`]
    ApiKey,
    /// [`ProviderCredentialSpec`]
    ProviderCredential,
    /// [`SecretSpec`] (F149)
    Secret,
    /// [`ModelGroupSpec`]
    ModelGroup,
    /// [`DeploymentSpec`]
    Deployment,
    /// [`PriceSpec`]
    Price,
    /// [`PluginBindingSpec`]
    PluginBinding,
    /// [`McpServerSpec`]
    McpServer,
    /// [`McpGrantSpec`]
    McpGrant,
    /// [`crate::tools::ToolPolicySpec`] (AIGW-87)
    ToolPolicy,
}

impl ResourceKind {
    /// Alle Arten in Abhängigkeitsreihenfolge (Verweisziele zuerst) – für Import und Export.
    pub const ALL: [Self; 13] = [
        Self::Organization,
        Self::Team,
        Self::User,
        Self::ProviderCredential,
        Self::Secret,
        Self::ModelGroup,
        Self::Deployment,
        Self::Price,
        Self::ApiKey,
        Self::McpServer,
        Self::McpGrant,
        Self::PluginBinding,
        Self::ToolPolicy,
    ];

    /// Name wie in YAML (`kind`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Organization => "Organization",
            Self::Team => "Team",
            Self::User => "User",
            Self::ApiKey => "ApiKey",
            Self::ProviderCredential => "ProviderCredential",
            Self::Secret => "Secret",
            Self::ModelGroup => "ModelGroup",
            Self::Deployment => "Deployment",
            Self::Price => "Price",
            Self::PluginBinding => "PluginBinding",
            Self::McpServer => "McpServer",
            Self::McpGrant => "McpGrant",
            Self::ToolPolicy => "ToolPolicy",
        }
    }
}

impl ResourceKind {
    /// Bezeichner im Audit-Log (`kind`, F115: einheitlich kebab-case wie `admin-session`).
    pub fn audit_name(self) -> &'static str {
        match self {
            Self::Organization => "organization",
            Self::Team => "team",
            Self::User => "user",
            Self::ApiKey => "api-key",
            Self::ProviderCredential => "provider-credential",
            Self::Secret => "secret",
            Self::ModelGroup => "model-group",
            Self::Deployment => "deployment",
            Self::Price => "price",
            Self::PluginBinding => "plugin-binding",
            Self::McpServer => "mcp-server",
            Self::McpGrant => "mcp-grant",
            Self::ToolPolicy => "tool-policy",
        }
    }

    /// Art zu einem Namen in YAML-Schreibweise (`ApiKey`) oder Audit-Schreibweise
    /// (`api-key`).
    pub fn parse_any(s: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|k| k.as_str() == s || k.audit_name() == s)
    }
}

impl fmt::Display for ResourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Metadaten einer Ressource.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObjectMeta {
    /// Eindeutiger Name je Art (DNS-Label-artig: `a-z0-9-._`, max. 128 Zeichen).
    pub name: String,
    /// Labels zum Filtern/Gruppieren.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// Angelegt (vom Server gesetzt, beim Import ignoriert).
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub created_at: Option<OffsetDateTime>,
    /// Zuletzt geändert (vom Server gesetzt, beim Import ignoriert).
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub updated_at: Option<OffsetDateTime>,
}

impl ObjectMeta {
    /// Metadaten nur mit Namen.
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }
}

/// Prüft einen Ressourcennamen.
pub fn validate_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-._".contains(&b))
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric());
    if ok {
        Ok(())
    } else {
        Err(format!(
            "ungültiger Name {name:?}: erlaubt sind a-z, 0-9, '-', '.', '_' (max. 128, beginnt alphanumerisch)"
        ))
    }
}

/// Gelesene Ressource: ID, Metadaten, Spezifikation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Object<S> {
    /// ID (UUIDv7).
    #[schema(value_type = String, format = Uuid)]
    pub id: Id,
    /// Metadaten.
    pub metadata: ObjectMeta,
    /// Spezifikation.
    pub spec: S,
}

/// Organisation.
pub type Organization = Object<OrganizationSpec>;
/// Team.
pub type Team = Object<TeamSpec>;
/// Nutzer.
pub type User = Object<UserSpec>;
/// Modellgruppe.
pub type ModelGroup = Object<ModelGroupSpec>;
/// Deployment.
pub type Deployment = Object<DeploymentSpec>;
/// Preis.
pub type Price = Object<PriceSpec>;
/// Plugin-Bindung.
pub type PluginBinding = Object<PluginBindingSpec>;
/// MCP-Server.
pub type McpServer = Object<McpServerSpec>;
/// MCP-Freigabe.
pub type McpGrant = Object<McpGrantSpec>;
/// Zugangsdaten (gelesene Sicht ohne Werte).
pub type ProviderCredential = Object<ProviderCredentialInfo>;
/// Geheimnis (gelesene Sicht ohne Werte, F149).
pub type Secret = Object<SecretInfo>;

/// API-Key (gelesene Sicht): Spezifikation plus vom Server verwaltete Felder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiKey {
    /// ID (UUIDv7).
    #[schema(value_type = String, format = Uuid)]
    pub id: Id,
    /// Metadaten.
    pub metadata: ObjectMeta,
    /// Spezifikation.
    pub spec: ApiKeySpec,
    /// Präfix, Zustand, letzte Nutzung.
    pub status: ApiKeyStatus,
}

/// Ergebnis der Key-Erzeugung: der Klartext wird **nur hier** einmalig herausgegeben.
#[derive(Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreatedApiKey {
    /// Gespeicherter Key.
    pub key: ApiKey,
    /// Klartext `sk-aigw-…` (nicht wiederherstellbar).
    pub secret: String,
}

// AIGW-58: `Debug` ohne Klartext (Schutz vor versehentlichem Loggen).
impl fmt::Debug for CreatedApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CreatedApiKey")
            .field("key", &self.key)
            .field("secret", &"***")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(validate_name("gpt-4o").is_ok());
        assert!(validate_name("team.platform_1").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("-x").is_err());
        assert!(validate_name("Upper").is_err());
        assert!(validate_name("a b").is_err());
    }

    #[test]
    fn ids_are_v7() {
        let a = new_id();
        let b = new_id();
        assert_eq!(a.get_version_num(), 7);
        assert!(a < b, "UUIDv7 ist zeitlich sortiert");
    }
}
