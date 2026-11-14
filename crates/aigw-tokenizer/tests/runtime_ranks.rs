// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Rangtabellen zur Laufzeit (AIGW-95): Prüfsumme und Format in beiden Builds; mit
//! `embedded-tokenizers` bleibt das Verzeichnis unbeachtet, ohne das Feature wird daraus
//! geladen bzw. auf die Heuristik zurückgefallen (`cargo test -p aigw-tokenizer
//! --no-default-features`).

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use aigw_tokenizer::{Encoding, RanksError, ranks};

#[test]
fn names_urls_checksums() {
    assert_eq!(Encoding::O200kBase.file_name(), "o200k_base.tiktoken");
    assert_eq!(
        Encoding::Cl100kBase.source_url(),
        "https://openaipublic.blob.core.windows.net/encodings/cl100k_base.tiktoken"
    );
    assert_eq!(ranks::EMBEDDED, cfg!(feature = "embedded-tokenizers"));
}

#[test]
fn parse_checks_sha256_and_format() {
    let cl = common::tiktoken_file(Encoding::Cl100kBase);
    ranks::verify(Encoding::Cl100kBase, &cl).unwrap();
    // Datei der falschen Kodierung.
    assert!(matches!(
        ranks::verify(Encoding::O200kBase, &cl),
        Err(RanksError::Checksum { .. })
    ));
    // Ein verändertes Byte.
    let mut bad = cl.clone();
    bad[100] ^= 0x01;
    let err = ranks::parse_tiktoken(Encoding::Cl100kBase, &bad).unwrap_err();
    assert!(matches!(err, RanksError::Checksum { .. }), "{err}");
    assert!(err.to_string().contains("cl100k_base.tiktoken"));
    // Gültige Datei ergibt dieselbe Tabelle wie tiktoken.
    let bpe = ranks::parse_tiktoken(Encoding::Cl100kBase, &cl).unwrap();
    assert_eq!(bpe.vocab_size(), 100_256);
    assert_eq!(bpe.encode("Hello, world!"), [9906, 11, 1917, 0]);
}

#[cfg(feature = "embedded-tokenizers")]
#[test]
fn embedded_build_ignores_ranks_dir() {
    let out = ranks::install_from_dir(std::path::Path::new("/nonexistent/aigw-ranks"));
    assert_eq!(out.len(), 2);
    assert!(
        out.iter()
            .all(|(_, o)| matches!(o, aigw_tokenizer::DirOutcome::Embedded))
    );
    for e in Encoding::ALL {
        assert!(e.is_available());
    }
    assert!(aigw_tokenizer::Tokenizer::Bpe(Encoding::O200kBase).is_exact());
}

/// Ein Test (globaler Zustand der installierten Tabellen): Fallback, fehlende/defekte Dateien,
/// Nachladen.
#[cfg(not(feature = "embedded-tokenizers"))]
#[test]
fn runtime_build_loads_from_dir_or_falls_back() {
    use aigw_tokenizer::{DirOutcome, Profile, Tokenizer, heuristic};

    const TEXT: &str = "Hello, world! Wie viele Tokens hat dieser Satz?";

    // Ohne Tabellen: Heuristik, nicht exakt.
    for e in Encoding::ALL {
        assert!(!e.is_available());
        assert!(e.load().is_err());
    }
    let est = usize::try_from(heuristic::estimate(TEXT, Profile::Generic)).unwrap();
    assert_eq!(Encoding::O200kBase.count(TEXT), est);
    assert!(!Tokenizer::Bpe(Encoding::O200kBase).is_exact());

    let dir = std::env::temp_dir().join(format!("aigw-ranks-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // Leeres Verzeichnis: beide fehlen.
    let out = ranks::install_from_dir(&dir);
    assert!(out.iter().all(|(_, o)| matches!(o, DirOutcome::Missing)));

    // o200k gültig, cl100k verändert.
    let o2 = common::tiktoken_file(Encoding::O200kBase);
    std::fs::write(dir.join("o200k_base.tiktoken"), &o2).unwrap();
    let mut cl = common::tiktoken_file(Encoding::Cl100kBase);
    let last = cl.len() - 2;
    cl[last] = b'9';
    std::fs::write(dir.join("cl100k_base.tiktoken"), &cl).unwrap();
    for (e, o) in ranks::install_from_dir(&dir) {
        match e {
            Encoding::O200kBase => assert!(matches!(o, DirOutcome::Installed), "{o:?}"),
            Encoding::Cl100kBase => assert!(
                matches!(o, DirOutcome::Invalid(RanksError::Checksum { .. })),
                "{o:?}"
            ),
        }
    }
    assert!(Encoding::O200kBase.is_available());
    assert_eq!(Encoding::O200kBase.count("Hello, world!"), 4);
    assert!(Tokenizer::Bpe(Encoding::O200kBase).is_exact());
    assert!(!Encoding::Cl100kBase.is_available());

    // Korrigierte Datei: nachladen (o200k bleibt installiert).
    std::fs::write(
        dir.join("cl100k_base.tiktoken"),
        common::tiktoken_file(Encoding::Cl100kBase),
    )
    .unwrap();
    let out = ranks::install_from_dir(&dir);
    assert!(out.iter().all(|(_, o)| matches!(o, DirOutcome::Installed)));
    assert_eq!(Encoding::Cl100kBase.load().unwrap().vocab_size(), 100_256);

    std::fs::remove_dir_all(&dir).unwrap();
}
