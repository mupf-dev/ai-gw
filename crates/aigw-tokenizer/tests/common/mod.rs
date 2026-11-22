// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Gemeinsame Hilfen der Integrationstests: `*.tiktoken`-Dateien aus den Tabellen in `data/`
//! zurückgewinnen (byte-genau, die SHA-256-Summe stimmt) und für Builds ohne
//! `embedded-tokenizers` zur Laufzeit installieren.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Once;

use aigw_tokenizer::{Encoding, ranks};
use base64::Engine as _;

/// Originaldatei `<name>.tiktoken` aus `data/<name>.bin`.
pub(crate) fn tiktoken_file(e: Encoding) -> Vec<u8> {
    let bin = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("data")
            .join(format!("{}.bin", e.name())),
    )
    .unwrap();
    let n = u32::from_le_bytes(bin[4..8].try_into().unwrap());
    let mut rest = &bin[8..];
    let mut out = Vec::with_capacity(bin.len() * 2);
    for rank in 0..n {
        let len = usize::from(rest[0]);
        let tok = &rest[1..=len];
        rest = &rest[len + 1..];
        out.extend_from_slice(
            base64::engine::general_purpose::STANDARD
                .encode(tok)
                .as_bytes(),
        );
        out.extend_from_slice(format!(" {rank}\n").as_bytes());
    }
    assert!(rest.is_empty());
    out
}

/// Stellt sicher, dass beide Tabellen verfügbar sind (eingebettet oder zur Laufzeit geladen).
pub(crate) fn ensure_tables() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        for e in Encoding::ALL {
            ranks::install_tiktoken(e, &tiktoken_file(e)).unwrap();
        }
    });
}
