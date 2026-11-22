// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Gemeinsame Hilfen der Integrationstests.
//!
//! Die echten Rangtabellen von OpenAI liegen nicht im Repository (AIGW-92). Tests, die sie
//! brauchen, laden sie aus `$AIGW_TOKENIZER_RANKS_DIR` (`scripts/fetch-tokenizers.sh`) und
//! überspringen sich sonst ([`real_tables`]). Alles andere läuft offline mit einer kleinen
//! synthetischen Tabelle im tiktoken-Format ([`synthetic_tiktoken`]).

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use aigw_tokenizer::ranks;
use base64::Engine as _;

/// Zusätzliche Tokens der synthetischen Tabelle nach den 256 Einzelbytes (Rang 256, 257, …).
pub(crate) const SYNTHETIC_MERGES: [&str; 6] = ["he", "ll", "hell", "hello", " w", " wo"];

/// Synthetische `*.tiktoken`-Datei: alle 256 Einzelbytes (Rang = Bytewert), danach
/// [`SYNTHETIC_MERGES`].
pub(crate) fn synthetic_tiktoken() -> Vec<u8> {
    let mut out = Vec::new();
    let tokens = (0..=255u8)
        .map(|b| vec![b])
        .chain(SYNTHETIC_MERGES.iter().map(|t| t.as_bytes().to_vec()));
    for (rank, tok) in tokens.enumerate() {
        out.extend_from_slice(
            base64::engine::general_purpose::STANDARD
                .encode(tok)
                .as_bytes(),
        );
        out.extend_from_slice(format!(" {rank}\n").as_bytes());
    }
    out
}

/// `true`, wenn die echten Tabellen installiert sind (sonst Test überspringen).
pub(crate) fn real_tables(test: &str) -> bool {
    ranks::tables_for_test(test)
}
