// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Rangtabellen zur Laufzeit (AIGW-95, AIGW-92): Prüfsumme, Format, Laden aus einem
//! Verzeichnis und Heuristik als Rückfall – offline mit einer synthetischen Tabelle (die echten
//! Tabellen prüft `reference.rs`, sofern vorhanden).

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use aigw_tokenizer::{DirOutcome, Encoding, Profile, RanksError, Tokenizer, heuristic, ranks};

#[test]
fn names_urls_checksums() {
    assert_eq!(Encoding::O200kBase.file_name(), "o200k_base.tiktoken");
    assert_eq!(
        Encoding::Cl100kBase.source_url(),
        "https://openaipublic.blob.core.windows.net/encodings/cl100k_base.tiktoken"
    );
    assert_eq!(
        ranks::sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    for e in Encoding::ALL {
        assert_eq!(e.sha256().len(), 64);
    }
}

#[test]
fn parse_checks_sha256_and_format() {
    let syn = common::synthetic_tiktoken();
    // Prüfsumme passt zu keiner offiziellen Datei.
    for e in Encoding::ALL {
        assert!(matches!(
            ranks::verify(e, &syn),
            Err(RanksError::Checksum { .. })
        ));
    }
    let err = ranks::parse_tiktoken(Encoding::Cl100kBase, &syn).unwrap_err();
    assert!(matches!(err, RanksError::Checksum { .. }), "{err}");
    assert!(err.to_string().contains("cl100k_base.tiktoken"));

    // Format und Tabelle (ohne Prüfsumme).
    let bpe = ranks::parse_tiktoken_unverified(Encoding::Cl100kBase, &syn).unwrap();
    assert_eq!(bpe.vocab_size(), 256 + common::SYNTHETIC_MERGES.len());
    // "hello" ist ein Token (Rang 259), " world" = " wo" + "r" + "l" + "d".
    assert_eq!(bpe.encode("hello world"), [259, 261, 114, 108, 100]);
    assert_eq!(bpe.count("hello world"), 5);

    let bad = |data: &[u8], want: &str| {
        let err = ranks::parse_tiktoken_unverified(Encoding::O200kBase, data).unwrap_err();
        assert!(
            matches!(err, RanksError::Format { .. }) && err.to_string().contains(want),
            "{err}"
        );
    };
    bad(b"YQ==\n", "Leerzeichen fehlt");
    bad(b"YQ== x\n", "keine Zahl");
    bad(b"YQ== 1\n", "lückenlos");
    bad(b"!!! 0\n", "Base64");
    // Formal gültig, aber Einzelbytes fehlen.
    let err = ranks::parse_tiktoken_unverified(Encoding::O200kBase, b"YQ== 0\n").unwrap_err();
    assert!(matches!(err, RanksError::Table { .. }), "{err}");
}

/// Ein Test (globaler Zustand der installierten Tabellen): Rückfall auf die Heuristik,
/// fehlende/ungültige Dateien, Installation.
#[test]
fn loads_from_dir_or_falls_back() {
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
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Leeres Verzeichnis: beide fehlen.
    let out = ranks::install_from_dir(&dir);
    assert_eq!(out.len(), 2);
    assert!(out.iter().all(|(_, o)| matches!(o, DirOutcome::Missing)));

    // Andere Datei unter dem richtigen Namen: abgelehnt (Prüfsumme), weiter Heuristik.
    std::fs::write(dir.join("o200k_base.tiktoken"), common::synthetic_tiktoken()).unwrap();
    for (e, o) in ranks::install_from_dir(&dir) {
        match e {
            Encoding::O200kBase => assert!(
                matches!(o, DirOutcome::Invalid(RanksError::Checksum { .. })),
                "{o:?}"
            ),
            Encoding::Cl100kBase => assert!(matches!(o, DirOutcome::Missing), "{o:?}"),
        }
    }
    assert!(!Encoding::O200kBase.is_available());

    // Installierte Tabelle wird verwendet; das Verzeichnis wird dafür nicht mehr gelesen.
    let bpe = ranks::parse_tiktoken_unverified(Encoding::O200kBase, &common::synthetic_tiktoken())
        .unwrap();
    assert!(ranks::install_parsed(Encoding::O200kBase, bpe));
    assert!(Encoding::O200kBase.is_available());
    assert!(Tokenizer::Bpe(Encoding::O200kBase).is_exact());
    assert_eq!(Encoding::O200kBase.count("hello world"), 5);
    assert_eq!(Tokenizer::Bpe(Encoding::O200kBase).count("hello world"), 5);
    let out = ranks::install_from_dir(&dir);
    assert!(out.iter().any(|(e, o)| *e == Encoding::O200kBase
        && matches!(o, DirOutcome::Installed)));
    assert!(!Encoding::Cl100kBase.is_available());

    std::fs::remove_dir_all(&dir).unwrap();
}
