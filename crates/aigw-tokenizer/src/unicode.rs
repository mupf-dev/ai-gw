// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Unicode-Zeichenklassen der Pre-Tokenizer (`\p{L}`, `\p{Lu}`, `\p{N}`, `\p{M}`, `\s` …).
//!
//! Die Bereiche stammen aus `regex-syntax` – denselben Tabellen, die tiktoken (über
//! `fancy-regex` → `regex`) verwendet. Für die BMP liegt eine Tabelle mit einem Byte je
//! Codepunkt vor (64 KiB, einmalig beim ersten Zugriff gebaut), darüber eine sortierte
//! Bereichsliste mit binärer Suche (Emojis, seltene CJK-Erweiterungen).

use std::sync::OnceLock;

use regex_syntax::hir::{Class, HirKind};

/// `Lu` oder `Lt` (Groß-/Titelbuchstabe).
pub(crate) const UPPER: u8 = 1;
/// `Ll` (Kleinbuchstabe).
pub(crate) const LOWER: u8 = 2;
/// `Lm` oder `Lo` (Modifikator-/sonstiger Buchstabe, z. B. CJK).
pub(crate) const OTHER_LETTER: u8 = 4;
/// `M` (Kombinationszeichen).
pub(crate) const MARK: u8 = 8;
/// `N` (Ziffer/Zahl).
pub(crate) const NUMBER: u8 = 16;
/// `White_Space` (wie `\s` im Unicode-Modus).
pub(crate) const SPACE: u8 = 32;

/// `\p{L}`.
pub(crate) const LETTER: u8 = UPPER | LOWER | OTHER_LETTER;
/// `[\p{Lu}\p{Lt}\p{Lm}\p{Lo}\p{M}]` (o200k: „Großteil“ eines Worts).
pub(crate) const UPPERISH: u8 = UPPER | OTHER_LETTER | MARK;
/// `[\p{Ll}\p{Lm}\p{Lo}\p{M}]` (o200k: „Kleinteil“ eines Worts).
pub(crate) const LOWERISH: u8 = LOWER | OTHER_LETTER | MARK;

struct Table {
    bmp: Box<[u8]>,
    /// Sortierte, disjunkte Bereiche oberhalb der BMP: (Anfang, Ende inkl., Klassen).
    astral: Vec<(u32, u32, u8)>,
}

fn ranges(pattern: &str) -> Vec<(u32, u32)> {
    let Ok(hir) = regex_syntax::ParserBuilder::new()
        .unicode(true)
        .build()
        .parse(pattern)
    else {
        return Vec::new();
    };
    match hir.kind() {
        HirKind::Class(Class::Unicode(c)) => c
            .ranges()
            .iter()
            .map(|r| (u32::from(r.start()), u32::from(r.end())))
            .collect(),
        _ => Vec::new(),
    }
}

fn build() -> Table {
    let classes: [(&str, u8); 8] = [
        (r"\p{Lu}", UPPER),
        (r"\p{Lt}", UPPER),
        (r"\p{Ll}", LOWER),
        (r"\p{Lm}", OTHER_LETTER),
        (r"\p{Lo}", OTHER_LETTER),
        (r"\p{M}", MARK),
        (r"\p{N}", NUMBER),
        (r"\s", SPACE),
    ];
    let mut bmp = vec![0u8; 0x1_0000].into_boxed_slice();
    let mut astral_raw: Vec<(u32, u32, u8)> = Vec::new();
    for (pat, bit) in classes {
        for (lo, hi) in ranges(pat) {
            if lo <= 0xFFFF {
                for cp in lo..=hi.min(0xFFFF) {
                    if let Some(slot) = bmp.get_mut(cp as usize) {
                        *slot |= bit;
                    }
                }
            }
            if hi > 0xFFFF {
                astral_raw.push((lo.max(0x1_0000), hi, bit));
            }
        }
    }
    // Bereiche oberhalb der BMP zu disjunkten Segmenten mit vereinigten Klassen zusammenfassen.
    let mut bounds: Vec<u32> = astral_raw
        .iter()
        .flat_map(|&(lo, hi, _)| [lo, hi.saturating_add(1)])
        .collect();
    bounds.sort_unstable();
    bounds.dedup();
    let mut astral = Vec::with_capacity(bounds.len());
    for w in bounds.windows(2) {
        let (lo, next) = (w[0], w[1]);
        let bits = astral_raw
            .iter()
            .filter(|&&(a, b, _)| a <= lo && lo <= b)
            .fold(0u8, |acc, &(_, _, bit)| acc | bit);
        if bits != 0 {
            astral.push((lo, next - 1, bits));
        }
    }
    Table { bmp, astral }
}

fn table() -> &'static Table {
    static T: OnceLock<Table> = OnceLock::new();
    T.get_or_init(build)
}

/// Klassen-Bits eines Zeichens.
#[inline]
pub(crate) fn class(c: char) -> u8 {
    let t = table();
    let cp = u32::from(c);
    if let Some(&b) = t.bmp.get(cp as usize) {
        return b;
    }
    match t.astral.binary_search_by(|&(lo, hi, _)| {
        if hi < cp {
            std::cmp::Ordering::Less
        } else if lo > cp {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    }) {
        Ok(i) => t.astral.get(i).map_or(0, |&(_, _, b)| b),
        Err(_) => 0,
    }
}

/// Baut die Tabellen vorab (z. B. beim Start), damit die erste Anfrage nicht wartet.
pub(crate) fn warm_up() {
    let _ = table();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes() {
        assert_eq!(class('a'), LOWER);
        assert_eq!(class('Z'), UPPER);
        assert_eq!(class('ǅ'), UPPER); // Lt
        assert_eq!(class('ʰ'), OTHER_LETTER); // Lm
        assert_eq!(class('東'), OTHER_LETTER); // Lo
        assert_eq!(class('\u{0301}'), MARK);
        assert_eq!(class('7'), NUMBER);
        assert_eq!(class('½'), NUMBER);
        assert_eq!(class(' '), SPACE);
        assert_eq!(class('\u{3000}'), SPACE);
        assert_eq!(class('\n'), SPACE);
        assert_eq!(class('!'), 0);
        assert_eq!(class('😀'), 0);
        assert_eq!(class('𝐀'), UPPER); // U+1D400, oberhalb der BMP
        assert_eq!(class('𠀀'), OTHER_LETTER); // CJK-Erweiterung B
        assert_eq!(class('𝟎'), NUMBER);
    }
}
