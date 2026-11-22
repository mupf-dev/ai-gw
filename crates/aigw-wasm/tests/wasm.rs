// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Tests der WASM-Plugins (AIGW-63) mit den eingecheckten Komponenten aus `plugins/`
//! (`tests/data/*.wasm`, gebaut mit `scripts/build-wasm-plugins.sh`).

#![allow(clippy::unwrap_used, clippy::panic, missing_docs)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use aigw_core::config::WasmConfig;
use aigw_core::ir::{ChatResponse, Choice, Content, Message, Role, StreamEvent};
use aigw_core::wire::WireFormat;
use aigw_plugins::{
    CallInfo, Chain, ChunkMode, ChunkRunner, Flow, IndexOptions, Meta, PluginChains, ReqCtx,
    RespCtx, ResponseHeaders,
};
use serde_json::{Value, json};

use support::{binding, chain, module, registry, request, snapshot};

const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn modules() -> Vec<aigw_core::wasm::WasmModule> {
    vec![
        module("keyword-guard", "keyword-guard", SHA_A),
        module("testkit", "testkit", SHA_B),
    ]
}

struct Out {
    flow: Flow,
    request: aigw_core::ir::ChatRequest,
    upstream: http::HeaderMap,
    response_headers: ResponseHeaders,
    meta: Meta,
}

async fn run(chain: &Chain, text: &str, headers: &http::HeaderMap, meta: Meta) -> Out {
    let info = CallInfo::new("rid-1", WireFormat::OpenAiChat);
    let mut ctx = ReqCtx {
        info: &info,
        request: request(text),
        client_headers: headers,
        upstream_headers: http::HeaderMap::new(),
        response_headers: ResponseHeaders::default(),
        meta,
    };
    let flow = chain.run_request(&mut ctx).await;
    Out {
        flow,
        request: ctx.request,
        upstream: ctx.upstream_headers,
        response_headers: ctx.response_headers,
        meta: ctx.meta,
    }
}

fn one(
    config: Value,
    fail_mode: Option<&str>,
    timeout_ms: Option<u64>,
) -> (Arc<Chain>, Arc<aigw_wasm::WasmRuntime>) {
    let (reg, rt) = registry(&WasmConfig::default());
    // Großzügiges Zeitlimit (Standard 100 ms): unter Last (parallele Testläufe) sonst Timeouts
    // statt der erwarteten Ergebnisse. Die Zeitgrenze selbst prüft `time_limit`.
    let timeout_ms = timeout_ms.or(Some(5_000));
    let snap = snapshot(modules(), vec![binding("b", config, fail_mode, timeout_ms)]);
    let (c, n) = chain(&snap, &reg);
    assert_eq!(n, 1, "Bindung nicht gebaut");
    (c, rt)
}

fn text_of(r: &aigw_core::ir::ChatRequest) -> String {
    match &r.messages[0].content {
        Content::Text(t) => t.clone(),
        other => format!("{other:?}"),
    }
}

fn reject_code(flow: &Flow) -> (u16, String) {
    match flow {
        Flow::Reject(e) => (e.status(), e.code.clone().unwrap_or_default()),
        other => panic!("Reject erwartet, erhalten {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn loads_passes_and_sets_headers() {
    let (c, rt) = one(
        json!({"module": "keyword-guard", "config": {
            "block": ["geheimprojekt"],
            "upstreamHeaders": {"x-guard": "on", "authorization": "Bearer böse"},
            "responseHeaders": {"x-guard": "checked"}
        }}),
        None,
        None,
    );
    // `authorization` an den Upstream ist gesperrt → Ausgabe ungültig → failMode closed.
    let out = run(&c, "Hallo", &http::HeaderMap::new(), Meta::new()).await;
    assert_eq!(reject_code(&out.flow), (500, "plugin_error".into()));
    assert_eq!(rt.metrics().errors("keyword-guard", "output"), 1);

    let (c, rt) = one(
        json!({"module": "keyword-guard", "config": {
            "block": ["geheimprojekt"],
            "upstreamHeaders": {"x-guard": "on"},
            "responseHeaders": {"x-guard": "checked"}
        }}),
        None,
        None,
    );
    let out = run(&c, "Hallo Welt", &http::HeaderMap::new(), Meta::new()).await;
    assert!(matches!(out.flow, Flow::Continue), "{:?}", out.flow);
    assert_eq!(out.upstream["x-guard"], "on");
    assert_eq!(out.response_headers.set["x-guard"], "checked");
    assert_eq!(text_of(&out.request), "Hallo Welt");
    assert_eq!(rt.metrics().calls("keyword-guard", "request", "pass"), 1);
    assert!(rt.metrics().fuel("keyword-guard") > 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn blocks_keyword() {
    let (c, rt) = one(
        json!({"module": "keyword-guard", "config": {"block": ["GeheimProjekt"]}}),
        None,
        None,
    );
    let out = run(
        &c,
        "Was weißt du über das geheimprojekt X?",
        &http::HeaderMap::new(),
        Meta::new(),
    )
    .await;
    assert_eq!(reject_code(&out.flow), (400, "keyword_blocked".into()));
    assert_eq!(
        out.response_headers.set[aigw_plugins::GUARDRAIL_HEADER],
        "wasm"
    );
    assert_eq!(rt.metrics().calls("keyword-guard", "request", "block"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn modifies_request_and_meta() {
    let (c, _) = one(
        json!({"module": "keyword-guard", "config": {"mask": ["intern"]}}),
        None,
        None,
    );
    let mut meta = Meta::new();
    meta.insert("aigw.pii".into(), json!({"geheim": true}));
    let out = run(
        &c,
        "das ist intern und intern",
        &http::HeaderMap::new(),
        meta,
    )
    .await;
    assert!(matches!(out.flow, Flow::Continue), "{:?}", out.flow);
    assert_eq!(text_of(&out.request), "das ist *** und ***");
    assert_eq!(out.meta["keywordGuardMasked"], json!(2));
    // Interne Schlüssel bleiben erhalten.
    assert_eq!(out.meta["aigw.pii"], json!({"geheim": true}));
}

#[tokio::test(flavor = "multi_thread")]
async fn meta_is_external_view_and_internal_keys_are_protected() {
    let (c, _) = one(
        json!({"module": "testkit", "config": {"mode": "meta"}}),
        None,
        None,
    );
    let mut meta = Meta::new();
    meta.insert("aigw.pii".into(), json!("intern"));
    meta.insert("team".into(), json!("research"));
    let out = run(&c, "x", &http::HeaderMap::new(), meta).await;
    assert!(matches!(out.flow, Flow::Continue), "{:?}", out.flow);
    let seen: Value =
        serde_json::from_str(out.response_headers.set["x-testkit-meta"].to_str().unwrap()).unwrap();
    assert_eq!(seen, json!({"team": "research"}));
    assert_eq!(out.meta["seen"], json!(true));
    assert_eq!(out.meta["aigw.pii"], json!("intern"));
    assert!(!out.meta.contains_key("team"));
    assert!(!out.meta.contains_key("aigw.secret"));
}

#[tokio::test(flavor = "multi_thread")]
async fn respond_and_bad_output() {
    let (c, _) = one(
        json!({"module": "testkit", "config": {"mode": "respond"}}),
        None,
        None,
    );
    let out = run(&c, "x", &http::HeaderMap::new(), Meta::new()).await;
    match out.flow {
        Flow::Respond(r) => {
            assert_eq!(r.model, "m");
            assert_eq!(r.id, "aigw-wasm-rid-1");
        }
        other => panic!("Respond erwartet: {other:?}"),
    }
    let (c, rt) = one(
        json!({"module": "testkit", "config": {"mode": "bad-output"}}),
        Some("open"),
        None,
    );
    let out = run(&c, "x", &http::HeaderMap::new(), Meta::new()).await;
    assert!(
        matches!(out.flow, Flow::Continue),
        "failMode open: {:?}",
        out.flow
    );
    assert_eq!(rt.metrics().errors("testkit", "output"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn guest_error_and_trap_follow_fail_mode() {
    for mode in ["error", "trap"] {
        let (c, rt) = one(
            json!({"module": "testkit", "config": {"mode": mode}}),
            None,
            None,
        );
        let out = run(&c, "x", &http::HeaderMap::new(), Meta::new()).await;
        assert_eq!(
            reject_code(&out.flow),
            (500, "plugin_error".into()),
            "{mode}"
        );
        let kind = if mode == "error" { "guest" } else { "trap" };
        assert_eq!(rt.metrics().errors("testkit", kind), 1, "{mode}");
        let (c, _) = one(
            json!({"module": "testkit", "config": {"mode": mode}}),
            Some("open"),
            None,
        );
        let out = run(&c, "x", &http::HeaderMap::new(), Meta::new()).await;
        assert!(
            matches!(out.flow, Flow::Continue),
            "{mode} open: {:?}",
            out.flow
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn fuel_limit() {
    let (c, rt) = one(
        json!({"module": "testkit", "fuel": 2_000_000, "config": {"mode": "spin"}}),
        None,
        Some(5_000),
    );
    let out = run(&c, "x", &http::HeaderMap::new(), Meta::new()).await;
    assert_eq!(reject_code(&out.flow), (500, "plugin_error".into()));
    assert_eq!(rt.metrics().errors("testkit", "fuel"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn time_limit() {
    let cfg = WasmConfig::default();
    let (c, rt) = one(
        json!({"module": "testkit", "fuel": cfg.max_fuel, "config": {"mode": "spin"}}),
        None,
        Some(50),
    );
    let started = std::time::Instant::now();
    let out = run(&c, "x", &http::HeaderMap::new(), Meta::new()).await;
    assert_eq!(reject_code(&out.flow), (500, "plugin_error".into()));
    assert!(started.elapsed() < Duration::from_secs(2));
    // Die Instanz selbst bricht per Epoch-Deadline ab (nicht nur die Kette wartet nicht mehr).
    for _ in 0..100 {
        if rt.metrics().errors("testkit", "timeout") == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(rt.metrics().errors("testkit", "timeout"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_limit() {
    let (c, rt) = one(
        json!({"module": "testkit", "maxMemoryMb": 8, "config": {"mode": "alloc", "mb": 32}}),
        None,
        None,
    );
    let out = run(&c, "x", &http::HeaderMap::new(), Meta::new()).await;
    assert_eq!(reject_code(&out.flow), (500, "plugin_error".into()));
    assert_eq!(rt.metrics().errors("testkit", "memory"), 1);
    // Innerhalb der Grenze geht es.
    let (c, _) = one(
        json!({"module": "testkit", "maxMemoryMb": 64, "config": {"mode": "alloc", "mb": 8}}),
        None,
        None,
    );
    let out = run(&c, "x", &http::HeaderMap::new(), Meta::new()).await;
    assert!(matches!(out.flow, Flow::Continue), "{:?}", out.flow);
}

#[test]
fn limits_above_global_maximum_are_invalid() {
    let (reg, _) = registry(&WasmConfig::default());
    for cfg in [
        json!({"module": "testkit", "fuel": u64::MAX}),
        json!({"module": "testkit", "maxMemoryMb": 100_000}),
    ] {
        let snap = snapshot(modules(), vec![binding("b", cfg, None, None)]);
        assert_eq!(chain(&snap, &reg).1, 0);
    }
}

#[test]
fn invalid_modules_and_bindings() {
    let (reg, rt) = registry(&WasmConfig::default());
    let mut broken = module("broken", "testkit", SHA_A);
    broken.content = bytes::Bytes::from_static(b"\0asm\x0d\0\x01\0kaputt");
    let mut core = module("core", "testkit", SHA_B);
    core.content = bytes::Bytes::from_static(b"\0asm\x01\0\0\0");
    let mut all = modules();
    all.extend([broken, core]);
    for cfg in [
        json!({"module": "broken"}),
        json!({"module": "core"}),
        json!({"module": "fehlt"}),
        json!({"module": "testkit", "sha256": SHA_A}),
        json!({"module": "testkit", "config": {"mode": "describe-error"}}),
    ] {
        // Ohne failMode: übersprungen.
        let snap = snapshot(all.clone(), vec![binding("b", cfg.clone(), None, None)]);
        assert_eq!(chain(&snap, &reg).1, 0, "{cfg}");
        // failMode closed: Kette gesperrt.
        let snap = snapshot(
            all.clone(),
            vec![binding("b", cfg.clone(), Some("closed"), None)],
        );
        let (c, _) = chain(&snap, &reg);
        let rt2 = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let out = rt2.block_on(run(&c, "x", &http::HeaderMap::new(), Meta::new()));
        assert_eq!(
            reject_code(&out.flow),
            (500, "plugin_config_invalid".into()),
            "{cfg}"
        );
    }
    let mut out = String::new();
    rt.metrics().render_prometheus(&mut out);
    assert!(
        out.contains("aigw_wasm_compile_total{outcome=\"error\"} "),
        "{out}"
    );
}

#[test]
fn fingerprint_follows_module_sha_and_compiled_cache() {
    let (reg, rt) = registry(&WasmConfig::default());
    let f = reg.get("wasm").unwrap();
    let cfg = json!({"module": "testkit"});
    let a = snapshot(modules(), vec![]);
    let mut b = a.clone();
    b.wasm_modules.insert(
        "testkit".into(),
        Arc::new(module("testkit", "testkit", SHA_A)),
    );
    assert_eq!(f.fingerprint(&a, &cfg), SHA_B);
    assert_eq!(f.fingerprint(&b, &cfg), SHA_A);

    // Hot Reload: neue Version mit anderem Modul baut die Bindung neu; gleiche SHA nutzt den
    // Cache kompilierter Module.
    let chains = PluginChains::new(reg.clone(), IndexOptions::default());
    let mut s1 = snapshot(
        modules(),
        vec![binding("b", json!({"module": "testkit"}), None, None)],
    );
    s1.version = 1;
    chains.rebuild(&s1);
    let first = chains.index().chain(None, None);
    let mut s2 = s1.clone();
    s2.version = 2;
    chains.rebuild(&s2);
    let same = chains.index().chain(None, None);
    assert!(Arc::ptr_eq(
        first.entries()[0].plugin(),
        same.entries()[0].plugin()
    ));
    let mut s3 = s2.clone();
    s3.version = 3;
    s3.wasm_modules.insert(
        "testkit".into(),
        Arc::new(module("testkit", "testkit", SHA_A)),
    );
    chains.rebuild(&s3);
    let rebuilt = chains.index().chain(None, None);
    assert!(!Arc::ptr_eq(
        first.entries()[0].plugin(),
        rebuilt.entries()[0].plugin()
    ));
    let mut out = String::new();
    rt.metrics().render_prometheus(&mut out);
    assert!(
        out.contains("aigw_wasm_compile_total{outcome=\"ok\"} 2\n"),
        "{out}"
    );
}

fn response(text: &str) -> ChatResponse {
    ChatResponse {
        id: "r".into(),
        model: "m".into(),
        created: None,
        choices: vec![Choice {
            index: 0,
            message: Message::text(Role::Assistant, text),
            stop_reason: None,
            stop_sequence: None,
            extra: Default::default(),
        }],
        usage: None,
        extra: Default::default(),
        source: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn output_phases_response_and_chunk() {
    let (c, _) = one(
        json!({"module": "keyword-guard", "config": {"block": ["geheim"], "mask": ["intern"], "output": true}}),
        None,
        None,
    );
    assert!(
        !c.needs_buffered_output(),
        "Stream-Haken vorhanden: kein Puffern"
    );
    // Antwort: maskieren.
    let info = CallInfo::new("rid", WireFormat::OpenAiChat);
    let mut headers = ResponseHeaders::default();
    let mut meta = Meta::new();
    let mut ctx = RespCtx {
        info: &info,
        response: response("das ist intern"),
        response_headers: &mut headers,
        meta: &mut meta,
    };
    assert!(matches!(c.run_response(&mut ctx).await, Flow::Continue));
    assert_eq!(
        ctx.response.choices[0].message.content,
        Content::Text("das ist ***".into())
    );
    // Stream: blockieren.
    let mut runner = ChunkRunner::new(
        Arc::clone(&c),
        Arc::new(info.clone()),
        Meta::new(),
        ChunkMode::Live,
    );
    let mut out = Vec::new();
    runner.push(
        StreamEvent::TextDelta {
            index: 0,
            text: "hallo ".into(),
        },
        &mut out,
    );
    assert!(
        matches!(&out[..], [StreamEvent::TextDelta { .. }]),
        "{out:?}"
    );
    out.clear();
    runner.push(
        StreamEvent::TextDelta {
            index: 0,
            text: "Geheim!".into(),
        },
        &mut out,
    );
    assert!(runner.is_terminated(), "{out:?}");
}

/// Stream-Pfad auf einer Current-Thread-Runtime (Tests, eigener Thread) mit Trap
/// (stderr-Ausgabe über WASI) – darf den Prozess nicht stören.
#[tokio::test]
async fn chunk_on_current_thread_runtime() {
    for (mode, kind) in [("trap", "trap"), ("drop", "")] {
        let (c, rt) = one(
            json!({"module": "testkit", "config": {"mode": mode}}),
            None,
            None,
        );
        let mut runner = ChunkRunner::new(
            c,
            Arc::new(CallInfo::new("rid", WireFormat::OpenAiChat)),
            Meta::new(),
            ChunkMode::Live,
        );
        let mut out = Vec::new();
        runner.push(
            StreamEvent::TextDelta {
                index: 0,
                text: "x".into(),
            },
            &mut out,
        );
        if kind.is_empty() {
            assert!(out.is_empty(), "drop: {out:?}");
        } else {
            assert!(runner.is_terminated());
            assert_eq!(rt.metrics().errors("testkit", kind), 1);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn include_headers_reach_module_but_secrets_never() {
    let (reg, _) = registry(&WasmConfig::default());
    let snap = snapshot(
        modules(),
        vec![binding(
            "b",
            json!({"module": "keyword-guard", "includeHeaders": ["authorization"], "config": {}}),
            None,
            None,
        )],
    );
    assert_eq!(
        chain(&snap, &reg).1,
        0,
        "authorization darf nicht freigegeben werden"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn disabled_runtime_fails_calls() {
    let mut reg = aigw_plugins::Registry::with_builtin();
    reg.register(Arc::new(aigw_wasm::Factory::new(None)));
    let snap = snapshot(
        modules(),
        vec![binding("b", json!({"module": "testkit"}), None, None)],
    );
    let (c, n) = chain(&snap, &reg);
    assert_eq!(n, 1);
    let out = run(&c, "x", &http::HeaderMap::new(), Meta::new()).await;
    assert_eq!(reject_code(&out.flow), (500, "plugin_error".into()));
}

#[test]
fn wit_is_exposed() {
    assert!(aigw_wasm::WIT.contains("package aigw:plugin@0.1.0"));
}

// ------------------------------------------------------------------ AIGW-101: Signaturen

/// Modul mit echtem SHA-256 (für die Signaturprüfung).
fn real(name: &str) -> aigw_core::wasm::WasmModule {
    let mut m = module(name, "testkit", SHA_A);
    m.sha256 = aigw_core::wasm::sha256_hex(&m.content);
    m
}

fn trusted(keys: &[(&str, &[u8])], require: Option<bool>) -> WasmConfig {
    WasmConfig {
        trusted_keys: keys
            .iter()
            .map(|(id, der)| aigw_core::wasm::TrustedKey {
                id: (*id).into(),
                public_key: aigw_core::wasm::public_key_pem(
                    &aigw_core::wasm::public_key_of(der).unwrap(),
                ),
            })
            .collect(),
        require_signature: require,
        ..WasmConfig::default()
    }
}

fn signed(mut m: aigw_core::wasm::WasmModule, id: &str, der: &[u8]) -> aigw_core::wasm::WasmModule {
    let sig = aigw_core::wasm::sign_sha256(der, &m.sha256).unwrap();
    m.signature = Some(aigw_core::wasm::ModuleSignature::parse(id, &sig).unwrap());
    m
}

fn loads(cfg: &WasmConfig, m: aigw_core::wasm::WasmModule) -> bool {
    let (reg, _) = registry(cfg);
    let snap = snapshot(
        vec![m],
        vec![binding("b", json!({"module": "testkit"}), None, None)],
    );
    chain(&snap, &reg).1 == 1
}

#[test]
fn signature_checked_before_loading() {
    let k1 = aigw_core::wasm::generate_pkcs8().unwrap();
    let k2 = aigw_core::wasm::generate_pkcs8().unwrap();
    let cfg = trusted(&[("k1", &k1)], None);
    // gültig
    assert!(loads(&cfg, signed(real("testkit"), "k1", &k1)));
    // Pflicht ohne Signatur
    assert!(!loads(&cfg, real("testkit")));
    // unbekannter Schlüssel
    assert!(!loads(&cfg, signed(real("testkit"), "k9", &k1)));
    // falsche Signatur (anderer privater Schlüssel unter bekannter ID)
    assert!(!loads(&cfg, signed(real("testkit"), "k1", &k2)));
    // Inhalt nach dem Signieren getauscht (z. B. direkt in der Datenbank): SHA passt nicht.
    let mut swapped = signed(real("testkit"), "k1", &k1);
    swapped.content = bytes::Bytes::from(support::wasm("keyword-guard"));
    assert!(!loads(&cfg, swapped.clone()));
    // ... auch wenn der SHA mitgeändert wurde (Signatur gilt für den alten SHA).
    swapped.sha256 = aigw_core::wasm::sha256_hex(&swapped.content);
    assert!(!loads(&cfg, swapped));
    // Übergangsmodus: unsigniert erlaubt, falsche Signatur nicht.
    let lax = trusted(&[("k1", &k1)], Some(false));
    assert!(loads(&lax, real("testkit")));
    assert!(!loads(&lax, signed(real("testkit"), "k1", &k2)));
    // Ohne Schlüssel: keine Prüfung (Verhalten wie vor AIGW-101).
    assert!(loads(
        &WasmConfig::default(),
        signed(real("testkit"), "k1", &k2)
    ));

    // Metrik der Ablehnungen.
    let (reg, rt) = registry(&cfg);
    let snap = snapshot(
        vec![real("testkit")],
        vec![binding("b", json!({"module": "testkit"}), None, None)],
    );
    assert_eq!(chain(&snap, &reg).1, 0);
    assert_eq!(rt.metrics().signature_rejections("missing"), 1);
    let mut out = String::new();
    rt.metrics().render_prometheus(&mut out);
    assert!(
        out.contains("aigw_wasm_signature_rejected_total{reason=\"missing\"} 1\n"),
        "{out}"
    );
}

#[test]
fn signature_key_rotation() {
    let old = aigw_core::wasm::generate_pkcs8().unwrap();
    let new = aigw_core::wasm::generate_pkcs8().unwrap();
    let m_old = signed(real("testkit"), "k2026", &old);
    let m_new = signed(real("testkit"), "k2027", &new);
    // Übergang: beide Schlüssel vertraut.
    let both = trusted(&[("k2026", &old), ("k2027", &new)], None);
    assert!(loads(&both, m_old.clone()));
    assert!(loads(&both, m_new.clone()));
    // Alter Schlüssel entfernt: nur neu signierte Module laden.
    let only_new = trusted(&[("k2027", &new)], None);
    assert!(!loads(&only_new, m_old.clone()));
    assert!(loads(&only_new, m_new.clone()));
    // Neu signieren (gleicher Inhalt) ändert den Fingerabdruck → Bindung wird neu gebaut.
    let (reg, _) = registry(&only_new);
    let f = reg.get("wasm").unwrap();
    let cfg = json!({"module": "testkit"});
    let a = snapshot(vec![m_old], vec![]);
    let b = snapshot(vec![m_new], vec![]);
    assert_ne!(f.fingerprint(&a, &cfg), f.fingerprint(&b, &cfg));
}

#[test]
fn invalid_trust_config_is_rejected() {
    let cfg = WasmConfig {
        require_signature: Some(true),
        ..WasmConfig::default()
    };
    assert!(aigw_wasm::WasmRuntime::new(&cfg).is_err());
}
