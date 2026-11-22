// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Generischer Regex-Split für `tokenizer.json` (Pre-Tokenizer `Split`, `ByteLevel` mit
//! `use_regex`).
//!
//! Die Muster der Tokenizer (GPT-2, Llama 3, Tekken, tiktoken) verwenden den Lookahead
//! `\s+(?!\S)`, den die `regex`-Crate nicht kennt. Er wird gleichwertig nachgebildet: Die
//! Alternative wird zu einer benannten Gruppe `(\s+)`; endet ein solcher Treffer mit mindestens
//! zwei Zeichen vor einem Nicht-Leerzeichen, wird das letzte Zeichen zurückgegeben (genau das
//! macht das Backtracking von `\s+(?!\S)`). Possessive Quantoren (`?+`, `++`, `*+`, `{n}+`)
//! werden – für diese Muster gleichwertig – zu gierigen. Die `regex`-Crate wählt wie
//! Backtracking-Engines die erste passende Alternative (leftmost-first).

use regex::Regex;

const LOOKAHEAD: &str = r"\s+(?!\S)";
const WS_GROUP: &str = "aigwws";

/// Kompilierter Split-Ausdruck.
#[derive(Debug, Clone)]
pub(crate) struct RegexSplit {
    re: Regex,
    lookahead: bool,
}

/// Entfernt possessive Quantoren (`x?+` → `x?`), außerhalb von Zeichenklassen.
fn strip_possessive(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    let mut chars = p.chars();
    let mut in_class = false;
    let mut prev_quant = false;
    while let Some(c) = chars.next() {
        if c == '\\' {
            out.push(c);
            if let Some(n) = chars.next() {
                out.push(n);
                // \p{…}, \P{…}, \x{…}, \u{…}: Klammer gehört zum Escape (kein Quantor).
                if matches!(n, 'p' | 'P' | 'x' | 'u') && chars.as_str().starts_with('{') {
                    for b in chars.by_ref() {
                        out.push(b);
                        if b == '}' {
                            break;
                        }
                    }
                }
            }
            // Ein Escape ist ein Atom: ein folgendes `+` ist ein Quantor, kein possessives.
            prev_quant = false;
            continue;
        }
        if in_class {
            if c == ']' {
                in_class = false;
            }
            out.push(c);
            prev_quant = false;
            continue;
        }
        if c == '[' {
            in_class = true;
            out.push(c);
            prev_quant = false;
            continue;
        }
        if c == '+' && prev_quant {
            prev_quant = false;
            continue;
        }
        prev_quant = matches!(c, '?' | '+' | '*' | '}');
        out.push(c);
    }
    out
}

impl RegexSplit {
    /// Übersetzt ein Muster aus `tokenizer.json` bzw. tiktoken.
    pub(crate) fn new(pattern: &str) -> Result<Self, String> {
        let mut p = strip_possessive(pattern);
        let lookahead = p.contains(LOOKAHEAD);
        if lookahead {
            p = p.replacen(LOOKAHEAD, &format!(r"(?P<{WS_GROUP}>\s+)"), 1);
        }
        for la in ["(?=", "(?!", "(?<=", "(?<!"] {
            if p.contains(la) {
                return Err(format!(
                    "Regex {pattern:?}: Lookaround {la}…) wird nicht unterstützt"
                ));
            }
        }
        let re = Regex::new(&p).map_err(|e| format!("Regex {pattern:?}: {e}"))?;
        Ok(Self { re, lookahead })
    }

    /// Treffer als Byte-Bereiche (aufsteigend, nicht überlappend; leere Treffer übersprungen).
    pub(crate) fn matches(&self, s: &str, out: &mut Vec<(usize, usize)>) {
        let mut pos = 0;
        let mut locs = self.re.capture_locations();
        let ws_idx = if self.lookahead {
            self.re.capture_names().position(|n| n == Some(WS_GROUP))
        } else {
            None
        };
        while pos < s.len() {
            let Some(m) = self.re.captures_read_at(&mut locs, s, pos) else {
                break;
            };
            let (a, mut b) = (m.start(), m.end());
            if b == a {
                // Leerer Treffer: ein Zeichen weiter.
                pos = a + s
                    .get(a..)
                    .and_then(|r| r.chars().next())
                    .map_or(1, char::len_utf8);
                continue;
            }
            if let Some(gi) = ws_idx
                && locs.get(gi).is_some()
                && b < s.len()
                && let Some(seg) = s.get(a..b)
            {
                let mut it = seg.char_indices();
                if let (Some(_), Some((last, _))) = (it.next(), it.next_back()) {
                    b = a + last;
                }
            }
            out.push((a, b));
            pos = b;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn possessive_and_lookahead() {
        assert_eq!(
            strip_possessive(r"a?+b++[+]+c{1,3}+\++"),
            r"a?b+[+]+c{1,3}\++"
        );
        let r = RegexSplit::new(r"\p{L}+|\s+(?!\S)|\s+").unwrap();
        let s = "ab   cd  ";
        let mut m = Vec::new();
        r.matches(s, &mut m);
        let parts: Vec<&str> = m.iter().map(|&(a, b)| &s[a..b]).collect();
        assert_eq!(parts, ["ab", "  ", " ", "cd", "  "]);
        assert!(RegexSplit::new(r"a(?=b)").is_err());
    }
}
