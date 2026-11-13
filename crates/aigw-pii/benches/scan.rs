// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Durchsatz der PII-Erkennung: typischer Prompt mit 4 KB bzw. 64 KB (Ziel: 4 KB < 50 µs).

#![allow(missing_docs)]

use aigw_pii::{Engine, Finding};

fn main() {
    divan::main();
}

/// Prosa mit Zahlen, Code-Schnipseln und einigen echten Funden (~1 je 1 KB).
fn prompt(size: usize) -> String {
    const PARA: &str = "Bitte fasse den folgenden Vorgang zusammen. Am 13.11.2026 um 14:05 Uhr \
        meldete der Kunde (Ticket #4711, Version v2.4.1) einen Fehler in Modul 3.14: \
        `let buf = Vec::with_capacity(4096);` liefert 0 Bytes. Die Kosten lagen bei 1.250,00 EUR \
        bzw. 0,5 % des Budgets. The quick brown fox jumps over the lazy dog; see RFC 9457 and \
        section 4.2 for details on problem documents and error handling in HTTP APIs. ";
    const PII: [&str; 4] = [
        "Kontakt: max.mustermann@example.de, Tel. +49 30 1234567. ",
        "IBAN DE89 3704 0044 0532 0130 00 und Karte 4111 1111 1111 1111. ",
        "Server 192.168.10.20 mit Key ghp_1234567890abcdefghijABCDEFGHIJ123456. ",
        "Steuer-ID 86095742719, Ausweis T220001293. ",
    ];
    let mut s = String::with_capacity(size + 512);
    let mut k = 0;
    while s.len() < size {
        s.push_str(PARA);
        if s.len() / 1024 > k {
            s.push_str(PII[k % PII.len()]);
            k += 1;
        }
    }
    s.truncate(size);
    while !s.is_char_boundary(s.len()) {
        s.pop();
    }
    s
}

#[divan::bench(args = [4 * 1024, 64 * 1024])]
fn scan(bencher: divan::Bencher<'_, '_>, size: usize) {
    let engine = Engine::default();
    let text = prompt(size);
    let mut out: Vec<Finding> = Vec::with_capacity(64);
    bencher
        .counter(divan::counter::BytesCount::new(text.len()))
        .bench_local(|| {
            out.clear();
            engine.scan(divan::black_box(&text), &mut out);
            divan::black_box(out.len())
        });
}

#[divan::bench(args = [4 * 1024, 64 * 1024])]
fn scan_clean_prose(bencher: divan::Bencher<'_, '_>, size: usize) {
    let engine = Engine::default();
    let text: String = "Lorem ipsum dolor sit amet, consectetur adipiscing elit 2026. "
        .repeat(size / 60 + 1)[..size]
        .to_owned();
    let mut out: Vec<Finding> = Vec::new();
    bencher
        .counter(divan::counter::BytesCount::new(text.len()))
        .bench_local(|| {
            out.clear();
            engine.scan(divan::black_box(&text), &mut out);
            divan::black_box(out.len())
        });
}

#[divan::bench(args = [4 * 1024])]
fn mask_labels(bencher: divan::Bencher<'_, '_>, size: usize) {
    let engine = Engine::default();
    let text = prompt(size);
    bencher.bench_local(|| divan::black_box(engine.mask_labels(divan::black_box(&text)).len()));
}
