// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Genauigkeit gegen Referenzzählungen (Fixtures aus `tools/gen_fixtures.py`):
//!
//! - `tiktoken.jsonl`: Token-IDs von tiktoken 0.14 für cl100k_base und o200k_base (Sprachen,
//!   Code, CJK, Emojis, Sonderfälle, 400 Zufallstexte) – müssen exakt übereinstimmen.
//! - `hf.jsonl`: Token-Anzahl laut Hugging Face `tokenizers` 0.23 für hier trainierte
//!   `tokenizer.json` (Byte-Level-BPE mit Llama-3-/Tekken-/GPT-2-Muster, SentencePiece-BPE mit
//!   byte_fallback, Metaspace-BPE, Unigram).
//! - `hf_real.jsonl`: echte Tokenizer (Llama 3.2, Mistral 7B v0.3, Mistral Nemo, TinyLlama);
//!   nur mit `AIGW_TOKENIZER_HF_DIR=<verzeichnis mit llama3.json, mistral7b.json, …>`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stderr,
    missing_docs
)]

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use aigw_tokenizer::{Encoding, HfTokenizer, Profile, heuristic};
use serde_json::Value;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn rows(name: &str) -> Vec<Value> {
    std::fs::read_to_string(fixture(name))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn ids(v: &Value) -> Vec<u32> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| u32::try_from(x.as_u64().unwrap()).unwrap())
        .collect()
}

#[test]
fn tiktoken_reference_ids() {
    common::ensure_tables();
    let rows = rows("tiktoken.jsonl");
    assert!(rows.len() > 400);
    for enc in Encoding::ALL {
        let bpe = enc.load().unwrap();
        for r in &rows {
            let text = r["text"].as_str().unwrap();
            let want = ids(&r[enc.name()]);
            assert_eq!(bpe.encode(text), want, "{enc} {text:?}");
            assert_eq!(bpe.count(text), want.len(), "{enc} {text:?}");
        }
    }
}

#[test]
fn long_pieces_use_heap_path_correctly() {
    common::ensure_tables();
    // Lange Stücke ohne Leerzeichen (CJK, Wiederholungen) laufen über den Heap-Pfad; die
    // Zählung muss mit der Summe kürzerer Referenzen konsistent und plausibel sein.
    let bpe = Encoding::O200kBase.load().unwrap();
    let cjk = "人工智能正在改变世界".repeat(40);
    let n = bpe.count(&cjk);
    assert!(n > 40 && n < 400, "{n}");
    let ids = bpe.encode(&cjk);
    assert_eq!(ids.len(), n);
    assert_eq!(
        bpe.count(&"x".repeat(1000)),
        bpe.encode(&"x".repeat(1000)).len()
    );
}

#[test]
fn hf_trained_fixtures() {
    let mut cache: HashMap<String, HfTokenizer> = HashMap::new();
    let mut checked = 0;
    for r in rows("hf.jsonl") {
        let name = r["tokenizer"].as_str().unwrap().to_owned();
        let tok = cache.entry(name.clone()).or_insert_with(|| {
            HfTokenizer::from_file(&fixture(&format!("hf_{name}.json"))).unwrap()
        });
        let text = r["text"].as_str().unwrap();
        let want = usize::try_from(r["count"].as_u64().unwrap()).unwrap();
        assert_eq!(tok.count(text), want, "{name} {text:?}");
        checked += 1;
    }
    assert_eq!(cache.len(), 6);
    assert!(checked > 500);
}

#[test]
fn hf_real_tokenizers() {
    let Ok(dir) = std::env::var("AIGW_TOKENIZER_HF_DIR") else {
        eprintln!("AIGW_TOKENIZER_HF_DIR nicht gesetzt – übersprungen");
        return;
    };
    let mut cache: HashMap<String, Option<HfTokenizer>> = HashMap::new();
    let mut checked = 0;
    let mut mismatches = Vec::new();
    for r in rows("hf_real.jsonl") {
        let name = r["tokenizer"].as_str().unwrap().to_owned();
        let tok = cache.entry(name.clone()).or_insert_with(|| {
            HfTokenizer::from_file(&Path::new(&dir).join(format!("{name}.json"))).ok()
        });
        let Some(tok) = tok else { continue };
        let text = r["text"].as_str().unwrap();
        let want = usize::try_from(r["count"].as_u64().unwrap()).unwrap();
        let got = tok.count(text);
        if got != want {
            mismatches.push(format!("{name}: {got} statt {want} für {text:?}"));
        }
        checked += 1;
    }
    eprintln!("{checked} Texte gegen echte Tokenizer geprüft");
    let missing: Vec<&String> = cache
        .iter()
        .filter(|(_, t)| t.is_none())
        .map(|(n, _)| n)
        .collect();
    assert!(missing.is_empty(), "nicht ladbar: {missing:?}");
    assert!(checked > 0);
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

#[test]
fn heuristic_close_to_bpe_for_prose_and_code() {
    common::ensure_tables();
    // Die Heuristik `generic` liegt für Prosa (EN/DE) und Code nahe an o200k_base.
    let samples = [
        "The quick brown fox jumps over the lazy dog. It was a sunny day and everyone was \
         happy to be outside, enjoying the weather and talking about their plans.",
        "Die Bundesregierung hat am Mittwoch beschlossen, die Förderung für Wärmepumpen zu \
         verlängern. Größere Unternehmen müssen außerdem Berichte über ihre Energieeffizienz \
         vorlegen.",
        "fn main() {\n    let v: Vec<u32> = (0..10).map(|x| x * 2).collect();\n    \
         println!(\"{:?}\", v);\n}\n",
    ];
    for s in samples {
        let exact = Encoding::O200kBase.count(s) as f64;
        let est = heuristic::estimate(s, Profile::Generic) as f64;
        let ratio = est / exact;
        assert!((0.8..=1.35).contains(&ratio), "{ratio:.2} für {s:?}");
    }
}
