// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! GitOps-Manifeste (`apiVersion: aigw/v1alpha1`, `kind`, `metadata`, `spec`).

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::spec::{
    ApiKeySpec, DeploymentSpec, McpGrantSpec, McpServerSpec, ModelGroupSpec, OrganizationSpec,
    PluginBindingSpec, PriceSpec, ProviderCredentialSpec, SecretSpec, TeamSpec, UserSpec,
};
use super::{ObjectMeta, ResourceKind, validate_name};

/// Aktuelle API-Version der Manifeste.
pub const API_VERSION: &str = "aigw/v1alpha1";

/// Spezifikation einer Ressource, unterschieden nach `kind`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum ResourceSpec {
    /// Organisation.
    Organization(OrganizationSpec),
    /// Team.
    Team(TeamSpec),
    /// Nutzer.
    User(UserSpec),
    /// API-Key (Klartext nur per `valueFrom`, sonst erzeugt der Server einen).
    ApiKey(ApiKeySpec),
    /// Zugangsdaten.
    ProviderCredential(ProviderCredentialSpec),
    /// Geheimnis (F149).
    Secret(SecretSpec),
    /// Modellgruppe.
    ModelGroup(ModelGroupSpec),
    /// Deployment.
    Deployment(DeploymentSpec),
    /// Preis.
    Price(PriceSpec),
    /// Plugin-Bindung.
    PluginBinding(PluginBindingSpec),
    /// MCP-Server.
    McpServer(McpServerSpec),
    /// MCP-Freigabe.
    McpGrant(McpGrantSpec),
    /// Tool-Richtlinie (AIGW-87).
    ToolPolicy(crate::tools::ToolPolicySpec),
}

impl ResourceSpec {
    /// Art der Ressource.
    pub fn kind(&self) -> ResourceKind {
        match self {
            Self::Organization(_) => ResourceKind::Organization,
            Self::Team(_) => ResourceKind::Team,
            Self::User(_) => ResourceKind::User,
            Self::ApiKey(_) => ResourceKind::ApiKey,
            Self::ProviderCredential(_) => ResourceKind::ProviderCredential,
            Self::Secret(_) => ResourceKind::Secret,
            Self::ModelGroup(_) => ResourceKind::ModelGroup,
            Self::Deployment(_) => ResourceKind::Deployment,
            Self::Price(_) => ResourceKind::Price,
            Self::PluginBinding(_) => ResourceKind::PluginBinding,
            Self::McpServer(_) => ResourceKind::McpServer,
            Self::McpGrant(_) => ResourceKind::McpGrant,
            Self::ToolPolicy(_) => ResourceKind::ToolPolicy,
        }
    }

    fn to_value(&self) -> serde_json::Result<serde_json::Value> {
        match self {
            Self::Organization(s) => serde_json::to_value(s),
            Self::Team(s) => serde_json::to_value(s),
            Self::User(s) => serde_json::to_value(s),
            Self::ApiKey(s) => serde_json::to_value(s),
            Self::ProviderCredential(s) => serde_json::to_value(s),
            Self::Secret(s) => serde_json::to_value(s),
            Self::ModelGroup(s) => serde_json::to_value(s),
            Self::Deployment(s) => serde_json::to_value(s),
            Self::Price(s) => serde_json::to_value(s),
            Self::PluginBinding(s) => serde_json::to_value(s),
            Self::McpServer(s) => serde_json::to_value(s),
            Self::McpGrant(s) => serde_json::to_value(s),
            Self::ToolPolicy(s) => serde_json::to_value(s),
        }
    }

    fn from_value(kind: ResourceKind, v: serde_json::Value) -> serde_json::Result<Self> {
        use serde_json::from_value as f;
        Ok(match kind {
            ResourceKind::Organization => Self::Organization(f(v)?),
            ResourceKind::Team => Self::Team(f(v)?),
            ResourceKind::User => Self::User(f(v)?),
            ResourceKind::ApiKey => Self::ApiKey(f(v)?),
            ResourceKind::ProviderCredential => Self::ProviderCredential(f(v)?),
            ResourceKind::Secret => Self::Secret(f(v)?),
            ResourceKind::ModelGroup => Self::ModelGroup(f(v)?),
            ResourceKind::Deployment => Self::Deployment(f(v)?),
            ResourceKind::Price => Self::Price(f(v)?),
            ResourceKind::PluginBinding => Self::PluginBinding(f(v)?),
            ResourceKind::McpServer => Self::McpServer(f(v)?),
            ResourceKind::McpGrant => Self::McpGrant(f(v)?),
            ResourceKind::ToolPolicy => Self::ToolPolicy(f(v)?),
        })
    }
}

/// Ein GitOps-Dokument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// Metadaten (`createdAt`/`updatedAt` werden beim Anwenden ignoriert).
    pub metadata: ObjectMeta,
    /// Spezifikation inkl. Art.
    pub spec: ResourceSpec,
}

/// Rohform mit `spec` als JSON-Wert; die Art entscheidet über den Zieltyp.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawManifest {
    api_version: String,
    kind: ResourceKind,
    metadata: ObjectMeta,
    #[serde(default)]
    spec: serde_json::Value,
}

impl Serialize for Manifest {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        RawManifest {
            api_version: API_VERSION.to_owned(),
            kind: self.spec.kind(),
            metadata: self.metadata.clone(),
            spec: self.spec.to_value().map_err(serde::ser::Error::custom)?,
        }
        .serialize(s)
    }
}

impl<'de> Deserialize<'de> for Manifest {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = RawManifest::deserialize(d)?;
        if raw.api_version != API_VERSION {
            return Err(D::Error::custom(format!(
                "nicht unterstützte apiVersion {:?} (erwartet {API_VERSION})",
                raw.api_version
            )));
        }
        let spec = if raw.spec.is_null() {
            serde_json::Value::Object(serde_json::Map::new())
        } else {
            raw.spec
        };
        let spec = ResourceSpec::from_value(raw.kind, spec).map_err(|e| {
            D::Error::custom(format!(
                "{} {:?}: spec ungültig: {e}",
                raw.kind, raw.metadata.name
            ))
        })?;
        Ok(Self {
            metadata: raw.metadata,
            spec,
        })
    }
}

/// Fehler beim Lesen oder Prüfen von Manifesten.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ManifestError {
    /// YAML-Syntax oder Struktur ungültig.
    #[error("YAML ungültig: {0}")]
    Yaml(String),
    /// Inhaltlich ungültig.
    #[error("{kind} {name:?}: {message}")]
    Invalid {
        /// Art.
        kind: ResourceKind,
        /// Name.
        name: String,
        /// Meldung.
        message: String,
    },
}

impl Manifest {
    /// Liest alle Dokumente (`---`-getrennt) aus YAML-Text und prüft sie.
    pub fn parse_yaml(text: &str) -> Result<Vec<Self>, ManifestError> {
        let docs: Vec<Self> =
            serde_saphyr::from_multiple(text).map_err(|e| ManifestError::Yaml(e.to_string()))?;
        for d in &docs {
            d.validate()?;
        }
        Ok(docs)
    }

    /// Schreibt Manifeste als YAML-Mehrfachdokument.
    pub fn to_yaml(docs: &[Self]) -> Result<String, ManifestError> {
        serde_saphyr::to_string_multiple(docs).map_err(|e| ManifestError::Yaml(e.to_string()))
    }

    /// Prüft Name und Geheimnisregel: in YAML nur `valueFrom`, nie Klartext.
    pub fn validate(&self) -> Result<(), ManifestError> {
        let invalid = |message: String| ManifestError::Invalid {
            kind: self.spec.kind(),
            name: self.metadata.name.clone(),
            message,
        };
        validate_name(&self.metadata.name).map_err(invalid)?;
        let data = match &self.spec {
            ResourceSpec::ProviderCredential(c) => Some(&c.data),
            ResourceSpec::Secret(s) => Some(&s.data),
            _ => None,
        };
        for (field, r) in data.into_iter().flatten() {
            if r.value.is_some() || r.value_from.is_none() {
                return Err(invalid(format!(
                    "Feld {field:?}: in YAML nur `valueFrom` (env/file) erlaubt, kein Klartext"
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{RoutingStrategy, ScopeLevel, SecretRef};
    use crate::types::ProviderKind;

    const EXAMPLE: &str = r#"
apiVersion: aigw/v1alpha1
kind: ProviderCredential
metadata:
  name: openai-prod
spec:
  provider: openai
  data:
    apiKey:
      valueFrom:
        env: OPENAI_API_KEY
---
apiVersion: aigw/v1alpha1
kind: ModelGroup
metadata:
  name: gpt-4o
  labels:
    tier: premium
spec:
  aliases: [gpt-4o-latest]
  routing: least-latency
  fallbacks:
    - modelGroup: claude-sonnet
      on: [rate-limit, unavailable]
---
apiVersion: aigw/v1alpha1
kind: Deployment
metadata:
  name: gpt-4o-openai
spec:
  modelGroup: gpt-4o
  provider: openai
  providerModel: gpt-4o-2024-08-06
  credential: openai-prod
  weight: 3
  capacity: { rpm: 500, tpm: 300000 }
---
apiVersion: aigw/v1alpha1
kind: Price
metadata:
  name: gpt-4o-2024-08
spec:
  provider: openai
  model: gpt-4o-2024-08-06
  validFrom: 2024-08-06T00:00:00Z
  perMillionTokens:
    input: "2.50"
    cachedInput: 1.25
    output: 10
---
apiVersion: aigw/v1alpha1
kind: PluginBinding
metadata:
  name: pii-global
spec:
  scope: { level: global }
  plugin: pii
  config:
    mode: mask
"#;

    #[test]
    fn parses_multi_document_yaml() {
        let docs = Manifest::parse_yaml(EXAMPLE).unwrap();
        assert_eq!(docs.len(), 5);
        let ResourceSpec::ModelGroup(mg) = &docs[1].spec else {
            panic!("ModelGroup erwartet")
        };
        assert_eq!(mg.routing, RoutingStrategy::LeastLatency);
        assert_eq!(docs[1].metadata.labels["tier"], "premium");
        let ResourceSpec::Deployment(d) = &docs[2].spec else {
            panic!("Deployment erwartet")
        };
        assert_eq!(d.provider, ProviderKind::OpenAi);
        assert_eq!(d.capacity.tpm, Some(300_000));
        let ResourceSpec::Price(p) = &docs[3].spec else {
            panic!("Price erwartet")
        };
        assert_eq!(
            p.per_million_tokens.cached_input.map(|a| a.to_string()),
            Some("1.25".into())
        );
        let ResourceSpec::PluginBinding(b) = &docs[4].spec else {
            panic!("PluginBinding erwartet")
        };
        assert_eq!(b.scope.level, ScopeLevel::Global);

        // Rundreise: YAML → Manifest → YAML → Manifest
        let yaml = Manifest::to_yaml(&docs).unwrap();
        assert_eq!(Manifest::parse_yaml(&yaml).unwrap(), docs);
    }

    #[test]
    fn rejects_plaintext_secrets_unknown_fields_and_versions() {
        let plain = EXAMPLE
            .split("---")
            .next()
            .unwrap()
            .replace("valueFrom:\n        env: OPENAI_API_KEY", "value: sk-123");
        let err = Manifest::parse_yaml(&plain).unwrap_err().to_string();
        assert!(err.contains("valueFrom"), "{err}");

        let typo =
            "apiVersion: aigw/v1alpha1\nkind: Team\nmetadata: {name: t}\nspec: {organisation: x}\n";
        assert!(Manifest::parse_yaml(typo).is_err());

        let ver = "apiVersion: aigw/v2\nkind: Organization\nmetadata: {name: o}\n";
        assert!(Manifest::parse_yaml(ver).is_err());

        let ok = "apiVersion: aigw/v1alpha1\nkind: Organization\nmetadata: {name: o}\n";
        assert_eq!(Manifest::parse_yaml(ok).unwrap().len(), 1);
    }

    #[test]
    fn secret_ref_from_env() {
        let docs = Manifest::parse_yaml(EXAMPLE).unwrap();
        let ResourceSpec::ProviderCredential(c) = &docs[0].spec else {
            panic!("ProviderCredential erwartet")
        };
        assert_eq!(c.data["apiKey"], SecretRef::env("OPENAI_API_KEY"));
    }
}
