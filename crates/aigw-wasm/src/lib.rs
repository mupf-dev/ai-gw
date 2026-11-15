// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! WASM-Plugins von ai-gw (AIGW-63, `docs/konzept/17-wasm-plugins.md`).
//!
//! - [`WasmRuntime`]: eine wasmtime-Engine je Prozess (Cranelift, Component Model, Fuel als
//!   CPU-Grenze, Epoch-Interrupt als Zeitgrenze, Pooling-Allocator), Linker mit WASI 0.2
//!   **ohne** Freigaben (kein Dateisystem, keine Sockets, keine Umgebung) und dem
//!   Host-Interface `aigw:plugin/host` (nur `log`), Cache kompilierter Komponenten nach
//!   SHA-256.
//! - [`Factory`]: Plugin-Art `wasm` – sucht das Modul der Bindung im Snapshot, kompiliert es
//!   (Cache), ruft einmal `describe(config)` auf und baut die Instanz. Ein neues Modul unter
//!   gleichem Namen ändert den Fingerabdruck (SHA-256) – der Reload baut die Bindung neu.
//! - Signaturen (AIGW-101): vor dem Kompilieren prüft die Fabrik das Modul gegen
//!   `wasm.trusted_keys` ([`aigw_core::wasm::TrustPolicy::check_module`]: SHA-256 des Inhalts
//!   neu berechnet, Ed25519-Signatur, Pflicht). Nicht ladbare Module machen die Bindung
//!   ungültig (→ `failMode`) – auch wenn sie an der Admin-API vorbei in Datenbank oder
//!   Snapshot-Cache gelangt sind.
//! - Je Aufruf eine frische Instanz (kein Zustand zwischen Anfragen): Anfrage- und
//!   Antwort-Phase laufen über `spawn_blocking` ([`aigw_plugins::Hook::Pending`], Zeitgrenze
//!   der Kette + Epoch-Deadline), die Stream-Phase synchron ([`run_blocking`]).
//!
//! Die WIT-Welt liegt in `crates/aigw-wasm/wit/plugin.wit`.

mod engine;
mod metrics;
mod plugin;

use std::sync::Arc;
use std::time::Duration;

use aigw_plugins::builtin::wasm::{self as cfg, WasmBindingConfig};
use aigw_plugins::{BuildCtx, Phases, Plugin, PluginFactory};
use serde_json::Value;

pub use engine::WasmRuntime;
pub use metrics::WasmMetrics;

use engine::CallLimits;
use plugin::WasmPlugin;

/// Die WIT-Welt der Plugins (zum Ausliefern an Plugin-Autoren, z. B. in der Doku).
pub const WIT: &str = include_str!("../wit/plugin.wit");

const MIB: usize = 1024 * 1024;

/// Führt eine blockierende Funktion außerhalb des asynchronen Kontexts aus.
///
/// WASI-Aufrufe der Komponente (z. B. Schreiben auf stderr bei einem Panic) blockieren intern
/// auf Tokio; auf einem Worker-Thread wäre das ein Fehler. Daher: Multi-Thread-Runtime →
/// `block_in_place`; Current-Thread-Runtime (Tests) → eigener Thread; ohne Runtime → direkt.
pub fn run_blocking<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        Ok(_) => std::thread::scope(|s| match s.spawn(f).join() {
            Ok(r) => r,
            Err(p) => std::panic::resume_unwind(p),
        }),
        Err(_) => f(),
    }
}

/// Fabrik der Plugin-Art `wasm` mit Laufzeit (ersetzt [`cfg::Factory`] im Server).
#[derive(Debug, Clone)]
pub struct Factory {
    rt: Option<Arc<WasmRuntime>>,
}

impl Factory {
    /// Fabrik mit Laufzeit; `None` = WASM abgeschaltet (`wasm.enabled: false`): Bindungen
    /// werden geprüft, Aufrufe schlagen fehl (→ `failMode`).
    pub fn new(rt: Option<Arc<WasmRuntime>>) -> Self {
        Self { rt }
    }

    fn limits(
        &self,
        rt: &WasmRuntime,
        c: &WasmBindingConfig,
        timeout: Duration,
    ) -> Result<CallLimits, String> {
        let g = rt.config();
        let fuel = c.fuel.unwrap_or(g.default_fuel);
        if fuel > g.max_fuel {
            return Err(format!("fuel: höchstens {} (wasm.max_fuel)", g.max_fuel));
        }
        let mem = c.max_memory_mb.unwrap_or(g.default_memory_mb);
        if mem > g.max_memory_mb {
            return Err(format!(
                "maxMemoryMb: höchstens {} (wasm.max_memory_mb)",
                g.max_memory_mb
            ));
        }
        Ok(CallLimits {
            fuel,
            memory: (mem as usize).saturating_mul(MIB),
            timeout,
        })
    }
}

impl PluginFactory for Factory {
    fn name(&self) -> &'static str {
        cfg::NAME
    }

    fn build(&self, config: &Value) -> Result<Arc<dyn Plugin>, String> {
        cfg::Factory.build(config)
    }

    fn build_in(&self, ctx: &BuildCtx<'_>, config: &Value) -> Result<Arc<dyn Plugin>, String> {
        let Some(rt) = &self.rt else {
            return cfg::Factory.build(config);
        };
        let p = WasmBindingConfig::parse(config)?;
        let m = ctx
            .snapshot
            .wasm_modules
            .get(&p.cfg.module)
            .ok_or_else(|| format!("module: unbekanntes WASM-Modul {:?}", p.cfg.module))?;
        if let Some(sha) = &p.cfg.sha256
            && *sha != m.sha256
        {
            return Err(format!(
                "module {:?}: SHA-256 {} erwartet, gespeichert ist {}",
                m.name, sha, m.sha256
            ));
        }
        if let Err(e) = rt.trust().check_module(m) {
            rt.metrics().signature_rejected(e.code());
            return Err(format!("module {:?}: {e}", m.name));
        }
        let limits = self.limits(rt, &p.cfg, ctx.timeout)?;
        let compiled = rt.compile(m)?;
        let module: Arc<str> = Arc::from(m.name.as_str());
        let declared = plugin::describe(
            rt,
            &compiled,
            &module,
            &p.config_json,
            // `describe` läuft beim Aufbau der Kette, nicht je Anfrage: großzügigere Zeitgrenze.
            CallLimits {
                timeout: limits.timeout.max(Duration::from_secs(2)),
                ..limits
            },
        )?;
        let phases = match p.phases {
            Some(want) => Phases {
                request: want.request && declared.request,
                response: want.response && declared.response,
                chunk: want.chunk && declared.chunk,
            },
            None => declared,
        };
        if phases == Phases::default() {
            return Err("keine Phase übrig (Modul meldet keine der gewählten Phasen)".into());
        }
        Ok(Arc::new(WasmPlugin {
            rt: Arc::clone(rt),
            module,
            compiled,
            config_json: Arc::from(p.config_json.as_str()),
            include: p.include_headers,
            phases,
            limits,
        }))
    }

    fn fingerprint(&self, snap: &aigw_core::snapshot::Snapshot, config: &Value) -> String {
        config
            .get("module")
            .and_then(Value::as_str)
            .and_then(|m| snap.wasm_modules.get(m))
            .map(|m| match &m.signature {
                // Neu signiert (gleicher Inhalt) → Prüfergebnis kann sich ändern → neu bauen.
                Some(s) => format!("{}:{}:{}", m.sha256, s.key_id, s.signature),
                None => m.sha256.clone(),
            })
            .unwrap_or_default()
    }

    fn default_timeout(&self) -> Duration {
        cfg::DEFAULT_TIMEOUT
    }
}
