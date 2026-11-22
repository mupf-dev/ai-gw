// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Laufzeit: Engine (Cranelift, Fuel, Epoch-Interrupt, Pooling-Allocator), Linker mit
//! eingeschränktem WASI, Cache kompilierter Komponenten und Ausführung je Aufruf.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use aigw_core::config::WasmConfig;
use aigw_core::wasm::WasmModule;
use wasmtime::component::{
    Component, ComponentExportIndex, InstancePre, Linker, ResourceTable, Val,
};
use wasmtime::{
    Config, Engine, InstanceAllocationStrategy, OptLevel, PoolingAllocationConfig, ResourceLimiter,
    Store, StoreContextMut, Trap,
};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::metrics::WasmMetrics;

/// Name des Host-Interfaces in der WIT-Welt.
pub(crate) const HOST_INTERFACE: &str = "aigw:plugin/host@0.1.0";
/// Höchstzahl Log-Meldungen je Aufruf.
const MAX_LOGS: u32 = 16;
/// Höchstlänge einer Log-Meldung.
const MAX_LOG_BYTES: usize = 1024;
const MIB: usize = 1024 * 1024;

/// Grenzen eines Aufrufs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CallLimits {
    /// Fuel.
    pub(crate) fuel: u64,
    /// Bytes je linearem Speicher.
    pub(crate) memory: usize,
    /// Zeitgrenze.
    pub(crate) timeout: Duration,
}

/// Fehler eines Aufrufs (Art für Metriken und Meldung).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CallError {
    /// Fuel aufgebraucht.
    Fuel,
    /// Zeitgrenze (Epoch) überschritten.
    Timeout,
    /// Speichergrenze überschritten.
    Memory,
    /// Trap (Panic, `unreachable`, Speicherzugriff …).
    Trap(String),
    /// Instanz nicht anlegbar (Pool erschöpft …).
    Instantiate(String),
    /// Das Modul meldet einen Fehler (`result::err`).
    Guest(String),
    /// Ausgabe ungültig (Typ, JSON, verbotene Änderung).
    Output(String),
}

impl CallError {
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Fuel => "fuel",
            Self::Timeout => "timeout",
            Self::Memory => "memory",
            Self::Trap(_) => "trap",
            Self::Instantiate(_) => "instantiate",
            Self::Guest(_) => "guest",
            Self::Output(_) => "output",
        }
    }

    pub(crate) fn message(&self, module: &str) -> String {
        match self {
            Self::Fuel => format!("WASM-Modul {module:?}: Fuel-Grenze (CPU) überschritten"),
            Self::Timeout => format!("WASM-Modul {module:?}: Zeitgrenze überschritten"),
            Self::Memory => format!("WASM-Modul {module:?}: Speichergrenze überschritten"),
            Self::Trap(m) => format!("WASM-Modul {module:?}: Abbruch (Trap): {m}"),
            Self::Instantiate(m) => format!("WASM-Modul {module:?}: Instanz nicht anlegbar: {m}"),
            Self::Guest(m) => format!("WASM-Modul {module:?} meldet Fehler: {m}"),
            Self::Output(m) => format!("WASM-Modul {module:?}: ungültige Ausgabe: {m}"),
        }
    }
}

fn truncate(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push('…');
    }
    s
}

/// Speichergrenze je Store (merkt sich die Überschreitung für die Fehlerart).
struct MemLimiter {
    max: usize,
    exceeded: bool,
}

impl ResourceLimiter for MemLimiter {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if desired > self.max {
            self.exceeded = true;
            return Err(wasmtime::format_err!(
                "Speichergrenze {} MiB überschritten",
                self.max / MIB
            ));
        }
        Ok(true)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(desired <= 100_000)
    }

    fn instances(&self) -> usize {
        64
    }

    fn tables(&self) -> usize {
        64
    }

    fn memories(&self) -> usize {
        16
    }
}

/// Zustand eines Stores (je Aufruf).
pub(crate) struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
    limiter: MemLimiter,
    logs: u32,
    module: Arc<str>,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// Kompilierte Komponente.
pub(crate) struct Compiled {
    pub(crate) sha256: String,
    pre: InstancePre<HostState>,
    describe: ComponentExportIndex,
    handle: ComponentExportIndex,
}

impl std::fmt::Debug for Compiled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Compiled")
            .field("sha256", &self.sha256)
            .finish_non_exhaustive()
    }
}

/// Exportierte Funktion.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Export {
    Describe,
    Handle,
}

#[derive(Default)]
struct Cache {
    map: HashMap<String, (Arc<Compiled>, u64)>,
    clock: u64,
}

/// WASM-Laufzeit des Gateways (eine je Prozess).
pub struct WasmRuntime {
    engine: Engine,
    linker: Linker<HostState>,
    cfg: WasmConfig,
    trust: aigw_core::wasm::TrustPolicy,
    pooling: bool,
    cache: Mutex<Cache>,
    metrics: Arc<WasmMetrics>,
    stop: Arc<AtomicBool>,
}

impl std::fmt::Debug for WasmRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmRuntime")
            .field("pooling", &self.pooling)
            .finish_non_exhaustive()
    }
}

impl Drop for WasmRuntime {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn engine_config(cfg: &WasmConfig, pooling: bool) -> Config {
    let mut c = Config::new();
    c.wasm_component_model(true);
    c.consume_fuel(true);
    c.epoch_interruption(true);
    c.cranelift_opt_level(OptLevel::Speed);
    c.max_wasm_stack(512 * 1024);
    if pooling {
        let n = cfg.pool_instances;
        let mut p = PoolingAllocationConfig::default();
        p.total_component_instances(n);
        p.total_core_instances(n.saturating_mul(4));
        p.total_memories(n.saturating_mul(2));
        p.total_tables(n.saturating_mul(4));
        p.max_memory_size((cfg.max_memory_mb as usize).saturating_mul(MIB));
        c.allocation_strategy(InstanceAllocationStrategy::Pooling(p));
    }
    c
}

impl WasmRuntime {
    /// Legt die Laufzeit an (Engine, Linker, Epoch-Takt). Der Pooling-Allocator fällt auf
    /// Instanzen bei Bedarf zurück, wenn das System den Adressraum nicht reservieren kann.
    pub fn new(cfg: &WasmConfig) -> Result<Arc<Self>, String> {
        cfg.validate()?;
        let trust = aigw_core::wasm::TrustPolicy::from_config(cfg)?;
        let (engine, pooling) = match Engine::new(&engine_config(cfg, cfg.pooling)) {
            Ok(e) => (e, cfg.pooling),
            Err(e) if cfg.pooling => {
                tracing::warn!(
                    error = %e,
                    "WASM: Pooling-Allocator nicht verfügbar – Instanzen werden bei Bedarf angelegt"
                );
                (
                    Engine::new(&engine_config(cfg, false))
                        .map_err(|e| format!("WASM-Engine: {e}"))?,
                    false,
                )
            }
            Err(e) => return Err(format!("WASM-Engine: {e}")),
        };
        let mut linker = Linker::<HostState>::new(&engine);
        // WASI 0.2 ohne Freigaben: keine Verzeichnisse, keine Umgebung, keine Sockets
        // (Adressprüfung lehnt alles ab), stdout/stderr verworfen. Nur Uhr und Zufall.
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|e| format!("WASM-Linker (WASI): {e}"))?;
        linker
            .instance(HOST_INTERFACE)
            .and_then(|mut i| {
                i.func_wrap(
                    "log",
                    |mut store: StoreContextMut<'_, HostState>, (level, msg): (u8, String)| {
                        let st = store.data_mut();
                        st.logs += 1;
                        if st.logs <= MAX_LOGS {
                            let msg = truncate(msg, MAX_LOG_BYTES);
                            let module = &*st.module;
                            match level {
                                0 => tracing::debug!(wasm_module = module, "{msg}"),
                                1 => tracing::info!(wasm_module = module, "{msg}"),
                                2 => tracing::warn!(wasm_module = module, "{msg}"),
                                _ => tracing::error!(wasm_module = module, "{msg}"),
                            }
                        }
                        Ok(())
                    },
                )
            })
            .map_err(|e| format!("WASM-Linker (host): {e}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        {
            let engine = engine.clone();
            let stop = Arc::clone(&stop);
            let tick = Duration::from_millis(cfg.epoch_tick_ms);
            std::thread::Builder::new()
                .name("aigw-wasm-epoch".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(tick);
                        engine.increment_epoch();
                    }
                })
                .map_err(|e| format!("WASM-Epoch-Takt: {e}"))?;
        }
        Ok(Arc::new(Self {
            engine,
            linker,
            cfg: cfg.clone(),
            trust,
            pooling,
            cache: Mutex::new(Cache::default()),
            metrics: Arc::new(WasmMetrics::default()),
            stop,
        }))
    }

    /// Metriken.
    pub fn metrics(&self) -> &Arc<WasmMetrics> {
        &self.metrics
    }

    /// Konfiguration.
    pub fn config(&self) -> &WasmConfig {
        &self.cfg
    }

    /// Signatur-Richtlinie (`wasm.trusted_keys`, AIGW-101).
    pub fn trust(&self) -> &aigw_core::wasm::TrustPolicy {
        &self.trust
    }

    /// Läuft der Pooling-Allocator?
    pub fn pooling(&self) -> bool {
        self.pooling
    }

    /// Kompiliert eine Komponente (oder holt sie aus dem Cache nach SHA-256) und prüft
    /// Importe (nur WASI 0.2 und `aigw:plugin/host`) und Exporte (`describe`, `handle`).
    pub(crate) fn compile(&self, m: &WasmModule) -> Result<Arc<Compiled>, String> {
        {
            let mut c = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
            c.clock += 1;
            let now = c.clock;
            if let Some((hit, used)) = c.map.get_mut(&m.sha256) {
                *used = now;
                self.metrics.record_compile("cached");
                return Ok(Arc::clone(hit));
            }
        }
        let started = Instant::now();
        let compiled = self.compile_uncached(m);
        self.metrics
            .record_compile(if compiled.is_ok() { "ok" } else { "error" });
        let compiled = Arc::new(compiled?);
        tracing::info!(
            wasm_module = %m.name,
            sha256 = %m.sha256,
            ms = started.elapsed().as_millis() as u64,
            "WASM-Modul kompiliert"
        );
        let mut c = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        let now = c.clock;
        c.map.insert(m.sha256.clone(), (Arc::clone(&compiled), now));
        while c.map.len() > self.cfg.compiled_cache_entries.max(1) {
            let oldest = c
                .map
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => c.map.remove(&k),
                None => break,
            };
        }
        Ok(compiled)
    }

    fn compile_uncached(&self, m: &WasmModule) -> Result<Compiled, String> {
        if !aigw_core::wasm::looks_like_component(&m.content) {
            return Err("keine WebAssembly-Komponente (Component Model)".into());
        }
        let component = Component::new(&self.engine, &m.content).map_err(|e| {
            format!(
                "Kompilieren fehlgeschlagen: {}",
                truncate(format!("{e:#}"), 512)
            )
        })?;
        let pre = self.linker.instantiate_pre(&component).map_err(|e| {
            format!(
                "Importe nicht erfüllbar (erlaubt: WASI 0.2 ohne Freigaben, {HOST_INTERFACE}): {}",
                truncate(format!("{e:#}"), 512)
            )
        })?;
        let describe = component
            .get_export_index(None, "describe")
            .ok_or("Export `describe` fehlt (WIT-Welt aigw:plugin/plugin)")?;
        let handle = component
            .get_export_index(None, "handle")
            .ok_or("Export `handle` fehlt (WIT-Welt aigw:plugin/plugin)")?;
        Ok(Compiled {
            sha256: m.sha256.clone(),
            pre,
            describe,
            handle,
        })
    }

    /// Führt `export` in einer frischen Instanz aus (blockierend; nie direkt auf einem
    /// Tokio-Worker aufrufen, siehe [`crate::run_blocking`]). Liefert Ergebnis und Fuel.
    pub(crate) fn call(
        &self,
        c: &Compiled,
        export: Export,
        module: &Arc<str>,
        arg: Val,
        limits: CallLimits,
    ) -> Result<(Val, u64), CallError> {
        let state = HostState {
            wasi: WasiCtxBuilder::new()
                .allow_tcp(false)
                .allow_udp(false)
                .allow_ip_name_lookup(false)
                .build(),
            table: ResourceTable::new(),
            limiter: MemLimiter {
                max: limits.memory,
                exceeded: false,
            },
            logs: 0,
            module: Arc::clone(module),
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|s| &mut s.limiter);
        store
            .set_fuel(limits.fuel)
            .map_err(|e| CallError::Instantiate(e.to_string()))?;
        let tick = self.cfg.epoch_tick_ms.max(1);
        let ticks = u64::try_from(limits.timeout.as_millis())
            .unwrap_or(u64::MAX)
            .div_ceil(tick)
            .max(1)
            .saturating_add(1);
        store.set_epoch_deadline(ticks);
        store.epoch_deadline_trap();
        let instance = c
            .pre
            .instantiate(&mut store)
            .map_err(|e| classify(&store, e, true))?;
        let idx = match export {
            Export::Describe => &c.describe,
            Export::Handle => &c.handle,
        };
        let func = instance
            .get_func(&mut store, idx)
            .ok_or_else(|| CallError::Instantiate("Export nicht gefunden".into()))?;
        let mut results = [Val::Bool(false)];
        func.call(&mut store, &[arg], &mut results)
            .map_err(|e| classify(&store, e, false))?;
        let used = limits.fuel.saturating_sub(store.get_fuel().unwrap_or(0));
        let [result] = results;
        Ok((result, used))
    }
}

fn classify(store: &Store<HostState>, e: wasmtime::Error, instantiate: bool) -> CallError {
    if store.data().limiter.exceeded {
        return CallError::Memory;
    }
    match e.downcast_ref::<Trap>() {
        Some(Trap::OutOfFuel) => CallError::Fuel,
        Some(Trap::Interrupt) => CallError::Timeout,
        Some(t) => CallError::Trap(t.to_string()),
        None if instantiate => CallError::Instantiate(truncate(format!("{e:#}"), 512)),
        None => CallError::Trap(truncate(format!("{e:#}"), 512)),
    }
}
