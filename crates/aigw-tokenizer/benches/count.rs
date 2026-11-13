// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Durchsatz der Token-Zählung: typischer Prompt mit 4 KB (Ziel: < 50 µs) bzw. 64 KB, gemischt
//! Englisch/Deutsch/Code; dazu 4 KB CJK (lange Stücke) und die Heuristik.

#![allow(missing_docs)]

use aigw_tokenizer::{Encoding, Profile, heuristic};

fn main() {
    // Tabellen vorab laden (nicht Teil der Messung).
    for e in Encoding::ALL {
        let _ = e.load();
    }
    divan::main();
}

fn prompt(size: usize) -> String {
    const PARA: &str = "You are a helpful assistant. Please summarize the following support \
        ticket and propose next steps. The customer reported on 2026-11-13 that the export \
        job fails with error code 0x1F after 3.5 minutes. Bitte antworte auf Deutsch: Der \
        Kunde möchte wissen, ob die Daten vollständig übertragen wurden und wann die \
        Störung behoben wird.\n```rust\nlet buf = Vec::with_capacity(4096);\nfor (i, x) in \
        items.iter().enumerate() { total += x.len() * i; }\n```\n";
    let mut s = String::with_capacity(size + PARA.len());
    while s.len() < size {
        s.push_str(PARA);
    }
    truncate(s, size)
}

fn cjk(size: usize) -> String {
    const PARA: &str = "人工智能正在改变世界。東京は日本の首都です。今日は天気がとても良いので、公園を散歩しました。";
    let mut s = String::with_capacity(size + PARA.len());
    while s.len() < size {
        s.push_str(PARA);
    }
    truncate(s, size)
}

fn truncate(mut s: String, size: usize) -> String {
    let mut end = size.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s
}

#[divan::bench(args = [4096, 65536])]
fn o200k(b: divan::Bencher<'_, '_>, size: usize) {
    let text = prompt(size);
    b.bench(|| Encoding::O200kBase.count(divan::black_box(&text)));
}

#[divan::bench(args = [4096, 65536])]
fn cl100k(b: divan::Bencher<'_, '_>, size: usize) {
    let text = prompt(size);
    b.bench(|| Encoding::Cl100kBase.count(divan::black_box(&text)));
}

#[divan::bench(args = [4096])]
fn o200k_cjk(b: divan::Bencher<'_, '_>, size: usize) {
    let text = cjk(size);
    b.bench(|| Encoding::O200kBase.count(divan::black_box(&text)));
}

#[divan::bench(args = [4096])]
fn heuristic_claude(b: divan::Bencher<'_, '_>, size: usize) {
    let text = prompt(size);
    b.bench(|| heuristic::estimate(divan::black_box(&text), Profile::Claude));
}
