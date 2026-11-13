// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Mikro-Benchmarks: OpenAI-Request (Bytes) → IR → OpenAI- bzw. Anthropic-JSON.
//!
//! `cargo bench -p aigw-core --bench ir`

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use aigw_core::wire::{Losses, anthropic, openai};

const FULL: &[u8] = include_bytes!("../tests/fixtures/openai/request_full.json");

/// Typische Chat-Anfrage: Systemprompt, ~2 KB Nutzertext, ein Tool.
fn typical() -> Vec<u8> {
    let text = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. ".repeat(36);
    serde_json::to_vec(&serde_json::json!({
        "model": "gpt-4o",
        "messages": [
            {"role": "system", "content": "Du bist ein hilfreicher Assistent."},
            {"role": "user", "content": text},
            {"role": "assistant", "content": "Verstanden."},
            {"role": "user", "content": "Fasse zusammen."}
        ],
        "tools": [{"type": "function", "function": {"name": "search", "parameters": {"type": "object", "properties": {"q": {"type": "string"}}}}}],
        "temperature": 0.2,
        "max_completion_tokens": 256,
        "stream": true,
        "stream_options": {"include_usage": true}
    }))
    .unwrap()
}

fn main() {
    divan::main();
}

#[divan::bench]
fn openai_parse_to_ir_typical(b: divan::Bencher<'_, '_>) {
    let body = typical();
    b.counter(divan::counter::BytesCount::new(body.len()))
        .bench(|| openai::decode_request(divan::black_box(&body)).unwrap());
}

#[divan::bench]
fn openai_roundtrip_typical(b: divan::Bencher<'_, '_>) {
    let body = typical();
    b.counter(divan::counter::BytesCount::new(body.len()))
        .bench(|| {
            let ir = openai::decode_request(divan::black_box(&body)).unwrap();
            openai::encode_request(ir, &mut Losses::new()).unwrap()
        });
}

#[divan::bench]
fn openai_roundtrip_full_fixture(b: divan::Bencher<'_, '_>) {
    b.counter(divan::counter::BytesCount::new(FULL.len()))
        .bench(|| {
            let ir = openai::decode_request(divan::black_box(FULL)).unwrap();
            openai::encode_request(ir, &mut Losses::new()).unwrap()
        });
}

#[divan::bench]
fn openai_to_anthropic_typical(b: divan::Bencher<'_, '_>) {
    let body = typical();
    b.counter(divan::counter::BytesCount::new(body.len()))
        .bench(|| {
            let ir = openai::decode_request(divan::black_box(&body)).unwrap();
            anthropic::encode_request(ir, &mut Losses::new()).unwrap()
        });
}

#[divan::bench]
fn serde_value_baseline_typical(b: divan::Bencher<'_, '_>) {
    // Vergleich: nur JSON → serde_json::Value → JSON (untere Schranke ohne IR).
    let body = typical();
    b.counter(divan::counter::BytesCount::new(body.len()))
        .bench(|| {
            let v: serde_json::Value = serde_json::from_slice(divan::black_box(&body)).unwrap();
            serde_json::to_vec(&v).unwrap()
        });
}
