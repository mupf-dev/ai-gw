// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Abbildung der Spezifikationen auf Tabellenspalten.

use aigw_core::observe::PayloadMode;
use aigw_core::resource::{
    Amount, ApiKeySpec, Capacity, DeploymentSpec, McpGrantSpec, McpServerSpec, ModelGroupSpec,
    OrganizationSpec, PluginBindingSpec, PriceSpec, ResourceKind, RoutingStrategy, Scope,
    ScopeLevel, TeamSpec, UserSpec,
};
use aigw_core::types::ProviderKind;
use serde::de::DeserializeOwned;

use super::{Col, PlainResource, RefCol, Resource, Row, level_kind, trunc_millis};
use crate::StoreError;

fn provider(row: &Row<'_>, col: &str) -> Result<ProviderKind, StoreError> {
    let s = row.text(col)?;
    ProviderKind::parse(&s)
        .ok_or_else(|| StoreError::Corrupt(format!("unbekannter Anbieter {s:?}")))
}

/// Liest ein serde-Enum aus seiner Textdarstellung.
fn enum_text<T: DeserializeOwned>(row: &Row<'_>, col: &str) -> Result<T, StoreError> {
    Ok(serde_json::from_value(serde_json::Value::String(
        row.text(col)?,
    ))?)
}

/// Textdarstellung eines serde-Enums.
fn enum_col<T: serde::Serialize>(v: &T) -> Result<Col, StoreError> {
    match serde_json::to_value(v)? {
        serde_json::Value::String(s) => Ok(Col::text(s)),
        other => Err(StoreError::Invalid(format!(
            "kein einfacher Enum-Wert: {other}"
        ))),
    }
}

fn u64_col(v: Option<u64>) -> Result<Col, StoreError> {
    v.map(|v| i64::try_from(v).map_err(|_| StoreError::Invalid(format!("Wert {v} zu groß"))))
        .transpose()
        .map(Col::opt_int)
}

/// Verweis über eine Ebene (Plugin-Bindung, MCP-Freigabe).
fn scope_ref(scope: &Scope) -> Result<Col, StoreError> {
    match (level_kind(scope.level), &scope.target) {
        (None, None) => Ok(Col::Ref(None)),
        (None, Some(_)) => Err(StoreError::Invalid(
            "Ebene `global` hat kein Ziel (`target` weglassen)".into(),
        )),
        (Some(k), Some(t)) => Ok(Col::reference(k, Some(t))),
        (Some(_), None) => Err(StoreError::Invalid(format!(
            "Ebene `{}` braucht ein Ziel (`target`)",
            scope.level.as_str()
        ))),
    }
}

fn scope_from_row(row: &Row<'_>, level_col: &str, target_col: &str) -> Result<Scope, StoreError> {
    let level = row.text(level_col)?;
    let level = ScopeLevel::parse(&level)
        .ok_or_else(|| StoreError::Corrupt(format!("unbekannte Ebene {level:?}")))?;
    Ok(Scope {
        level,
        target: row.ref_name(target_col)?,
    })
}

impl Resource for OrganizationSpec {
    const KIND: ResourceKind = ResourceKind::Organization;

    fn to_columns(&self) -> Result<Vec<(&'static str, Col)>, StoreError> {
        Ok(vec![
            ("display_name", Col::opt_text(self.display_name.clone())),
            ("limits", Col::json(&self.limits)?),
            ("budget", Col::opt_json(self.budget.as_ref())?),
            ("metadata", Col::json(&self.metadata)?),
            (
                "payload_mode",
                Col::opt_text(self.payload_mode.map(PayloadMode::as_str)),
            ),
            ("expose_reasoning", Col::opt_bool(self.expose_reasoning)),
        ])
    }

    fn from_row(row: &Row<'_>) -> Result<Self, StoreError> {
        Ok(Self {
            display_name: row.opt_text("display_name")?,
            limits: row.json("limits")?,
            budget: row.opt_json("budget")?,
            metadata: row.json("metadata")?,
            payload_mode: payload_mode(row)?,
            expose_reasoning: row.opt_bool("expose_reasoning")?,
        })
    }
}
impl PlainResource for OrganizationSpec {}

/// Spalte `payload_mode` (F30); unbekannte Werte gelten als beschädigt.
fn payload_mode(row: &Row<'_>) -> Result<Option<PayloadMode>, StoreError> {
    row.opt_text("payload_mode")?
        .map(|s| {
            serde_json::from_value(serde_json::Value::String(s.clone()))
                .map_err(|_| StoreError::Corrupt(format!("payload_mode {s:?} unbekannt")))
        })
        .transpose()
}

impl Resource for TeamSpec {
    const KIND: ResourceKind = ResourceKind::Team;
    const REFS: &'static [RefCol] = &[RefCol::Fixed("organization_id", ResourceKind::Organization)];

    fn to_columns(&self) -> Result<Vec<(&'static str, Col)>, StoreError> {
        Ok(vec![
            (
                "organization_id",
                Col::reference(ResourceKind::Organization, Some(&self.organization)),
            ),
            ("display_name", Col::opt_text(self.display_name.clone())),
            ("allowed_models", Col::json(&self.allowed_models)?),
            ("limits", Col::json(&self.limits)?),
            ("budget", Col::opt_json(self.budget.as_ref())?),
            ("metadata", Col::json(&self.metadata)?),
            (
                "payload_mode",
                Col::opt_text(self.payload_mode.map(PayloadMode::as_str)),
            ),
            ("expose_reasoning", Col::opt_bool(self.expose_reasoning)),
        ])
    }

    fn from_row(row: &Row<'_>) -> Result<Self, StoreError> {
        Ok(Self {
            organization: row.ref_name("organization_id")?.unwrap_or_default(),
            display_name: row.opt_text("display_name")?,
            allowed_models: row.json("allowed_models")?,
            limits: row.json("limits")?,
            budget: row.opt_json("budget")?,
            metadata: row.json("metadata")?,
            payload_mode: payload_mode(row)?,
            expose_reasoning: row.opt_bool("expose_reasoning")?,
        })
    }
}
impl PlainResource for TeamSpec {}

impl Resource for UserSpec {
    const KIND: ResourceKind = ResourceKind::User;

    fn to_columns(&self) -> Result<Vec<(&'static str, Col)>, StoreError> {
        Ok(vec![
            ("oidc_issuer", Col::opt_text(self.oidc_issuer.clone())),
            ("oidc_subject", Col::opt_text(self.oidc_subject.clone())),
            ("email", Col::opt_text(self.email.clone())),
            ("display_name", Col::opt_text(self.display_name.clone())),
            ("roles", Col::json(&self.roles)?),
        ])
    }

    fn from_row(row: &Row<'_>) -> Result<Self, StoreError> {
        Ok(Self {
            oidc_issuer: row.opt_text("oidc_issuer")?,
            oidc_subject: row.opt_text("oidc_subject")?,
            email: row.opt_text("email")?,
            display_name: row.opt_text("display_name")?,
            roles: row.json("roles")?,
        })
    }

    fn validate(&self) -> Result<(), StoreError> {
        if self.oidc_issuer.is_some() != self.oidc_subject.is_some() {
            return Err(StoreError::Invalid(
                "oidcIssuer und oidcSubject nur gemeinsam angeben".into(),
            ));
        }
        Ok(())
    }
}
impl PlainResource for UserSpec {}

/// API-Keys: nur die Spezifikationsspalten; Präfix/Hash/Zustand pflegt `api_key.rs`.
impl Resource for ApiKeySpec {
    const KIND: ResourceKind = ResourceKind::ApiKey;
    const REFS: &'static [RefCol] = &[
        RefCol::Fixed("organization_id", ResourceKind::Organization),
        RefCol::Fixed("team_id", ResourceKind::Team),
        RefCol::Fixed("user_id", ResourceKind::User),
    ];
    const SEARCH: &'static [&'static str] = &["prefix"];

    fn to_columns(&self) -> Result<Vec<(&'static str, Col)>, StoreError> {
        Ok(vec![
            (
                "organization_id",
                Col::reference(ResourceKind::Organization, Some(&self.organization)),
            ),
            (
                "team_id",
                Col::reference(ResourceKind::Team, self.team.as_deref()),
            ),
            (
                "user_id",
                Col::reference(ResourceKind::User, self.user.as_deref()),
            ),
            ("expires_at", Col::opt_time(self.expires_at)),
            ("allowed_models", Col::json(&self.allowed_models)?),
            ("limits", Col::json(&self.limits)?),
            ("budget", Col::opt_json(self.budget.as_ref())?),
            ("metadata", Col::json(&self.metadata)?),
            (
                "payload_mode",
                Col::opt_text(self.payload_mode.map(PayloadMode::as_str)),
            ),
            ("expose_reasoning", Col::opt_bool(self.expose_reasoning)),
        ])
    }

    fn from_row(row: &Row<'_>) -> Result<Self, StoreError> {
        Ok(Self {
            organization: row.ref_name("organization_id")?.unwrap_or_default(),
            team: row.ref_name("team_id")?,
            user: row.ref_name("user_id")?,
            expires_at: row.opt_time("expires_at")?,
            allowed_models: row.json("allowed_models")?,
            limits: row.json("limits")?,
            budget: row.opt_json("budget")?,
            metadata: row.json("metadata")?,
            payload_mode: payload_mode(row)?,
            expose_reasoning: row.opt_bool("expose_reasoning")?,
        })
    }
}

impl Resource for ModelGroupSpec {
    const KIND: ResourceKind = ResourceKind::ModelGroup;

    fn to_columns(&self) -> Result<Vec<(&'static str, Col)>, StoreError> {
        Ok(vec![
            ("aliases", Col::json(&self.aliases)?),
            ("routing", Col::text(self.routing.as_str())),
            ("fallbacks", Col::json(&self.fallbacks)?),
            ("limits", Col::json(&self.limits)?),
            ("settings", Col::json(&self.settings)?),
        ])
    }

    fn from_row(row: &Row<'_>) -> Result<Self, StoreError> {
        Ok(Self {
            aliases: row.json("aliases")?,
            routing: enum_text::<RoutingStrategy>(row, "routing")?,
            fallbacks: row.json("fallbacks")?,
            limits: row.json("limits")?,
            settings: row.json("settings")?,
        })
    }

    fn validate(&self) -> Result<(), StoreError> {
        for a in &self.aliases {
            if a.is_empty() || a.len() > 256 {
                return Err(StoreError::Invalid(format!("ungültiger Alias {a:?}")));
            }
        }
        Ok(())
    }

    fn aliases(&self) -> Option<&[String]> {
        Some(&self.aliases)
    }
}
impl PlainResource for ModelGroupSpec {}

impl Resource for DeploymentSpec {
    const KIND: ResourceKind = ResourceKind::Deployment;
    const REFS: &'static [RefCol] = &[
        RefCol::Fixed("model_group_id", ResourceKind::ModelGroup),
        RefCol::Fixed("credential_id", ResourceKind::ProviderCredential),
    ];
    const SEARCH: &'static [&'static str] = &["provider_model"];

    fn to_columns(&self) -> Result<Vec<(&'static str, Col)>, StoreError> {
        Ok(vec![
            (
                "model_group_id",
                Col::reference(ResourceKind::ModelGroup, Some(&self.model_group)),
            ),
            ("provider", Col::text(self.provider.as_str())),
            ("endpoint", Col::opt_text(self.endpoint.clone())),
            ("provider_model", Col::text(self.provider_model.clone())),
            (
                "credential_id",
                Col::reference(ResourceKind::ProviderCredential, self.credential.as_deref()),
            ),
            ("weight", Col::int(i64::from(self.weight))),
            ("priority", Col::int(i64::from(self.priority))),
            ("rpm", u64_col(self.capacity.rpm)?),
            ("tpm", u64_col(self.capacity.tpm)?),
            ("enabled", Col::bool(self.enabled)),
            ("settings", Col::json(&self.settings)?),
        ])
    }

    fn from_row(row: &Row<'_>) -> Result<Self, StoreError> {
        Ok(Self {
            model_group: row.ref_name("model_group_id")?.unwrap_or_default(),
            provider: provider(row, "provider")?,
            endpoint: row.opt_text("endpoint")?,
            provider_model: row.text("provider_model")?,
            credential: row.ref_name("credential_id")?,
            weight: row.u32("weight")?,
            priority: row.u32("priority")?,
            capacity: Capacity {
                rpm: row.opt_u64("rpm")?,
                tpm: row.opt_u64("tpm")?,
            },
            enabled: row.bool("enabled")?,
            settings: row.json("settings")?,
        })
    }
}
impl PlainResource for DeploymentSpec {}

impl Resource for PriceSpec {
    const KIND: ResourceKind = ResourceKind::Price;
    const SEARCH: &'static [&'static str] = &["model"];

    fn to_columns(&self) -> Result<Vec<(&'static str, Col)>, StoreError> {
        Ok(vec![
            (
                "provider",
                Col::text(self.provider.map(ProviderKind::as_str).unwrap_or_default()),
            ),
            ("model", Col::text(self.model.clone())),
            ("valid_from", Col::time(self.valid_from)),
            ("token_prices", Col::json(&self.per_million_tokens)?),
            (
                "per_request",
                Col::opt_int(self.per_request.map(Amount::nanos)),
            ),
            ("per_image", Col::opt_int(self.per_image.map(Amount::nanos))),
        ])
    }

    fn from_row(row: &Row<'_>) -> Result<Self, StoreError> {
        let p = row.text("provider")?;
        Ok(Self {
            provider: if p.is_empty() {
                None
            } else {
                Some(provider(row, "provider")?)
            },
            model: row.text("model")?,
            valid_from: row.time("valid_from")?,
            per_million_tokens: row.json("token_prices")?,
            per_request: row.opt_int("per_request")?.map(Amount::from_nanos),
            per_image: row.opt_int("per_image")?.map(Amount::from_nanos),
        })
    }

    fn validate(&self) -> Result<(), StoreError> {
        if trunc_millis(self.valid_from) != self.valid_from {
            return Err(StoreError::Invalid(
                "validFrom: höchstens Millisekunden-Genauigkeit".into(),
            ));
        }
        Ok(())
    }
}
impl PlainResource for PriceSpec {}

impl Resource for PluginBindingSpec {
    const KIND: ResourceKind = ResourceKind::PluginBinding;
    const REFS: &'static [RefCol] = &[RefCol::ByLevel("target_id", "level")];

    fn to_columns(&self) -> Result<Vec<(&'static str, Col)>, StoreError> {
        Ok(vec![
            ("level", Col::text(self.scope.level.as_str())),
            ("target_id", scope_ref(&self.scope)?),
            ("plugin", Col::text(self.plugin.clone())),
            ("position", Col::int(i64::from(self.order))),
            ("enabled", Col::bool(self.enabled)),
            ("config", Col::json(&self.config)?),
        ])
    }

    fn from_row(row: &Row<'_>) -> Result<Self, StoreError> {
        Ok(Self {
            scope: scope_from_row(row, "level", "target_id")?,
            plugin: row.text("plugin")?,
            order: i32::try_from(row.int("position")?)
                .map_err(|_| StoreError::Corrupt("position außerhalb i32".into()))?,
            enabled: row.bool("enabled")?,
            config: row.json("config")?,
        })
    }

    fn validate(&self) -> Result<(), StoreError> {
        scope_ref(&self.scope).map(drop)
    }
}
impl PlainResource for PluginBindingSpec {}

impl Resource for McpServerSpec {
    const KIND: ResourceKind = ResourceKind::McpServer;
    const REFS: &'static [RefCol] = &[RefCol::Fixed(
        "credential_id",
        ResourceKind::ProviderCredential,
    )];
    const SEARCH: &'static [&'static str] = &["url"];

    fn to_columns(&self) -> Result<Vec<(&'static str, Col)>, StoreError> {
        Ok(vec![
            ("url", Col::text(self.url.clone())),
            ("transport", enum_col(&self.transport)?),
            (
                "credential_id",
                Col::reference(ResourceKind::ProviderCredential, self.credential.as_deref()),
            ),
            ("enabled", Col::bool(self.enabled)),
            ("settings", Col::json(&self.settings)?),
        ])
    }

    fn from_row(row: &Row<'_>) -> Result<Self, StoreError> {
        Ok(Self {
            url: row.text("url")?,
            transport: enum_text(row, "transport")?,
            credential: row.ref_name("credential_id")?,
            enabled: row.bool("enabled")?,
            settings: row.json("settings")?,
        })
    }
}
impl PlainResource for McpServerSpec {}

impl Resource for McpGrantSpec {
    const KIND: ResourceKind = ResourceKind::McpGrant;
    const REFS: &'static [RefCol] = &[
        RefCol::Fixed("mcp_server_id", ResourceKind::McpServer),
        RefCol::ByLevel("subject_id", "subject_level"),
    ];

    fn to_columns(&self) -> Result<Vec<(&'static str, Col)>, StoreError> {
        Ok(vec![
            (
                "mcp_server_id",
                Col::reference(ResourceKind::McpServer, Some(&self.server)),
            ),
            ("subject_level", Col::text(self.subject.level.as_str())),
            ("subject_id", scope_ref(&self.subject)?),
            ("tools", Col::json(&self.tools)?),
        ])
    }

    fn from_row(row: &Row<'_>) -> Result<Self, StoreError> {
        Ok(Self {
            server: row.ref_name("mcp_server_id")?.unwrap_or_default(),
            subject: scope_from_row(row, "subject_level", "subject_id")?,
            tools: row.json("tools")?,
        })
    }

    fn validate(&self) -> Result<(), StoreError> {
        if !matches!(self.subject.level, ScopeLevel::Team | ScopeLevel::ApiKey) {
            return Err(StoreError::Invalid(
                "MCP-Freigaben gelten nur für `team` oder `api-key`".into(),
            ));
        }
        scope_ref(&self.subject).map(drop)
    }
}
impl PlainResource for McpGrantSpec {}
