// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Mikro-Benchmarks der Plugin-Kette (AIGW-36): Overhead der leeren Kette, Lookup der
//! wirksamen Kette und Kosten je eingebautem Plugin.
//!
//! `cargo bench -p aigw-plugins --bench chain`

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::hint::black_box;
use std::sync::Arc;

use aigw_core::ir::{ChatRequest, StreamEvent};
use aigw_core::wire::WireFormat;
use aigw_plugins::{
    CallInfo, Chain, ChunkCtx, ChunkFlow, ChunkMode, ChunkRunner, Entry, Flow, Meta, Phases,
    Plugin, PluginError, PluginFactory, ReqCtx, ResponseHeaders, builtin,
};
use futures_util::FutureExt as _;
use http::HeaderMap;
use serde_json::json;

fn main() {
    divan::main();
}

fn request() -> ChatRequest {
    let mut r = ChatRequest {
        model: "gpt-4o".into(),
        ..ChatRequest::default()
    };
    r.max_tokens = Some(4096);
    r.sampling.temperature = Some(1.2);
    r
}

fn run(chain: &Chain, info: &CallInfo, headers: &HeaderMap) -> Flow {
    let mut ctx = ReqCtx {
        info,
        request: request(),
        client_headers: headers,
        upstream_headers: HeaderMap::new(),
        response_headers: ResponseHeaders::default(),
        meta: Meta::new(),
    };
    let flow = chain
        .run_request(&mut ctx)
        .now_or_never()
        .expect("synchron");
    black_box(&ctx);
    flow
}

fn chain_of(plugins: Vec<Arc<dyn Plugin>>) -> Chain {
    Chain::new(
        plugins
            .into_iter()
            .enumerate()
            .map(|(i, p)| Arc::new(Entry::new(format!("b{i}"), p)))
            .collect(),
    )
}

fn header() -> Arc<dyn Plugin> {
    builtin::header::Factory
        .build(&json!({
            "request": {"forward": ["x-request-source"], "set": {"x-tenant": "acme"}},
            "response": {"set": {"x-policy": "strict"}, "remove": ["x-aigw-deployment"]}
        }))
        .unwrap()
}

fn transform() -> Arc<dyn Plugin> {
    builtin::request_transform::Factory
        .build(&json!({
            "defaults": {"topP": 0.9},
            "overrides": {"user": "gateway"},
            "caps": {"maxTokens": 1024, "temperature": 1.0}
        }))
        .unwrap()
}

/// Nur die Anfrage bauen (Basislinie für die Differenzen unten).
#[divan::bench]
fn request_baseline() -> ChatRequest {
    black_box(request())
}

#[divan::bench]
fn empty_chain_request(b: divan::Bencher<'_, '_>) {
    let chain = Chain::empty();
    let info = CallInfo::new("rid", WireFormat::OpenAiChat);
    let headers = HeaderMap::new();
    b.bench(|| run(&chain, &info, &headers));
}

#[divan::bench]
fn header_plugin_request(b: divan::Bencher<'_, '_>) {
    let chain = chain_of(vec![header()]);
    let info = CallInfo::new("rid", WireFormat::OpenAiChat);
    let mut headers = HeaderMap::new();
    headers.insert("x-request-source", "bench".parse().unwrap());
    b.bench(|| run(&chain, &info, &headers));
}

#[divan::bench]
fn request_transform_plugin_request(b: divan::Bencher<'_, '_>) {
    let chain = chain_of(vec![transform()]);
    let info = CallInfo::new("rid", WireFormat::OpenAiChat);
    let headers = HeaderMap::new();
    b.bench(|| run(&chain, &info, &headers));
}

#[divan::bench]
fn header_and_transform_request(b: divan::Bencher<'_, '_>) {
    let chain = chain_of(vec![header(), transform()]);
    let info = CallInfo::new("rid", WireFormat::OpenAiChat);
    let headers = HeaderMap::new();
    b.bench(|| run(&chain, &info, &headers));
}

/// Lookup der wirksamen Kette (Key-ID → Präfix, Modellgruppe → Spalte) in einem Index
/// mit 10 000 Keys, 100 Teams mit Bindungen und 20 Modellgruppen mit Bindungen.
#[divan::bench]
fn index_lookup(b: divan::Bencher<'_, '_>) {
    let (idx, key) = support::big_index();
    b.bench(|| black_box(idx.chain(Some(&key), Some("group-7"))));
}

/// Was der Hot Path ohne Bindungen tatsächlich tut: `PluginChains::chain_for` (ArcSwap-Load,
/// Versionsvergleich, Lookup, `Arc`-Klon) und `is_empty` – Kontext, Meta und Stream-Hülle
/// werden dann nicht angelegt.
#[divan::bench]
fn hot_path_without_bindings(b: divan::Bencher<'_, '_>) {
    let chains = aigw_plugins::PluginChains::default();
    let (_, key) = support::big_index();
    b.bench(|| {
        let chain = chains.chain_for(None, Some(black_box(&key)), Some("group-7"));
        black_box(chain.is_empty())
    });
}

#[divan::bench]
fn index_lookup_no_bindings(b: divan::Bencher<'_, '_>) {
    let idx = aigw_plugins::PluginIndex::default();
    let (_, key) = support::big_index();
    b.bench(|| black_box(idx.chain(Some(&key), Some("group-7"))));
}

struct Pass;

impl Plugin for Pass {
    fn name(&self) -> &'static str {
        "pass"
    }
    fn phases(&self) -> Phases {
        Phases {
            request: false,
            response: false,
            chunk: true,
        }
    }
    fn on_chunk(
        &self,
        _ctx: &mut ChunkCtx<'_>,
        ev: &mut StreamEvent,
    ) -> Result<ChunkFlow, PluginError> {
        black_box(ev);
        Ok(ChunkFlow::Continue)
    }
}

/// Ein Text-Chunk durch eine Kette mit einem durchreichenden Stream-Plugin.
#[divan::bench]
fn chunk_pass_through(b: divan::Bencher<'_, '_>) {
    let chain = Arc::new(chain_of(vec![Arc::new(Pass)]));
    let mut runner = ChunkRunner::new(
        chain,
        Arc::new(CallInfo::new("rid", WireFormat::OpenAiChat)),
        Meta::new(),
        ChunkMode::Live,
    );
    let mut out = Vec::with_capacity(4);
    b.bench_local(|| {
        runner.push(
            StreamEvent::TextDelta {
                index: 0,
                text: String::new(),
            },
            &mut out,
        );
        black_box(&out);
        out.clear();
    });
}

mod support {
    use std::sync::Arc;

    use aigw_core::resource::{
        ApiKey, ApiKeySpec, ApiKeyState, ApiKeyStatus, JsonObject, Object, ObjectMeta,
        OrganizationSpec, PluginBindingSpec, Scope, ScopeLevel, TeamSpec,
    };
    use aigw_core::snapshot::{KeyCtx, KeyHash, Snapshot};
    use aigw_plugins::{IndexOptions, PluginIndex, Registry};
    use serde_json::json;

    fn binding(
        name: String,
        level: ScopeLevel,
        target: Option<String>,
    ) -> Arc<Object<PluginBindingSpec>> {
        let config: JsonObject = match json!({"response": {"set": {"x-b": "1"}}}) {
            serde_json::Value::Object(m) => m,
            _ => JsonObject::new(),
        };
        Arc::new(Object {
            id: uuid::Uuid::now_v7(),
            metadata: ObjectMeta::named(name),
            spec: PluginBindingSpec {
                scope: Scope { level, target },
                plugin: "header".into(),
                order: 0,
                enabled: true,
                config,
            },
        })
    }

    pub(super) fn big_index() -> (PluginIndex, KeyCtx) {
        let org = Arc::new(Object {
            id: uuid::Uuid::now_v7(),
            metadata: ObjectMeta::named("acme"),
            spec: OrganizationSpec::default(),
        });
        let teams: Vec<_> = (0..100)
            .map(|i| {
                Arc::new(Object {
                    id: uuid::Uuid::now_v7(),
                    metadata: ObjectMeta::named(format!("team-{i}")),
                    spec: TeamSpec {
                        organization: "acme".into(),
                        ..TeamSpec::default()
                    },
                })
            })
            .collect();
        let mut snap = Snapshot {
            version: 1,
            ..Snapshot::default()
        };
        snap.plugin_bindings
            .push(binding("g".into(), ScopeLevel::Global, None));
        for i in 0..100 {
            snap.plugin_bindings.push(binding(
                format!("t{i}"),
                ScopeLevel::Team,
                Some(format!("team-{i}")),
            ));
        }
        for i in 0..20 {
            snap.plugin_bindings.push(binding(
                format!("m{i}"),
                ScopeLevel::ModelGroup,
                Some(format!("group-{i}")),
            ));
        }
        let mut last = None;
        for i in 0..10_000u32 {
            let k = KeyCtx {
                key: ApiKey {
                    id: uuid::Uuid::now_v7(),
                    metadata: ObjectMeta::named(format!("key-{i}")),
                    spec: ApiKeySpec::default(),
                    status: ApiKeyStatus {
                        prefix: "sk".into(),
                        state: ApiKeyState::Active,
                        last_used_at: None,
                    },
                },
                team: Some(Arc::clone(&teams[(i % 100) as usize])),
                organization: Some(Arc::clone(&org)),
                expires_at: None,
            };
            let mut h = [0u8; 32];
            h[..4].copy_from_slice(&i.to_le_bytes());
            last = Some(k.clone());
            snap.keys.insert(KeyHash(h), Arc::new(k));
        }
        let idx = PluginIndex::build(&snap, &Registry::with_builtin(), IndexOptions::default());
        (idx, last.expect("Keys"))
    }
}
