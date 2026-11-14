// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Latenz eines WASM-Plugins (AIGW-63): Leer-Plugin (`testkit`, `mode: pass`) in der
//! Anfrage-Phase (frische Instanz je Aufruf, `spawn_blocking`) und im Stream-Pfad (synchron,
//! `block_in_place`), dazu das Beispiel `keyword-guard`.
//!
//! `cargo bench -p aigw-wasm --bench wasm`

#![allow(missing_docs, clippy::unwrap_used)]

#[path = "../tests/support.rs"]
mod support;

use std::sync::{Arc, LazyLock};

use aigw_core::config::WasmConfig;
use aigw_core::ir::StreamEvent;
use aigw_core::wire::WireFormat;
use aigw_plugins::{CallInfo, Chain, ChunkMode, ChunkRunner, Meta, ReqCtx, ResponseHeaders};
use serde_json::json;

fn main() {
    divan::main();
}

struct Env {
    rt: tokio::runtime::Runtime,
    pass: Arc<Chain>,
    guard: Arc<Chain>,
}

static ENV: LazyLock<Env> = LazyLock::new(|| {
    let (registry, _) = support::registry(&WasmConfig::default());
    let snap = |b| {
        support::snapshot(
            vec![
                support::module("testkit", "testkit", &"a".repeat(64)),
                support::module("keyword-guard", "keyword-guard", &"b".repeat(64)),
            ],
            vec![b],
        )
    };
    let pass = support::chain(
        &snap(support::binding(
            "pass",
            json!({"module": "testkit", "config": {"mode": "pass"}}),
            None,
            None,
        )),
        &registry,
    )
    .0;
    let guard = support::chain(
        &snap(support::binding(
            "guard",
            json!({"module": "keyword-guard", "config": {"block": ["geheimprojekt"], "mask": ["intern"]}}),
            None,
            None,
        )),
        &registry,
    )
    .0;
    Env {
        rt: tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap(),
        pass,
        guard,
    }
});

fn run_request(chain: &Arc<Chain>) {
    let info = CallInfo::new("r", WireFormat::OpenAiChat);
    let headers = http::HeaderMap::new();
    ENV.rt.block_on(async {
        let mut ctx = ReqCtx {
            info: &info,
            request: support::request("Hallo, wie geht es dir heute? Bitte kurz antworten."),
            client_headers: &headers,
            upstream_headers: http::HeaderMap::new(),
            response_headers: ResponseHeaders::default(),
            meta: Meta::new(),
        };
        divan::black_box(chain.run_request(&mut ctx).await);
    });
}

/// Leer-Plugin, Anfrage-Phase (inkl. Kontext/JSON, Instanz, `spawn_blocking`).
#[divan::bench]
fn empty_plugin_request() {
    run_request(&ENV.pass);
}

/// Beispiel `keyword-guard` (JSON parsen, Wörter suchen), Anfrage-Phase.
#[divan::bench]
fn keyword_guard_request() {
    run_request(&ENV.guard);
}

/// Leer-Plugin, ein Stream-Ereignis (synchron über `block_in_place`).
#[divan::bench]
fn empty_plugin_chunk(b: divan::Bencher<'_, '_>) {
    let mut runner = ChunkRunner::new(
        Arc::clone(&ENV.pass),
        Arc::new(CallInfo::new("r", WireFormat::OpenAiChat)),
        Meta::new(),
        ChunkMode::Live,
    );
    let mut out = Vec::with_capacity(4);
    let _guard = ENV.rt.enter();
    b.bench_local(|| {
        ENV.rt.block_on(async {
            runner.push(
                StreamEvent::TextDelta {
                    index: 0,
                    text: "Hallo".into(),
                },
                &mut out,
            );
        });
        divan::black_box(&out);
        out.clear();
    });
}
