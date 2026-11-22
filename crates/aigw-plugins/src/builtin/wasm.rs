// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Plugin `wasm` (AIGW-63, `docs/konzept/17-wasm-plugins.md`): führt eine hochgeladene
//! WebAssembly-Komponente (WIT-Welt `aigw:plugin/plugin`) in einer Sandbox aus.
//!
//! ```yaml
//! plugin: wasm
//! config:
//!   module: keyword-guard          # Name des Moduls (Admin-API /wasm-modules)
//!   sha256: 3f0c…                  # optional: nur dieses Modul (GitOps: Name + SHA)
//!   config: {block: [geheim]}      # beliebiges JSON, geht als Text an das Modul
//!   phases: [request, response]    # optional: Phasen einschränken (Standard: die des Moduls)
//!   includeHeaders: [x-client-app] # Client-Header für die Anfrage-Phase (nie Zugangsdaten)
//!   fuel: 50000000                 # CPU-Grenze je Aufruf (Standard wasm.default_fuel)
//!   maxMemoryMb: 32                # Speichergrenze je Instanz (Standard wasm.default_memory_mb)
//!   timeoutMs: 50                  # Kettensteuerung: Zeitgrenze je Aufruf (Standard 100 ms)
//!   failMode: closed               # Kettensteuerung (Standard closed)
//! ```
//!
//! Hier steht nur die **Konfiguration** und ihre Prüfung (Admin-API, GitOps). Die Laufzeit
//! (wasmtime, Sandbox, Cache) liegt im Crate `aigw-wasm`, das im Server diese Fabrik ersetzt;
//! ohne Laufzeit schlägt jeder Aufruf fehl (→ `failMode`).

use std::sync::Arc;
use std::time::Duration;

use http::HeaderName;
use serde::Deserialize;
use serde_json::Value;

use crate::builtin::webhook::SECRET_HEADERS;
use crate::plugin::{Flow, Hook, Phases, Plugin, PluginError, ReqCtx};
use crate::registry::PluginFactory;

/// Name der Plugin-Art.
pub const NAME: &str = "wasm";
/// Standard-Zeitgrenze je Aufruf.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(100);
/// Höchstgröße von `config` (JSON-Text) in Byte.
pub const MAX_CONFIG_BYTES: usize = 64 * 1024;

/// Phase, die eine Bindung zulässt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PhaseName {
    /// Anfrage.
    Request,
    /// Antwort (ganz bzw. gepuffert).
    Response,
    /// Stream-Ereignisse.
    Chunk,
}

/// Konfiguration einer Bindung der Art `wasm` (ohne Kettensteuerung).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WasmBindingConfig {
    /// Name des Moduls.
    pub module: String,
    /// Erwarteter SHA-256 des Moduls (klein normalisiert); `None` = jede Version.
    #[serde(default)]
    pub sha256: Option<String>,
    /// Konfiguration für das Modul (beliebiges JSON).
    #[serde(default = "empty_object")]
    pub config: Value,
    /// Phasen einschränken (Schnittmenge mit den Phasen des Moduls).
    #[serde(default)]
    pub phases: Option<Vec<PhaseName>>,
    /// Client-Header, die das Modul in der Anfrage-Phase sieht.
    #[serde(default)]
    pub include_headers: Vec<String>,
    /// Fuel je Aufruf.
    #[serde(default)]
    pub fuel: Option<u64>,
    /// Speichergrenze je Instanz (MiB).
    #[serde(default)]
    pub max_memory_mb: Option<u32>,
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

/// Geprüfte Bindungs-Konfiguration.
#[derive(Debug, Clone)]
pub struct Parsed {
    /// Rohwerte.
    pub cfg: WasmBindingConfig,
    /// `config` als JSON-Text (so geht es an das Modul).
    pub config_json: String,
    /// Zugelassene Phasen (`None` = alle, die das Modul meldet).
    pub phases: Option<Phases>,
    /// Header für die Anfrage-Phase.
    pub include_headers: Vec<HeaderName>,
}

impl WasmBindingConfig {
    /// Liest und prüft die Konfiguration (unabhängig von Laufzeit und Datenbank).
    pub fn parse(config: &Value) -> Result<Parsed, String> {
        let mut cfg: Self = serde_json::from_value(config.clone()).map_err(|e| e.to_string())?;
        aigw_core::resource::validate_name(&cfg.module).map_err(|e| format!("module: {e}"))?;
        if let Some(sha) = &cfg.sha256 {
            cfg.sha256 = Some(aigw_core::wasm::normalize_sha256(sha)?);
        }
        let config_json = serde_json::to_string(&cfg.config).map_err(|e| e.to_string())?;
        if config_json.len() > MAX_CONFIG_BYTES {
            return Err(format!("config: höchstens {MAX_CONFIG_BYTES} Byte JSON"));
        }
        let phases = match &cfg.phases {
            None => None,
            Some(p) if p.is_empty() => {
                return Err("phases: mindestens eine Phase (request, response, chunk)".into());
            }
            Some(p) => Some(Phases {
                request: p.contains(&PhaseName::Request),
                response: p.contains(&PhaseName::Response),
                chunk: p.contains(&PhaseName::Chunk),
            }),
        };
        if cfg.fuel == Some(0) {
            return Err("fuel: mindestens 1".into());
        }
        if cfg.max_memory_mb == Some(0) {
            return Err("maxMemoryMb: mindestens 1".into());
        }
        let mut include_headers = Vec::with_capacity(cfg.include_headers.len());
        for h in &cfg.include_headers {
            let name = HeaderName::from_bytes(h.trim().as_bytes())
                .map_err(|_| format!("includeHeaders: ungültiger Header {h:?}"))?;
            if SECRET_HEADERS.contains(&name.as_str()) {
                return Err(format!(
                    "includeHeaders: {name} darf nicht an Plugins gehen (Zugangsdaten)"
                ));
            }
            include_headers.push(name);
        }
        Ok(Parsed {
            cfg,
            config_json,
            phases,
            include_headers,
        })
    }
}

/// Fabrik ohne Laufzeit: prüft nur die Konfiguration (Admin-API, GitOps, `aigw config
/// validate`). Gebaute Instanzen schlagen bei jedem Aufruf fehl.
#[derive(Debug, Clone, Copy, Default)]
pub struct Factory;

impl PluginFactory for Factory {
    fn name(&self) -> &'static str {
        NAME
    }

    fn build(&self, config: &Value) -> Result<Arc<dyn Plugin>, String> {
        WasmBindingConfig::parse(config)?;
        Ok(Arc::new(Unavailable))
    }

    fn default_timeout(&self) -> Duration {
        DEFAULT_TIMEOUT
    }
}

/// Platzhalter ohne Laufzeit.
#[derive(Debug)]
struct Unavailable;

impl Plugin for Unavailable {
    fn name(&self) -> &'static str {
        NAME
    }

    fn phases(&self) -> Phases {
        Phases::REQUEST
    }

    fn on_request<'a>(&'a self, _ctx: &'a mut ReqCtx<'_>) -> Hook<'a, Flow> {
        Hook::Ready(Err(PluginError::new(
            "WASM-Laufzeit nicht verfügbar (Build ohne Feature `wasm` oder wasm.enabled: false)",
        )))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_and_validates() {
        let p = WasmBindingConfig::parse(&json!({
            "module": "keyword-guard",
            "sha256": "AB".repeat(32),
            "config": {"block": ["x"]},
            "phases": ["request", "chunk"],
            "includeHeaders": ["x-client-app"]
        }))
        .unwrap();
        assert_eq!(p.cfg.sha256.as_deref(), Some("ab".repeat(32).as_str()));
        assert_eq!(p.config_json, r#"{"block":["x"]}"#);
        let ph = p.phases.unwrap();
        assert!(ph.request && ph.chunk && !ph.response);
        for bad in [
            json!({}),
            json!({"module": "Bad Name"}),
            json!({"module": "m", "sha256": "xyz"}),
            json!({"module": "m", "phases": []}),
            json!({"module": "m", "includeHeaders": ["authorization"]}),
            json!({"module": "m", "fuel": 0}),
            json!({"module": "m", "unknown": 1}),
        ] {
            assert!(WasmBindingConfig::parse(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn factory_without_runtime_validates_only() {
        assert!(Factory.build(&json!({"module": "m"})).is_ok());
        assert!(Factory.build(&json!({"module": ""})).is_err());
    }
}
