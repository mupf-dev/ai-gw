// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Gemeinsame Hilfen für Tests und Benchmark von `aigw-wasm`.

#![allow(dead_code, unreachable_pub, clippy::unwrap_used, missing_docs)]

use std::sync::Arc;

use aigw_core::config::WasmConfig;
use aigw_core::ir::{ChatRequest, Message, Role};
use aigw_core::resource::{JsonObject, Object, ObjectMeta, PluginBindingSpec, Scope, ScopeLevel};
use aigw_core::snapshot::Snapshot;
use aigw_core::wasm::WasmModule;
use aigw_plugins::{Chain, IndexOptions, PluginIndex, Registry};
use serde_json::Value;

/// Inhalt einer Testdatei (`tests/data/<name>.wasm`).
pub fn wasm(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/data/{name}.wasm",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// Modul mit Platzhalter-SHA (`sha` frei wählbar für Tests des Fingerabdrucks).
pub fn module(name: &str, file: &str, sha: &str) -> WasmModule {
    WasmModule {
        id: uuid_like(name),
        name: name.into(),
        sha256: sha.into(),
        description: None,
        created_by: "test".into(),
        created_at: time::OffsetDateTime::UNIX_EPOCH,
        updated_at: time::OffsetDateTime::UNIX_EPOCH,
        content: bytes::Bytes::from(wasm(file)),
    }
}

fn uuid_like(name: &str) -> aigw_core::resource::Id {
    let mut b = [0u8; 16];
    for (i, c) in name.bytes().enumerate().take(16) {
        b[i] = c;
    }
    aigw_core::resource::Id::from_bytes(b)
}

/// Bindung `name` (global) der Art `wasm`; `config` = Plugin-Konfiguration (mit `module`),
/// `fail_mode`/`timeout_ms` als Kettensteuerung.
pub fn binding(
    name: &str,
    config: Value,
    fail_mode: Option<&str>,
    timeout_ms: Option<u64>,
) -> Arc<Object<PluginBindingSpec>> {
    let config: JsonObject = match config {
        Value::Object(m) => m,
        _ => JsonObject::new(),
    };
    Arc::new(Object {
        id: uuid_like(&format!("b-{name}")),
        metadata: ObjectMeta::named(name),
        spec: PluginBindingSpec {
            inherit: None,
            locked: false,
            fail_mode: fail_mode.map(|f| serde_json::from_value(Value::String(f.into())).unwrap()),
            timeout_ms,
            output_reject_mode: None,
            scope: Scope {
                level: ScopeLevel::Global,
                target: None,
            },
            plugin: "wasm".into(),
            order: 0,
            enabled: true,
            config,
        },
    })
}

/// Registry mit WASM-Laufzeit.
pub fn registry(cfg: &WasmConfig) -> (Registry, Arc<aigw_wasm::WasmRuntime>) {
    let rt = aigw_wasm::WasmRuntime::new(cfg).unwrap();
    let mut r = Registry::with_builtin();
    r.register(Arc::new(aigw_wasm::Factory::new(Some(Arc::clone(&rt)))));
    (r, rt)
}

/// Snapshot mit Modulen und Bindungen.
pub fn snapshot(
    modules: Vec<WasmModule>,
    bindings: Vec<Arc<Object<PluginBindingSpec>>>,
) -> Snapshot {
    let mut s = Snapshot {
        version: 1,
        ..Snapshot::default()
    };
    for m in modules {
        s.wasm_modules.insert(m.name.clone(), Arc::new(m));
    }
    s.plugin_bindings = bindings;
    s
}

/// Globale Kette des Snapshots (und Zahl wirksamer Bindungen).
pub fn chain(snap: &Snapshot, registry: &Registry) -> (Arc<Chain>, usize) {
    let idx = PluginIndex::build(snap, registry, IndexOptions::default());
    (idx.chain(None, None), idx.bindings())
}

/// Anfrage mit einer Nutzernachricht.
pub fn request(text: &str) -> ChatRequest {
    ChatRequest {
        model: "m".into(),
        messages: vec![Message::text(Role::User, text)],
        ..ChatRequest::default()
    }
}
