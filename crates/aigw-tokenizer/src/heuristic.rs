// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Heuristische Token-Schätzung mit Sprachfaktor (Claude, Gemini, unbekannte Modelle).
//!
//! Für Claude und Gemini sind die Tokenizer nicht offen verfügbar. Geschätzt wird aus den
//! Zeichen in einem Durchlauf:
//!
//! - **Schriftklassen:** lateinische Schrift (inkl. Umlaute/Akzente) über „Zeichen je Token“
//!   der erkannten Sprache; CJK (Han, Kana, Hangul) über „Tokens je Zeichen“; andere Alphabete
//!   (Kyrillisch, Griechisch, Arabisch, Devanagari …) über eigene Zeichen je Token; Zeichen
//!   oberhalb der BMP (Emojis) mit 1,5 Tokens.
//! - **Sprache des lateinischen Anteils:** Code (Anteil typischer Code-Satzzeichen ≥ 8 %),
//!   Deutsch (Umlaute/ß oder häufige deutsche Funktionswörter), sonst Englisch.
//!
//! Die Faktoren je [`Profile`] sind Erfahrungswerte mit leichtem Aufschlag (lieber etwas mehr
//! als zu wenig – sie dienen der Reservierung); `generic` liegt für Prosa und Code innerhalb
//! von −20 %/+35 % von o200k_base (Test `heuristic_close_to_bpe_for_prose_and_code`). Für
//! Claude/Gemini gibt es keine offene Referenz; die Werte sind Schätzungen.

use std::fmt;

/// Faktorensatz der Heuristik.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Profile {
    /// Unbekannte Modelle (orientiert an o200k_base).
    Generic,
    /// Anthropic Claude (Tokenizer erzeugt mehr Tokens je Zeichen als o200k).
    Claude,
    /// Google Gemini/Gemma (SentencePiece, großes Vokabular; effizient bei CJK).
    Gemini,
}

/// Erkannte Sprache des lateinischen Anteils.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    /// Englisch (und ähnliche Sprachen ohne Merkmale).
    English,
    /// Deutsch.
    German,
    /// Quelltext/strukturierte Daten.
    Code,
}

#[derive(Debug, Clone, Copy)]
struct Factors {
    /// Zeichen je Token (lateinische Schrift) für Englisch, Deutsch, Code.
    en: f64,
    de: f64,
    code: f64,
    /// Tokens je CJK-Zeichen.
    cjk: f64,
    /// Zeichen je Token für andere Alphabete.
    other: f64,
}

impl Profile {
    /// Bezeichner (`heuristic:<name>`).
    pub const fn name(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::Claude => "claude",
            Self::Gemini => "gemini",
        }
    }

    /// Gegenstück zu [`Profile::name`].
    pub fn from_name(s: &str) -> Option<Self> {
        Some(match s {
            "generic" => Self::Generic,
            "claude" => Self::Claude,
            "gemini" => Self::Gemini,
            _ => return None,
        })
    }

    const fn factors(self) -> Factors {
        match self {
            // Kalibriert an o200k_base (EN 5,2 / DE 5,2 / Code 3,5 Zeichen je Token, CJK
            // 0,7 Tokens je Zeichen) mit ≈ 10–15 % Aufschlag.
            Self::Generic => Factors {
                en: 4.6,
                de: 4.4,
                code: 3.0,
                cjk: 0.8,
                other: 3.0,
            },
            // Claude: kleineres Vokabular, Effizienz etwa wie cl100k_base (DE 3,7, CJK ≈ 1,05).
            Self::Claude => Factors {
                en: 4.0,
                de: 3.3,
                code: 3.0,
                cjk: 1.1,
                other: 2.0,
            },
            // Gemini: SentencePiece mit 256k Vokabular, ähnlich o200k.
            Self::Gemini => Factors {
                en: 4.6,
                de: 4.2,
                code: 3.2,
                cjk: 0.8,
                other: 3.0,
            },
        }
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "heuristic:{}", self.name())
    }
}

fn is_cjk(cp: u32) -> bool {
    matches!(cp,
        0x1100..=0x11FF | 0x3040..=0x30FF | 0x3100..=0x312F | 0x3130..=0x318F
        | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xA960..=0xA97F | 0xAC00..=0xD7AF
        | 0xF900..=0xFAFF | 0xFF66..=0xFF9F | 0x20000..=0x2FA1F)
}

/// Häufige deutsche Funktionswörter (kleingeschrieben).
const GERMAN_WORDS: [&str; 24] = [
    "der", "die", "das", "und", "ist", "nicht", "ein", "eine", "zu", "mit", "für", "auf", "dem",
    "den", "sich", "des", "von", "wir", "ich", "sie", "es", "werden", "wird", "auch",
];

/// Zählwerte eines Durchlaufs.
#[derive(Debug, Default, Clone, Copy)]
struct Stats {
    latin: u64,
    cjk: u64,
    other: u64,
    astral: u64,
    non_space: u64,
    code_punct: u64,
    umlauts: u64,
    words: u64,
    german_words: u64,
}

/// Wortpuffer (Kleinbuchstaben, UTF-8) für den Abgleich mit [`GERMAN_WORDS`] ohne Allokation.
#[derive(Default)]
struct Word {
    buf: [u8; 8],
    len: usize,
    /// Wort ist länger als der Puffer (zählt als Wort, nie als Funktionswort).
    long: bool,
}

impl Word {
    #[inline]
    fn push(&mut self, c: char) {
        let c = if c.is_ascii() {
            c.to_ascii_lowercase()
        } else {
            c
        };
        let mut tmp = [0u8; 4];
        let b = c.encode_utf8(&mut tmp).as_bytes();
        match self.buf.get_mut(self.len..self.len + b.len()) {
            Some(dst) if !self.long => {
                dst.copy_from_slice(b);
                self.len += b.len();
            }
            _ => self.long = true,
        }
    }

    #[inline]
    fn flush(&mut self, s: &mut Stats) {
        if self.len == 0 && !self.long {
            return;
        }
        s.words += 1;
        if !self.long
            && let Some(w) = self.buf.get(..self.len)
            && GERMAN_WORDS.iter().any(|g| g.as_bytes() == w)
        {
            s.german_words += 1;
        }
        self.len = 0;
        self.long = false;
    }
}

fn stats(text: &str) -> Stats {
    let mut s = Stats::default();
    let mut word = Word::default();
    for c in text.chars() {
        let cp = u32::from(c);
        if let Ok(b) = u8::try_from(cp)
            && b < 0x80
        {
            s.latin += 1;
            if b.is_ascii_alphabetic() {
                s.non_space += 1;
                word.push(c);
                continue;
            }
            if !b.is_ascii_whitespace() {
                s.non_space += 1;
            }
            if matches!(
                b,
                b'{' | b'}'
                    | b'['
                    | b']'
                    | b'('
                    | b')'
                    | b';'
                    | b'='
                    | b'<'
                    | b'>'
                    | b'_'
                    | b'/'
                    | b'\\'
                    | b'*'
                    | b'&'
                    | b'|'
                    | b'#'
                    | b'$'
                    | b':'
                    | b'"'
                    | b'`'
            ) {
                s.code_punct += 1;
            }
            word.flush(&mut s);
            continue;
        }
        if !c.is_whitespace() {
            s.non_space += 1;
        }
        if matches!(cp, 0x80..=0x024F | 0x1E00..=0x1EFF | 0x2000..=0x206F) {
            s.latin += 1;
            if matches!(c, 'ä' | 'ö' | 'ü' | 'ß' | 'Ä' | 'Ö' | 'Ü') {
                s.umlauts += 1;
            }
        } else if is_cjk(cp) {
            s.cjk += 1;
        } else if cp > 0xFFFF {
            s.astral += 1;
        } else {
            s.other += 1;
        }
        if c.is_alphabetic() {
            word.push(c);
        } else {
            word.flush(&mut s);
        }
    }
    word.flush(&mut s);
    s
}

/// Sprache des lateinischen Anteils.
pub fn language(text: &str) -> Language {
    classify(&stats(text))
}

fn classify(s: &Stats) -> Language {
    if s.non_space > 0 && s.code_punct * 100 >= s.non_space * 8 {
        Language::Code
    } else if s.umlauts * 200 >= s.latin.max(1)
        || (s.words >= 4 && s.german_words * 100 >= s.words * 12)
    {
        Language::German
    } else {
        Language::English
    }
}

/// Geschätzte Tokens von `text` mit dem Faktorensatz `profile`.
pub fn estimate(text: &str, profile: Profile) -> u64 {
    if text.is_empty() {
        return 0;
    }
    let s = stats(text);
    let f = profile.factors();
    let cpt = match classify(&s) {
        Language::English => f.en,
        Language::German => f.de,
        Language::Code => f.code,
    };
    #[allow(clippy::cast_precision_loss)]
    let t = s.latin as f64 / cpt
        + s.cjk as f64 * f.cjk
        + s.other as f64 / f.other
        + s.astral as f64 * 1.5;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let n = t.ceil() as u64;
    n.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages() {
        assert_eq!(
            language("The quick brown fox jumps over the lazy dog."),
            Language::English
        );
        assert_eq!(
            language("Die Katze sitzt auf dem Dach und es ist nicht kalt."),
            Language::German
        );
        assert_eq!(language("Größe"), Language::German);
        assert_eq!(
            language("fn main() { let x = vec![1, 2]; println!(\"{:?}\", x); }"),
            Language::Code
        );
    }

    #[test]
    fn estimates() {
        assert_eq!(estimate("", Profile::Claude), 0);
        assert_eq!(estimate("a", Profile::Claude), 1);
        // 44 Zeichen Englisch / 4,0 → 11
        assert_eq!(
            estimate(
                "The quick brown fox jumps over the lazy dog.",
                Profile::Claude
            ),
            11
        );
        // CJK: 10 Zeichen × 1,1
        assert_eq!(estimate("日本語のテキストです", Profile::Claude), 11);
        assert!(estimate("😀😀", Profile::Gemini) >= 3);
    }
}
