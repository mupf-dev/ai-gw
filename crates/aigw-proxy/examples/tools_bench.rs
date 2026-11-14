// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Mikro-Benchmark des Tool-Schritts (AIGW-87): Kosten je Anfrage ohne Tools, mit 30 Tools
//! (Claude-Code-ähnlich: Beschreibungen ~600 Zeichen, Schemas mit 8 Feldern) ohne Inventar und
//! Richtlinien, mit 6 Richtlinien und mit Inventar. Keine Richtlinie entfernt ein Tool, die
//! Anfrage bleibt also unverändert und wird ohne Klonen wiederverwendet.
//!
//! `cargo run --release -p aigw-proxy --example tools_bench`

#![allow(clippy::print_stdout)]

use std::hint::black_box;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use aigw_core::ir::{ChatRequest, FunctionTool, Message, Role, Tool, ToolKind};
use aigw_core::observe::PayloadMode;
use aigw_core::resource::{Object, ObjectMeta, Scope};
use aigw_core::snapshot::Snapshot;
use aigw_core::tools::{
    ToolAction, ToolBatch, ToolFlushResult, ToolInventorySink, ToolPolicyIndex, ToolPolicySpec,
};
use aigw_proxy::AuthContext;
use aigw_proxy::tools::{ToolInventory, apply_request};
use axum::http::HeaderMap;
use serde_json::json;

#[derive(Debug, Default)]
struct NullSink(Mutex<u64>);

impl ToolInventorySink for NullSink {
    fn flush<'a>(
        &'a self,
        batch: ToolBatch,
    ) -> aigw_core::tools::BoxFuture<'a, Result<ToolFlushResult, String>> {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) += batch.items.len() as u64;
        Box::pin(async { Ok(ToolFlushResult::default()) })
    }
}

fn request(n: usize) -> ChatRequest {
    let tools = (0..n)
        .map(|i| Tool {
            kind: ToolKind::Function(FunctionTool {
                name: format!("mcp__server{}__tool_{i}", i % 4),
                description: Some(format!(
                    "{i}: {}",
                    "Liest Dateien und Verzeichnisse. ".repeat(18)
                )),
                parameters: Some(json!({"type": "object", "properties": {
                    "path": {"type": "string", "description": "Pfad"},
                    "limit": {"type": "integer"}, "offset": {"type": "integer"},
                    "pattern": {"type": "string"}, "recursive": {"type": "boolean"},
                    "mode": {"type": "string", "enum": ["a", "b", "c"]},
                    "tags": {"type": "array", "items": {"type": "string"}},
                    "opts": {"type": "object", "properties": {"x": {"type": "number"}}}},
                    "required": ["path"]})),
                strict: None,
                extra: Default::default(),
            }),
            cache_control: None,
            extra: Default::default(),
        })
        .collect();
    ChatRequest {
        model: "m".into(),
        messages: vec![Message::text(Role::User, "hallo")],
        tools,
        ..ChatRequest::default()
    }
}

fn policies() -> AuthContext {
    let mk = |name: &str, tools: &[&str], action| Object {
        id: aigw_core::resource::new_id(),
        metadata: ObjectMeta::named(name),
        spec: ToolPolicySpec {
            scope: Scope::global(),
            tools: tools.iter().map(|s| (*s).to_owned()).collect(),
            servers: Vec::new(),
            action,
            unapproved: None,
            locked: false,
            enabled: true,
            description: None,
        },
    };
    let ps = [
        mk("a", &["*delete*"], ToolAction::BlockStrip),
        mk("b", &["shell", "bash"], ToolAction::BlockReject),
        mk("c", &["mcp__server1__*"], ToolAction::Allow),
        mk("d", &["*secret*"], ToolAction::BlockStrip),
        mk("e", &["mcp__*", "!*_9"], ToolAction::Observe),
        mk("f", &["web_*"], ToolAction::Allow),
    ];
    let snap = Snapshot {
        tool_index: Arc::new(ToolPolicyIndex::new(ps.iter())),
        ..Snapshot::default()
    };
    AuthContext {
        snapshot: Some(Arc::new(snap)),
        ..AuthContext::default()
    }
}

fn measure(label: &str, iters: u32, base: f64, mut f: impl FnMut()) -> f64 {
    for _ in 0..iters / 10 {
        f();
    }
    let t = Instant::now();
    for _ in 0..iters {
        f();
    }
    let us = t.elapsed().as_secs_f64() * 1e6 / f64::from(iters);
    println!(
        "{label:<52} {:>8.2} µs/Anfrage (netto {:>7.2} µs)",
        us,
        us - base
    );
    us
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Policies im Log nicht ausgeben (observe warnt sonst je Anfrage).
    let headers = HeaderMap::new();
    let none = request(0);
    let req = request(30);
    let off = Arc::new(ToolInventory::default());
    let inv = Arc::new(ToolInventory::new(
        Arc::new(NullSink::default()),
        &aigw_core::config::ToolsConfig::default(),
    ));
    let plain = AuthContext::default();
    let with_policies = policies();
    let n = 20_000;
    println!("Tool-Schritt (AIGW-87), {n} Iterationen, 30 Tools je Anfrage\n");
    let mut none = none;
    let mut req = req;
    let base = measure("ohne Tools", n, 0.0, || {
        let _ = black_box(apply_request(&off, &plain, "m", &headers, &mut none, PayloadMode::Off));
    });
    let base = measure("30 Tools, ohne Inventar, ohne Richtlinien", n, base, || {
        let _ = black_box(apply_request(&off, &plain, "m", &headers, &mut req, PayloadMode::Off));
    });
    measure("30 Tools, ohne Inventar, 6 Richtlinien", n, base, || {
        let _ = black_box(apply_request(&off, &with_policies, "m", &headers, &mut req, PayloadMode::Off));
    });
    measure("30 Tools, Inventar (bekannte Hashes), ohne Richtlinien", n, base, || {
        let _ = black_box(apply_request(&inv, &plain, "m", &headers, &mut req, PayloadMode::Off));
    });
    measure("30 Tools, Inventar + 6 Richtlinien", n, base, || {
        let _ = black_box(apply_request(&inv, &with_policies, "m", &headers, &mut req, PayloadMode::Off));
    });
    assert_eq!(req.tools.len(), 30);
    inv.flush().await;
}
