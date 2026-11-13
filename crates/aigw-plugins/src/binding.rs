// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Kettensteuerung einer Plugin-Bindung.
//!
//! `PluginBindingSpec.config` enthält neben der Plugin-Konfiguration vier reservierte
//! Schlüssel, die die Kette auswertet und die nicht an das Plugin gehen:
//!
//! | Schlüssel | Werte | Standard | Bedeutung |
//! |---|---|---|---|
//! | `inherit` | `append` / `override` / `disable` | `append` | Verhältnis zu Bindungen desselben Plugins auf äußeren Ebenen |
//! | `locked` | bool | `false` | innere Ebenen dürfen diese Bindung weder ersetzen noch abschalten |
//! | `failMode` | `open` / `closed` | Plugin-Standard | Verhalten bei Laufzeitfehler, Zeitüberschreitung und ungültiger Konfiguration |
//! | `timeoutMs` | 1–60000 | Plugin-Standard | Zeitlimit asynchroner Haken |

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Map, Value};

/// Reservierte Schlüssel in `config`.
pub const CONTROL_KEYS: [&str; 4] = ["inherit", "locked", "failMode", "timeoutMs"];

/// Größtes zulässiges Zeitlimit eines asynchronen Hakens.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(60);

/// Verhalten bei Fehlern eines Plugins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailMode {
    /// Fehler protokollieren, Plugin überspringen (Anfrage läuft weiter).
    Open,
    /// Anfrage ablehnen (`500 plugin_error`) bzw. Stream mit Fehler beenden.
    Closed,
}

impl FailMode {
    /// Bezeichner.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }
}

/// Verhältnis einer Bindung zu Bindungen desselben Plugins auf äußeren Ebenen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Inherit {
    /// Zusätzlich ausführen (Standard).
    #[default]
    Append,
    /// Geerbte Bindungen desselben Plugins ersetzen.
    Override,
    /// Geerbte Bindungen desselben Plugins abschalten (die Bindung selbst läuft nicht).
    Disable,
}

/// Ausgewertete Kettensteuerung.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Control {
    /// Vererbung.
    pub inherit: Inherit,
    /// Gesperrt für innere Ebenen.
    pub locked: bool,
    /// Fehlermodus (`None` = Standard des Plugins).
    pub fail_mode: Option<FailMode>,
    /// Zeitlimit (`None` = Standard des Plugins).
    pub timeout: Option<Duration>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawControl {
    #[serde(default)]
    inherit: Inherit,
    #[serde(default)]
    locked: bool,
    #[serde(default)]
    fail_mode: Option<FailMode>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

/// Ergebnis von [`split`].
#[derive(Debug, Clone)]
pub struct Split {
    /// Kettensteuerung (Fehler = ungültige reservierte Schlüssel).
    pub control: Result<Control, String>,
    /// Fehlermodus, auch wenn die übrige Steuerung ungültig ist – damit eine ungültige
    /// Bindung mit `failMode: closed` die Kette sperren kann.
    pub fail_mode: Option<FailMode>,
    /// Plugin-Konfiguration (alle nicht reservierten Schlüssel).
    pub plugin_config: Value,
}

/// Trennt `config` in Kettensteuerung und Plugin-Konfiguration.
pub fn split(config: &Map<String, Value>) -> Split {
    let mut control = Map::new();
    let mut rest = Map::new();
    for (k, v) in config {
        if CONTROL_KEYS.contains(&k.as_str()) {
            control.insert(k.clone(), v.clone());
        } else {
            rest.insert(k.clone(), v.clone());
        }
    }
    let fail_mode = control
        .get("failMode")
        .and_then(|v| serde_json::from_value::<FailMode>(v.clone()).ok());
    let parsed = serde_json::from_value::<RawControl>(Value::Object(control))
        .map_err(|e| format!("Kettensteuerung: {e}"))
        .and_then(|raw| {
            let timeout = match raw.timeout_ms {
                None => None,
                Some(0) => return Err("timeoutMs muss > 0 sein".to_owned()),
                Some(ms) if Duration::from_millis(ms) > MAX_TIMEOUT => {
                    return Err(format!(
                        "timeoutMs höchstens {} ms",
                        MAX_TIMEOUT.as_millis()
                    ));
                }
                Some(ms) => Some(Duration::from_millis(ms)),
            };
            Ok(Control {
                inherit: raw.inherit,
                locked: raw.locked,
                fail_mode: raw.fail_mode,
                timeout,
            })
        });
    Split {
        control: parsed,
        fail_mode,
        plugin_config: Value::Object(rest),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        match v {
            Value::Object(m) => m,
            _ => Map::new(),
        }
    }

    #[test]
    fn splits_control_and_plugin_config() {
        let sp = split(&obj(json!({
            "inherit": "override", "locked": true, "failMode": "open", "timeoutMs": 150,
            "set": {"a": "b"}
        })));
        let c = sp.control.unwrap_or_default();
        assert_eq!(c.inherit, Inherit::Override);
        assert!(c.locked);
        assert_eq!(c.fail_mode, Some(FailMode::Open));
        assert_eq!(c.timeout, Some(Duration::from_millis(150)));
        assert_eq!(sp.plugin_config, json!({"set": {"a": "b"}}));
    }

    #[test]
    fn invalid_control_keeps_fail_mode() {
        let sp = split(&obj(json!({"inherit": "sometimes", "failMode": "closed"})));
        assert!(sp.control.is_err());
        assert_eq!(sp.fail_mode, Some(FailMode::Closed));
        assert!(split(&obj(json!({"timeoutMs": 0}))).control.is_err());
        assert!(split(&obj(json!({"timeoutMs": 600_000}))).control.is_err());
    }
}
