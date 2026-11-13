// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Lesen der anbieterspezifischen Deployment-Einstellungen (`spec.settings`, camelCase).

use std::time::Duration;

use aigw_core::resource::JsonObject;
use aigw_core::snapshot::DeploymentCtx;
use serde_json::Value;

use crate::ConfigError;
use crate::upstream::Timeouts;

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

    pub(crate) fn object(&self, key: &str) -> Result<Option<&'a JsonObject>, ConfigError> {
        match self.map.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Object(o)) => Ok(Some(o)),
            Some(_) => Err(self.invalid(format!("settings.{key} muss ein Objekt sein"))),
        }
    }

    /// `settings.timeouts` = `{firstByteMs, idleMs, totalMs}` (alle optional).
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
                "firstByteMs" => t.first_byte = d,
                "idleMs" => t.idle = d,
                "totalMs" => t.total = d,
                other => {
                    return Err(self.invalid(format!(
                        "settings.timeouts.{other} unbekannt (erlaubt: firstByteMs, idleMs, totalMs)"
                    )));
                }
            }
        }
        Ok(t)
    }
}
