// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Token-Zählung von ai-gw (AIGW-71, Entscheidung F46).
//!
//! Eigenentwicklung ohne fremde Tokenizer-Bibliothek:
//!
//! - **OpenAI** `o200k_base` (GPT-4o, GPT-4.1, GPT-5, o-Serie) und `cl100k_base` (GPT-4,
//!   GPT-3.5, Embeddings v3): exakte BPE wie tiktoken. Die Rangtabellen sind im Binärformat
//!   eingebettet (`data/*.bin`, aus den von OpenAI veröffentlichten `*.tiktoken`, MIT), der
//!   Pre-Tokenizer ist als Zustandsautomat nachgebaut ([`Encoding`]).
//! - **Hugging Face** `tokenizer.json` (Llama 3, Mistral, …): BPE und Unigram, soweit diese
//!   Modelle sie brauchen ([`HfTokenizer`]); Datei je Modellgruppe (`settings.tokenizer:
//!   hf:<datei>`).
//! - **Heuristik** mit Sprachfaktor (Deutsch/Englisch/Code/CJK) für Claude und Gemini, deren
//!   Tokenizer nicht offen sind ([`heuristic`]).
//!
//! Zuordnung Modell → Tokenizer: automatisch nach Anbieter und Anbieter-Modellname
//! ([`auto`]), überschreibbar je Modellgruppe ([`TokenizerSetting`]).
//!
//! Verwendung im Gateway: Prompt-Schätzung für die Reservierung von Limits und Budgets und
//! Usage-Schätzung bei Abbruch bzw. fehlender Usage. Ausführlich:
//! `docs/konzept/11-limits-und-budgets.md` §9.

mod bpe;
pub mod heuristic;
mod hf;
mod pretok;
mod regex_split;
mod unicode;

use std::fmt;
use std::sync::{Arc, OnceLock};

pub use bpe::TableError;
pub use heuristic::{Language, Profile};
pub use hf::{HfError, HfTokenizer};

use bpe::Ranks;
use pretok::{Pattern, Pieces};

static CL100K_BLOB: &[u8] = include_bytes!("../data/cl100k_base.bin");
static O200K_BLOB: &[u8] = include_bytes!("../data/o200k_base.bin");

/// Eingebettete BPE-Kodierung von OpenAI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Encoding {
    /// `cl100k_base` (GPT-4, GPT-3.5-Turbo, text-embedding-3-*, ada-002).
    Cl100kBase,
    /// `o200k_base` (GPT-4o, GPT-4.1, GPT-5, o1/o3/o4).
    O200kBase,
}

/// Geladene BPE (Rangtabelle + Pre-Tokenizer).
#[derive(Debug)]
pub struct CoreBpe {
    ranks: Ranks,
    pattern: Pattern,
}

impl CoreBpe {
    /// Anzahl Tokens von `text`.
    pub fn count(&self, text: &str) -> usize {
        Pieces::new(text, self.pattern)
            .map(|p| self.ranks.count_piece(p.as_bytes()))
            .sum()
    }

    /// Token-IDs von `text` (ohne Spezial-Tokens, wie `encode_ordinary` von tiktoken).
    pub fn encode(&self, text: &str) -> Vec<u32> {
        let mut out = Vec::with_capacity(text.len() / 3);
        for p in Pieces::new(text, self.pattern) {
            self.ranks.encode_piece(p.as_bytes(), &mut out);
        }
        out
    }

    /// Größe des Vokabulars (ohne Spezial-Tokens).
    pub fn vocab_size(&self) -> usize {
        self.ranks.len()
    }
}

impl Encoding {
    /// Alle eingebetteten Kodierungen.
    pub const ALL: [Self; 2] = [Self::Cl100kBase, Self::O200kBase];

    /// Name wie bei tiktoken.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cl100kBase => "cl100k_base",
            Self::O200kBase => "o200k_base",
        }
    }

    /// Gegenstück zu [`Encoding::name`] (auch Kurzform ohne `_base`).
    pub fn from_name(s: &str) -> Option<Self> {
        Some(match s {
            "cl100k_base" | "cl100k" => Self::Cl100kBase,
            "o200k_base" | "o200k" => Self::O200kBase,
            _ => return None,
        })
    }

    /// Geladene Tabelle (beim ersten Zugriff aus den eingebetteten Daten, ≈ 10–30 ms).
    pub fn load(self) -> Result<&'static CoreBpe, &'static TableError> {
        static CL: OnceLock<Result<CoreBpe, TableError>> = OnceLock::new();
        static O2: OnceLock<Result<CoreBpe, TableError>> = OnceLock::new();
        let (cell, blob, pattern) = match self {
            Self::Cl100kBase => (&CL, CL100K_BLOB, Pattern::Cl100k),
            Self::O200kBase => (&O2, O200K_BLOB, Pattern::O200k),
        };
        cell.get_or_init(|| {
            unicode::warm_up();
            Ranks::from_blob(blob).map(|ranks| CoreBpe { ranks, pattern })
        })
        .as_ref()
    }

    /// Anzahl Tokens; fällt bei einer (nicht zu erwartenden) defekten Tabelle auf die
    /// Heuristik zurück.
    pub fn count(self, text: &str) -> usize {
        match self.load() {
            Ok(b) => b.count(text),
            Err(_) => {
                usize::try_from(heuristic::estimate(text, Profile::Generic)).unwrap_or(usize::MAX)
            }
        }
    }
}

impl fmt::Display for Encoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Ein Zähler für Tokens (günstig zu klonen).
#[derive(Debug, Clone)]
pub enum Tokenizer {
    /// Eingebettete OpenAI-BPE (exakt).
    Bpe(Encoding),
    /// Aus `tokenizer.json` (exakt bis auf Spezial-Tokens).
    Hf(Arc<HfTokenizer>, Arc<str>),
    /// Heuristik (Schätzung).
    Heuristic(Profile),
}

impl Tokenizer {
    /// Anzahl Tokens von `text`.
    pub fn count(&self, text: &str) -> u64 {
        let n = match self {
            Self::Bpe(e) => e.count(text),
            Self::Hf(t, _) => t.count(text),
            Self::Heuristic(p) => return heuristic::estimate(text, *p),
        };
        u64::try_from(n).unwrap_or(u64::MAX)
    }

    /// `true` für echte Tokenizer (BPE/HF), `false` für die Heuristik.
    pub fn is_exact(&self) -> bool {
        !matches!(self, Self::Heuristic(_))
    }
}

impl fmt::Display for Tokenizer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bpe(e) => write!(f, "{e}"),
            Self::Hf(_, name) => write!(f, "hf:{name}"),
            Self::Heuristic(p) => write!(f, "{p}"),
        }
    }
}

/// Einstellung `settings.tokenizer` einer Modellgruppe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenizerSetting {
    /// Automatisch nach Anbieter und Modellname ([`auto`]); Standard.
    Auto,
    /// Feste OpenAI-Kodierung (`o200k_base`, `cl100k_base`).
    Encoding(Encoding),
    /// Heuristik (`heuristic`, `heuristic:claude`, `heuristic:gemini`).
    Heuristic(Profile),
    /// `tokenizer.json` relativ zu `limits.tokenizer_dir` (`hf:<datei>`).
    HfFile(String),
}

impl TokenizerSetting {
    /// Liest den Wert von `settings.tokenizer`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.is_empty() || s == "auto" {
            return Ok(Self::Auto);
        }
        if let Some(e) = Encoding::from_name(s) {
            return Ok(Self::Encoding(e));
        }
        if s == "heuristic" {
            return Ok(Self::Heuristic(Profile::Generic));
        }
        if let Some(p) = s.strip_prefix("heuristic:") {
            return Profile::from_name(p).map(Self::Heuristic).ok_or_else(|| {
                format!("settings.tokenizer: Heuristik {p:?} unbekannt (generic, claude, gemini)")
            });
        }
        if let Some(f) = s.strip_prefix("hf:") {
            return valid_file_name(f)
                .map(|()| Self::HfFile(f.to_owned()))
                .map_err(|e| format!("settings.tokenizer: {e}"));
        }
        Err(format!(
            "settings.tokenizer: {s:?} unbekannt (auto, o200k_base, cl100k_base, heuristic, \
             heuristic:claude, heuristic:gemini, hf:<datei>)"
        ))
    }
}

/// Dateiname für `hf:` – relativ, ohne `..`, ohne Steuerzeichen (die Datei muss in
/// `limits.tokenizer_dir` liegen).
fn valid_file_name(f: &str) -> Result<(), String> {
    if f.is_empty() || f.len() > 255 {
        return Err("hf:<datei> braucht einen Dateinamen (≤ 255 Zeichen)".into());
    }
    if f.starts_with('/') || f.contains('\\') || f.contains(':') {
        return Err(format!(
            "hf:{f:?}: nur relative Pfade innerhalb von limits.tokenizer_dir"
        ));
    }
    if f.split('/')
        .any(|seg| seg.is_empty() || seg == "." || seg == "..")
    {
        return Err(format!(
            "hf:{f:?}: Pfadsegmente \"\", \".\" und \"..\" sind nicht erlaubt"
        ));
    }
    if f.chars().any(char::is_control) {
        return Err(format!("hf:{f:?}: Steuerzeichen im Dateinamen"));
    }
    Ok(())
}

/// Automatische Zuordnung nach Anbieter (`ProviderKind::as_str`) und Anbieter-Modellname.
///
/// | Modell | Tokenizer |
/// |---|---|
/// | `gpt-4o*`, `gpt-4.1*`, `gpt-4.5*`, `gpt-5*`, `o1*`, `o3*`, `o4*`, `chatgpt-*`, `gpt-oss*`, `codex*` | `o200k_base` |
/// | `gpt-4*`, `gpt-3.5*`/`gpt-35*`, `text-embedding-3*`, `text-embedding-ada-002` | `cl100k_base` |
/// | `*llama-3*`/`*llama3*` | `cl100k_base` (Llama 3 = cl100k + 28k Tokens; Näherung) |
/// | `*claude*` bzw. Anbieter `anthropic` | Heuristik `claude` |
/// | `*gemini*`, `*gemma*` | Heuristik `gemini` |
/// | sonst | Heuristik `generic` |
///
/// Namen werden ohne Präfix bis zum letzten `/` bzw. `.` vor einem Modellnamen verglichen
/// (`openai/gpt-4o`, `anthropic.claude-…`, `us.meta.llama3-…`).
pub fn auto(provider: Option<&str>, model: &str) -> Tokenizer {
    let m = model.to_ascii_lowercase();
    let base = m.rsplit('/').next().unwrap_or(&m);
    let starts = |p: &[&str]| p.iter().any(|x| base.starts_with(x));
    if m.contains("claude") || provider == Some("anthropic") {
        return Tokenizer::Heuristic(Profile::Claude);
    }
    if m.contains("gemini") || m.contains("gemma") {
        return Tokenizer::Heuristic(Profile::Gemini);
    }
    if starts(&[
        "gpt-4o", "gpt-4.1", "gpt-4.5", "gpt-5", "o1", "o3", "o4", "chatgpt-", "gpt-oss", "codex",
    ]) {
        return Tokenizer::Bpe(Encoding::O200kBase);
    }
    if starts(&[
        "gpt-4",
        "gpt-3.5",
        "gpt-35",
        "text-embedding-3",
        "text-embedding-ada-002",
    ]) {
        return Tokenizer::Bpe(Encoding::Cl100kBase);
    }
    if m.contains("llama-3") || m.contains("llama3") {
        return Tokenizer::Bpe(Encoding::Cl100kBase);
    }
    Tokenizer::Heuristic(Profile::Generic)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings() {
        assert_eq!(TokenizerSetting::parse("auto"), Ok(TokenizerSetting::Auto));
        assert_eq!(
            TokenizerSetting::parse("o200k_base"),
            Ok(TokenizerSetting::Encoding(Encoding::O200kBase))
        );
        assert_eq!(
            TokenizerSetting::parse("heuristic:claude"),
            Ok(TokenizerSetting::Heuristic(Profile::Claude))
        );
        assert_eq!(
            TokenizerSetting::parse("hf:llama3/tokenizer.json"),
            Ok(TokenizerSetting::HfFile("llama3/tokenizer.json".into()))
        );
        for bad in [
            "hf:../x.json",
            "hf:/etc/passwd",
            "hf:a//b",
            "hf:",
            "heuristic:x",
            "bert",
        ] {
            assert!(TokenizerSetting::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn auto_mapping() {
        let name = |p: Option<&str>, m: &str| auto(p, m).to_string();
        assert_eq!(name(Some("openai"), "gpt-4o-mini"), "o200k_base");
        assert_eq!(name(Some("openai"), "gpt-5"), "o200k_base");
        assert_eq!(name(Some("openai"), "o3-mini"), "o200k_base");
        assert_eq!(name(Some("openai"), "gpt-4-turbo"), "cl100k_base");
        assert_eq!(
            name(Some("openai"), "text-embedding-3-small"),
            "cl100k_base"
        );
        assert_eq!(
            name(Some("openai-compatible"), "openai/gpt-4.1"),
            "o200k_base"
        );
        assert_eq!(
            name(Some("bedrock"), "anthropic.claude-3-5-sonnet-20241022-v2:0"),
            "heuristic:claude"
        );
        assert_eq!(name(Some("anthropic"), "x"), "heuristic:claude");
        assert_eq!(name(Some("vertex"), "gemini-2.5-pro"), "heuristic:gemini");
        assert_eq!(
            name(Some("vllm"), "meta-llama/Llama-3.1-8B-Instruct"),
            "cl100k_base"
        );
        assert_eq!(
            name(Some("mistral"), "mistral-large-latest"),
            "heuristic:generic"
        );
        assert_eq!(name(Some("mock"), "mock"), "heuristic:generic");
    }

    #[test]
    fn tables_load() {
        assert_eq!(Encoding::Cl100kBase.load().unwrap().vocab_size(), 100_256);
        assert_eq!(Encoding::O200kBase.load().unwrap().vocab_size(), 199_998);
        assert_eq!(Encoding::O200kBase.count("Hello, world!"), 4);
    }
}
