// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Lesen der anbieterspezifischen Deployment-Einstellungen (`spec.settings`, camelCase).

use std::time::Duration;

use aigw_core::resource::{EntraAuth, JsonObject};
use aigw_core::snapshot::DeploymentCtx;
use serde_json::Value;

use crate::ConfigError;
use crate::upstream::Timeouts;

/// Einstellungen, die ein Deployment von seiner Modellgruppe erbt, wenn es sie nicht selbst
/// setzt (`ModelGroup.spec.settings`): `defaultMaxTokens`, `timeouts` (F44, als Ganzes),
/// `thinkingBudgetPolicy` (F59), `propagateTraceContext` (F98).
pub const GROUP_INHERITED_SETTINGS: &[&str] = &[
    "defaultMaxTokens",
    "timeouts",
    "thinkingBudgetPolicy",
    "propagateTraceContext",
];

/// Deployment mit den von der Modellgruppe geerbten Einstellungen
/// ([`GROUP_INHERITED_SETTINGS`]); unverändert (ohne Kopie), wenn nichts zu erben ist.
///
/// Aufruf beim Aufbau der Routen (vor [`Registry::build`](crate::Registry::build) bzw.
/// [`Upstream::from_deployment`](crate::Upstream::from_deployment)).
pub fn with_group_defaults<'a>(
    group_settings: &JsonObject,
    dep: &'a DeploymentCtx,
) -> std::borrow::Cow<'a, DeploymentCtx> {
    let own = &dep.deployment.spec.settings;
    let missing: Vec<&&str> = GROUP_INHERITED_SETTINGS
        .iter()
        .filter(|k| !own.contains_key(**k) && group_settings.contains_key(**k))
        .collect();
    if missing.is_empty() {
        return std::borrow::Cow::Borrowed(dep);
    }
    let mut d = dep.clone();
    for k in missing {
        if let Some(v) = group_settings.get(*k) {
            d.deployment
                .spec
                .settings
                .insert((*k).to_owned(), v.clone());
        }
    }
    std::borrow::Cow::Owned(d)
}

/// Deployment mit der Entra-ID-Anmeldung seiner Zugangsdaten (`ProviderCredential.spec.entra`,
/// AIGW-84): fehlende `settings.auth`/`entra*` kommen aus den Zugangsdaten, eigene Einstellungen
/// des Deployments haben Vorrang ([`EntraAuth::apply_to`]). Ohne `entra` unverändert.
///
/// Aufruf beim Aufbau der Routen nach [`with_group_defaults`].
pub fn with_credential_auth<'a>(
    entra: Option<&EntraAuth>,
    dep: std::borrow::Cow<'a, DeploymentCtx>,
) -> std::borrow::Cow<'a, DeploymentCtx> {
    let Some(e) = entra else {
        return dep;
    };
    let mut settings = dep.deployment.spec.settings.clone();
    if !e.apply_to(&mut settings) {
        return dep;
    }
    let mut d = dep.into_owned();
    d.deployment.spec.settings = settings;
    std::borrow::Cow::Owned(d)
}

/// Zugriff auf `spec.settings` mit Fehlermeldungen, die das Deployment nennen.
pub(crate) struct Settings<'a> {
    pub(crate) deployment: &'a str,
    map: &'a JsonObject,
}

impl<'a> Settings<'a> {
    pub(crate) fn of(dep: &'a DeploymentCtx) -> Self {
        Self {
            deployment: &dep.deployment.metadata.name,
            map: &dep.deployment.spec.settings,
        }
    }

    pub(crate) fn invalid(&self, message: impl Into<String>) -> ConfigError {
        ConfigError::Invalid {
            deployment: self.deployment.to_owned(),
            message: message.into(),
        }
    }

    pub(crate) fn missing(&self, what: &'static str) -> ConfigError {
        ConfigError::Missing {
            deployment: self.deployment.to_owned(),
            what,
        }
    }

    pub(crate) fn str(&self, key: &str) -> Result<Option<&'a str>, ConfigError> {
        match self.map.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.as_str())),
            Some(_) => Err(self.invalid(format!("settings.{key} muss ein String sein"))),
        }
    }

    pub(crate) fn bool(&self, key: &str) -> Result<Option<bool>, ConfigError> {
        match self.map.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(_) => Err(self.invalid(format!("settings.{key} muss true/false sein"))),
        }
    }

    /// Positive Ganzzahl.
    pub(crate) fn positive_u64(&self, key: &str) -> Result<Option<u64>, ConfigError> {
        match self.map.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_u64()
                .filter(|n| *n > 0)
                .map(Some)
                .ok_or_else(|| self.invalid(format!("settings.{key} muss eine Zahl > 0 sein"))),
        }
    }

    /// String oder Liste von Strings (z. B. `anthropicBeta`).
    pub(crate) fn str_list(&self, key: &str) -> Result<Vec<&'a str>, ConfigError> {
        let err = || {
            self.invalid(format!(
                "settings.{key} muss ein String oder eine Liste sein"
            ))
        };
        match self.map.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::String(s)) => Ok(s
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect()),
            Some(Value::Array(a)) => a
                .iter()
                .map(|v| v.as_str().map(str::trim).ok_or_else(err))
                .collect(),
            Some(_) => Err(err()),
        }
    }

    /// Rohwert (für Listen und frei übernommene Strukturen).
    pub(crate) fn value(&self, key: &str) -> Option<&'a Value> {
        self.map.get(key)
    }

    pub(crate) fn object(&self, key: &str) -> Result<Option<&'a JsonObject>, ConfigError> {
        match self.map.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Object(o)) => Ok(Some(o)),
            Some(_) => Err(self.invalid(format!("settings.{key} muss ein Objekt sein"))),
        }
    }

    /// `settings.timeouts` = `{connectMs, firstByteMs, idleMs, totalMs}` (alle optional).
    ///
    /// `connectMs` wirkt über eine Pool-Variante des Clients (F187, wie `caFile`).
    pub(crate) fn timeouts(&self) -> Result<Timeouts, ConfigError> {
        let mut t = Timeouts::default();
        let Some(o) = self.object("timeouts")? else {
            return Ok(t);
        };
        for (key, value) in o {
            let ms = value
                .as_u64()
                .filter(|v| *v > 0)
                .ok_or_else(|| self.invalid(format!("settings.timeouts.{key} muss > 0 sein")))?;
            let d = Duration::from_millis(ms);
            match key.as_str() {
                "connectMs" => t.connect = d,
                "firstByteMs" => t.first_byte = d,
                "idleMs" => t.idle = d,
                "totalMs" => t.total = d,
                other => {
                    return Err(self.invalid(format!(
                        "settings.timeouts.{other} unbekannt (erlaubt: connectMs, firstByteMs, idleMs, totalMs)"
                    )));
                }
            }
        }
        Ok(t)
    }
}
