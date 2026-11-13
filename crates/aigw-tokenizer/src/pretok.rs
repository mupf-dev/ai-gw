// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Handgeschriebene Pre-Tokenizer für `cl100k_base` und `o200k_base`.
//!
//! tiktoken zerlegt den Text vor der BPE mit einem regulären Ausdruck (`fancy-regex`, mit
//! Lookahead `\s+(?!\S)`). Hier ist jede Alternative als kleiner Zustandsautomat mit exakt
//! derselben Backtracking-Semantik nachgebaut – ohne Regex-Engine im Hot Path. Die
//! Gleichwertigkeit prüfen die Tests gegen die Referenz-IDs von tiktoken und gegen den
//! generischen Regex-Split ([`crate::regex_split`]) auf Zufallstexten.
//!
//! cl100k_base (tiktoken 0.14):
//! ```text
//! '(?i:[sdmt]|ll|ve|re)|[^\r\n\p{L}\p{N}]?+\p{L}++|\p{N}{1,3}+| ?[^\s\p{L}\p{N}]++[\r\n]*+
//! |\s++$|\s*[\r\n]|\s+(?!\S)|\s
//! ```
//! o200k_base:
//! ```text
//! [^\r\n\p{L}\p{N}]?[\p{Lu}\p{Lt}\p{Lm}\p{Lo}\p{M}]*[\p{Ll}\p{Lm}\p{Lo}\p{M}]+(?i:'s|'t|'re|'ve|'m|'ll|'d)?
//! |[^\r\n\p{L}\p{N}]?[\p{Lu}\p{Lt}\p{Lm}\p{Lo}\p{M}]+[\p{Ll}\p{Lm}\p{Lo}\p{M}]*(?i:'s|'t|'re|'ve|'m|'ll|'d)?
//! |\p{N}{1,3}| ?[^\s\p{L}\p{N}]+[\r\n/]*|\s*[\r\n]+|\s+(?!\S)|\s+
//! ```

use crate::unicode::{LETTER, LOWERISH, NUMBER, SPACE, UPPERISH, class};

/// Art des Pre-Tokenizers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pattern {
    /// `cl100k_base`.
    Cl100k,
    /// `o200k_base`.
    O200k,
}

/// Zeichen an Byte-Position `i` samt Länge in Bytes.
#[inline]
fn at(s: &str, i: usize) -> Option<(char, usize)> {
    let b = *s.as_bytes().get(i)?;
    if b < 0x80 {
        return Some((char::from(b), 1));
    }
    let c = s.get(i..)?.chars().next()?;
    Some((c, c.len_utf8()))
}

#[inline]
fn cls_at(s: &str, i: usize) -> Option<(u8, usize)> {
    at(s, i).map(|(c, l)| (class(c), l))
}

/// `[^\r\n\p{L}\p{N}]` – Präfixzeichen vor einem Wort.
#[inline]
fn is_prefix(c: char, k: u8) -> bool {
    c != '\r' && c != '\n' && k & (LETTER | NUMBER) == 0
}

/// `[^\s\p{L}\p{N}]` – Satzzeichen, Symbole, Kombinationszeichen …
#[inline]
fn is_punct(k: u8) -> bool {
    k & (SPACE | LETTER | NUMBER) == 0
}

/// Ende des längsten Laufs ab `i`, dessen Zeichen `pred` erfüllen.
#[inline]
fn run(s: &str, mut i: usize, pred: impl Fn(char, u8) -> bool) -> usize {
    while let Some((c, l)) = at(s, i) {
        if !pred(c, class(c)) {
            break;
        }
        i += l;
    }
    i
}

/// Höchstens `max` Zeichen, die `pred` erfüllen.
#[inline]
fn run_max(s: &str, mut i: usize, max: usize, pred: impl Fn(u8) -> bool) -> usize {
    for _ in 0..max {
        match cls_at(s, i) {
            Some((k, l)) if pred(k) => i += l,
            _ => break,
        }
    }
    i
}

/// `(?i:'s|'t|'re|'ve|'m|'ll|'d)` ab `i` (Ende oder `None`). `ſ` (U+017F) gilt ohne
/// Groß-/Kleinschreibung als `s`.
#[inline]
fn contraction(s: &str, i: usize) -> Option<usize> {
    let rest = s.as_bytes().get(i..)?;
    if rest.first() != Some(&b'\'') {
        return None;
    }
    let one = |b: u8| matches!(b.to_ascii_lowercase(), b's' | b't' | b'm' | b'd');
    let two = |a: u8, b: u8| {
        matches!(
            (a.to_ascii_lowercase(), b.to_ascii_lowercase()),
            (b'r', b'e') | (b'v', b'e') | (b'l', b'l')
        )
    };
    match rest.get(1..) {
        Some([a, b, ..]) if two(*a, *b) => Some(i + 3),
        Some([a, ..]) if one(*a) => Some(i + 2),
        // „ſ“ = C5 BF
        Some([0xC5, 0xBF, ..]) => Some(i + 3),
        _ => None,
    }
}

/// cl100k-Kontraktion: `'(?i:[sdmt]|ll|ve|re)` – Reihenfolge der Alternativen egal, kein
/// Präfix-Konflikt.
#[inline]
fn contraction_cl100k(s: &str, i: usize) -> Option<usize> {
    contraction(s, i)
}

/// Leerraum ab `i` (Zeichen `c` ist Leerraum): Alternativen `\s++$` (nur cl100k),
/// `\s*[\r\n]+`, `\s+(?!\S)`, `\s`/`\s+`.
#[inline]
fn whitespace(s: &str, i: usize, trailing_whole: bool) -> usize {
    let mut j = i;
    let mut last_nl_end = None;
    let mut last_start = i;
    let mut n = 0usize;
    while let Some((c, l)) = at(s, j) {
        if class(c) & SPACE == 0 {
            break;
        }
        if c == '\r' || c == '\n' {
            last_nl_end = Some(j + l);
        }
        last_start = j;
        j += l;
        n += 1;
    }
    if trailing_whole && j == s.len() {
        return j; // \s++$
    }
    if let Some(e) = last_nl_end {
        return e; // \s*[\r\n]+ (endet nach dem letzten Zeilenumbruch des Laufs)
    }
    if j == s.len() {
        return j; // \s+(?!\S) am Textende
    }
    if n >= 2 {
        return last_start; // \s+(?!\S): letztes Leerzeichen gehört zum nächsten Stück
    }
    j // \s bzw. \s+ mit genau einem Zeichen
}

/// Satzzeichen-Lauf ab `i` (` ?[^\s\p{L}\p{N}]+` + Zeilenumbrüche bzw. `/`).
#[inline]
fn punct(s: &str, i: usize, c: char, k: u8, slash: bool) -> Option<usize> {
    let start = if c == ' ' {
        match cls_at(s, i + 1) {
            Some((k2, _)) if is_punct(k2) => i + 1,
            _ => return None,
        }
    } else if is_punct(k) {
        i
    } else {
        return None;
    };
    let e = run(s, start, |_, k| is_punct(k));
    Some(run(s, e, |c, _| {
        c == '\r' || c == '\n' || (slash && c == '/')
    }))
}

fn next_cl100k(s: &str, i: usize) -> Option<usize> {
    let (c, l) = at(s, i)?;
    let k = class(c);
    if c == '\''
        && let Some(e) = contraction_cl100k(s, i)
    {
        return Some(e);
    }
    // [^\r\n\p{L}\p{N}]?+\p{L}++
    if is_prefix(c, k) {
        if let Some((k2, _)) = cls_at(s, i + l)
            && k2 & LETTER != 0
        {
            return Some(run(s, i + l, |_, k| k & LETTER != 0));
        }
    } else if k & LETTER != 0 {
        return Some(run(s, i, |_, k| k & LETTER != 0));
    }
    if k & NUMBER != 0 {
        return Some(run_max(s, i, 3, |k| k & NUMBER != 0));
    }
    if let Some(e) = punct(s, i, c, k, false) {
        return Some(e);
    }
    if k & SPACE != 0 {
        return Some(whitespace(s, i, true));
    }
    Some(i + l)
}

/// Rumpf der beiden o200k-Wortalternativen ab `s0` (ohne Präfix). `first`: Alternative 1
/// (`U*W+`), sonst Alternative 2 (`U+W*`). Liefert das Ende inkl. optionaler Kontraktion.
#[inline]
fn word_o200k(s: &str, s0: usize, first: bool) -> Option<usize> {
    // U-Lauf mit Merker auf das Ende des letzten Zeichens, das auch in W liegt.
    let mut e = s0;
    let mut last_w_end = None;
    while let Some((k, l)) = cls_at(s, e) {
        if k & UPPERISH == 0 {
            break;
        }
        e += l;
        if k & LOWERISH != 0 {
            last_w_end = Some(e);
        }
    }
    let end = if first {
        match cls_at(s, e) {
            // Greedy U* ganz, danach W+ (das Zeichen nach dem U-Lauf ist Ll).
            Some((k, _)) if k & LOWERISH != 0 => run(s, e, |_, k| k & LOWERISH != 0),
            // Backtracking: W+ beginnt beim letzten W-Zeichen im U-Lauf und endet dort.
            _ => last_w_end?,
        }
    } else {
        if e == s0 {
            return None;
        }
        run(s, e, |_, k| k & LOWERISH != 0)
    };
    Some(contraction(s, end).unwrap_or(end))
}

fn next_o200k(s: &str, i: usize) -> Option<usize> {
    let (c, l) = at(s, i)?;
    let k = class(c);
    let pre = is_prefix(c, k);
    for first in [true, false] {
        if pre && let Some(e) = word_o200k(s, i + l, first) {
            return Some(e);
        }
        if let Some(e) = word_o200k(s, i, first) {
            return Some(e);
        }
    }
    if k & NUMBER != 0 {
        return Some(run_max(s, i, 3, |k| k & NUMBER != 0));
    }
    if let Some(e) = punct(s, i, c, k, true) {
        return Some(e);
    }
    if k & SPACE != 0 {
        return Some(whitespace(s, i, false));
    }
    Some(i + l)
}

/// Iterator über die Stücke eines Textes.
#[derive(Debug, Clone)]
pub(crate) struct Pieces<'a> {
    s: &'a str,
    pos: usize,
    pattern: Pattern,
}

impl<'a> Pieces<'a> {
    pub(crate) fn new(s: &'a str, pattern: Pattern) -> Self {
        Self { s, pos: 0, pattern }
    }
}

impl<'a> Iterator for Pieces<'a> {
    type Item = &'a str;

    #[inline]
    fn next(&mut self) -> Option<&'a str> {
        let i = self.pos;
        if i >= self.s.len() {
            return None;
        }
        let e = match self.pattern {
            Pattern::Cl100k => next_cl100k(self.s, i),
            Pattern::O200k => next_o200k(self.s, i),
        }?;
        // Fortschritt ist garantiert (jede Alternative verbraucht ≥ 1 Zeichen); zur Sicherheit
        // trotzdem mindestens ein Zeichen.
        let e = if e > i {
            e
        } else {
            i + at(self.s, i).map_or(1, |(_, l)| l)
        };
        self.pos = e;
        self.s.get(i..e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(s: &str, p: Pattern) -> Vec<&str> {
        Pieces::new(s, p).collect()
    }

    #[test]
    fn cl100k_basics() {
        assert_eq!(
            split("Hello world's  end\n\n  x  ", Pattern::Cl100k),
            [
                "Hello", " world", "'s", " ", " end", "\n\n", " ", " x", "  "
            ]
        );
        assert_eq!(split("12345", Pattern::Cl100k), ["123", "45"]);
        assert_eq!(split("a  \n  ", Pattern::Cl100k), ["a", "  \n  "]);
        assert_eq!(split(" !!\r\nx", Pattern::Cl100k), [" !!\r\n", "x"]);
    }

    #[test]
    fn o200k_basics() {
        assert_eq!(
            split("HelloWORLD isn't XMLHttp", Pattern::O200k),
            ["Hello", "WORLD", " isn't", " XMLHttp"]
        );
        assert_eq!(split("a  \n  ", Pattern::O200k), ["a", "  \n", "  "]);
        assert_eq!(split("x/y//\n", Pattern::O200k), ["x", "/y", "//\n"]);
        assert_eq!(split("日本語です。", Pattern::O200k), ["日本語です", "。"]);
    }
}

#[cfg(test)]
mod equivalence {
    //! Handgeschriebene Pre-Tokenizer = generischer Regex-Split mit den Original-Mustern von
    //! tiktoken (Fixture-Texte und Zufallstexte).

    use super::*;
    use crate::regex_split::RegexSplit;

    const CL100K: &str = r"'(?i:[sdmt]|ll|ve|re)|[^\r\n\p{L}\p{N}]?+\p{L}++|\p{N}{1,3}+| ?[^\s\p{L}\p{N}]++[\r\n]*+|\s++$|\s*[\r\n]|\s+(?!\S)|\s";
    const O200K: &str = r"[^\r\n\p{L}\p{N}]?[\p{Lu}\p{Lt}\p{Lm}\p{Lo}\p{M}]*[\p{Ll}\p{Lm}\p{Lo}\p{M}]+(?i:'s|'t|'re|'ve|'m|'ll|'d)?|[^\r\n\p{L}\p{N}]?[\p{Lu}\p{Lt}\p{Lm}\p{Lo}\p{M}]+[\p{Ll}\p{Lm}\p{Lo}\p{M}]*(?i:'s|'t|'re|'ve|'m|'ll|'d)?|\p{N}{1,3}| ?[^\s\p{L}\p{N}]+[\r\n/]*|\s*[\r\n]+|\s+(?!\S)|\s+";

    fn texts() -> Vec<String> {
        let mut out: Vec<String> = include_str!("../tests/fixtures/tiktoken.jsonl")
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter_map(|v| v.get("text")?.as_str().map(str::to_owned))
            .collect();
        let pool: Vec<&str> = vec![
            "a", "B", "ü", "ß", "É", "1", "٣", "½", " ", "  ", "\n", "\r", "\t", "\u{a0}",
            "\u{3000}", "'", "'s", "'LL", "ſ", ".", "!", "/", "-", "_", "(", "東", "ア", "한", "ʰ",
            "ǅ", "\u{301}", "\u{93f}", "क", "😀", "🇩🇪", "𝐀", "𝟎", "x", "Hello", " the",
        ];
        let mut rnd = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..3000 {
            rnd = rnd
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let len = (rnd >> 58) as usize + 1;
            let mut s = String::new();
            for _ in 0..len {
                rnd = rnd
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                s.push_str(pool[(rnd >> 33) as usize % pool.len()]);
            }
            out.push(s);
        }
        out
    }

    fn by_regex<'a>(re: &RegexSplit, s: &'a str) -> Vec<&'a str> {
        let mut m = Vec::new();
        re.matches(s, &mut m);
        m.iter().map(|&(a, b)| &s[a..b]).collect()
    }

    #[test]
    fn handwritten_equals_regex() {
        for (pat, p) in [(CL100K, Pattern::Cl100k), (O200K, Pattern::O200k)] {
            let re = RegexSplit::new(pat).unwrap();
            for t in texts() {
                let a: Vec<&str> = Pieces::new(&t, p).collect();
                assert_eq!(a, by_regex(&re, &t), "{p:?} {t:?}");
            }
        }
    }
}
