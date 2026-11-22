// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Preiskatalog der direkten Anbieter (OpenAI, Anthropic, Mistral, Gemini über Vertex …).
//!
//! Mitgeliefert ist `crates/aigw-billing/catalog/prices.yaml` – eine eigene, gepflegte Datei
//! (Listenpreise aus den öffentlichen Preisseiten der Anbieter, siehe THIRD-PARTY.md).
//! Optional ersetzt eine per URL geladene Datei den Katalog (`billing.price_suggestions.catalog`):
//! im selben Format (`aigw`) oder als `model_prices_and_context_window.json` von LiteLLM
//! (`litellm`).

use std::collections::HashMap;

use aigw_core::resource::{Amount, TokenPrices};
use aigw_core::types::ProviderKind;
use serde::Deserialize;
use time::OffsetDateTime;

use super::decimal::scaled_amount;

/// Mitgelieferter Katalog.
const BUILTIN: &str = include_str!("../../catalog/prices.yaml");

/// Datei im Format `aigw`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CatalogFile {
    /// Stand (frei, z. B. Datum).
    version: String,
    /// Währung (nur `USD`).
    currency: String,
    /// Modelle.
    models: Vec<CatalogEntry>,
}

/// Ein Modell im Katalog.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CatalogEntry {
    /// Anbieter.
    pub provider: ProviderKind,
    /// Modellname beim Anbieter.
    pub model: String,
    /// Weitere Namen (Snapshots mit Datum, `-latest` …).
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Preis gültig ab (`JJJJ-MM-TT`), falls bekannt.
    #[serde(default)]
    pub valid_from: Option<String>,
    /// Preise je 1 Mio. Tokens.
    #[serde(default)]
    pub per_million_tokens: TokenPrices,
    /// Fester Preis je Anfrage.
    #[serde(default)]
    pub per_request: Option<Amount>,
    /// Preis je Bild.
    #[serde(default)]
    pub per_image: Option<Amount>,
    /// Preis je Minute Audio-Eingabe.
    #[serde(default)]
    pub per_audio_minute: Option<Amount>,
    /// Preis je 1 Mio. Zeichen (Sprachausgabe).
    #[serde(default)]
    pub per_million_characters: Option<Amount>,
    /// Quelle (Preisseite).
    #[serde(default)]
    pub source: Option<String>,
}

impl CatalogEntry {
    /// `validFrom` als Zeitpunkt (Mitternacht UTC).
    pub fn valid_from_at(&self) -> Option<OffsetDateTime> {
        let mut it = self.valid_from.as_deref()?.splitn(3, '-');
        let y: i32 = it.next()?.parse().ok()?;
        let m: u8 = it.next()?.parse().ok()?;
        let d: u8 = it.next()?.parse().ok()?;
        let month = time::Month::try_from(m).ok()?;
        time::Date::from_calendar_date(y, month, d)
            .ok()
            .map(|d| d.midnight().assume_utc())
    }
}

/// Preiskatalog mit Suchindex.
#[derive(Debug, Clone)]
pub struct Catalog {
    /// Stand.
    pub version: String,
    entries: Vec<CatalogEntry>,
    index: HashMap<(ProviderKind, String), usize>,
}

/// Wie ein Katalogeintrag gefunden wurde.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogMatch {
    /// Name oder Alias.
    Exact,
    /// Name ohne Datums-/Versionsanhang (`gpt-4o-2024-08-06` → `gpt-4o`).
    Base,
}

impl Catalog {
    /// Mitgelieferter Katalog.
    pub fn builtin() -> Result<Self, String> {
        Self::parse_aigw(BUILTIN)
    }

    /// Katalog im Format `aigw` (YAML oder JSON).
    pub fn parse_aigw(text: &str) -> Result<Self, String> {
        let f: CatalogFile = serde_saphyr::from_str(text).map_err(|e| e.to_string())?;
        if f.currency != "USD" {
            return Err(format!("currency {:?}: nur USD", f.currency));
        }
        for e in &f.models {
            let spec = aigw_core::resource::PriceSpec {
                provider: Some(e.provider),
                model: e.model.clone(),
                valid_from: OffsetDateTime::UNIX_EPOCH,
                per_million_tokens: e.per_million_tokens,
                per_request: e.per_request,
                per_image: e.per_image,
                per_audio_minute: e.per_audio_minute,
                per_million_characters: e.per_million_characters,
            };
            aigw_core::validate::price(&spec)
                .map_err(|m| format!("{}/{}: {m}", e.provider.as_str(), e.model))?;
            if e.valid_from.is_some() && e.valid_from_at().is_none() {
                return Err(format!(
                    "{}/{}: validFrom JJJJ-MM-TT",
                    e.provider.as_str(),
                    e.model
                ));
            }
        }
        Ok(Self::new(f.version, f.models))
    }

    /// `model_prices_and_context_window.json` (LiteLLM, MIT): Einträge von OpenAI, Anthropic,
    /// Mistral und Gemini (als `vertex`) mit Preisen je Token.
    pub fn parse_litellm(text: &str) -> Result<Self, String> {
        let v: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(text).map_err(|e| e.to_string())?;
        let mut models = Vec::new();
        for (name, e) in &v {
            let Some(o) = e.as_object() else { continue };
            let provider = match o.get("litellm_provider").and_then(|p| p.as_str()) {
                Some("openai") => ProviderKind::OpenAi,
                Some("anthropic") => ProviderKind::Anthropic,
                Some("mistral") => ProviderKind::Mistral,
                Some("gemini" | "vertex_ai-language-models") => ProviderKind::Vertex,
                _ => continue,
            };
            let model = name.rsplit_once('/').map_or(name.as_str(), |(_, m)| m);
            let per_token = |k: &str| {
                o.get(k)
                    .and_then(|n| n.as_number())
                    .and_then(|n| scaled_amount(&n.to_string(), 6))
            };
            let prices = TokenPrices {
                input: per_token("input_cost_per_token"),
                output: per_token("output_cost_per_token"),
                cached_input: per_token("cache_read_input_token_cost"),
                cache_write: per_token("cache_creation_input_token_cost"),
                reasoning: per_token("output_cost_per_reasoning_token"),
                audio_input: per_token("input_cost_per_audio_token"),
                audio_output: per_token("output_cost_per_audio_token"),
                image_input: None,
            };
            if prices.input.is_none() && prices.output.is_none() {
                continue;
            }
            models.push(CatalogEntry {
                provider,
                model: model.to_owned(),
                aliases: Vec::new(),
                valid_from: None,
                per_million_tokens: prices,
                per_request: None,
                per_image: None,
                per_audio_minute: None,
                per_million_characters: None,
                source: Some("litellm".into()),
            });
        }
        if models.is_empty() {
            return Err("keine verwertbaren Einträge".into());
        }
        Ok(Self::new("litellm".into(), models))
    }

    fn new(version: String, entries: Vec<CatalogEntry>) -> Self {
        let mut index = HashMap::new();
        for (i, e) in entries.iter().enumerate() {
            for name in std::iter::once(&e.model).chain(&e.aliases) {
                // Erster Eintrag gewinnt (bei LiteLLM: Anbieterpräfix und ohne kollidieren).
                index
                    .entry((e.provider, name.to_ascii_lowercase()))
                    .or_insert(i);
            }
        }
        Self {
            version,
            entries,
            index,
        }
    }

    /// Anzahl Modelle.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` ohne Modelle.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Eintrag für `model` des Anbieters `provider`.
    pub fn lookup(
        &self,
        provider: ProviderKind,
        model: &str,
    ) -> Option<(&CatalogEntry, CatalogMatch)> {
        let m = model.trim().to_ascii_lowercase();
        let get = |name: &str| {
            self.index
                .get(&(provider, name.to_owned()))
                .and_then(|i| self.entries.get(*i))
        };
        if let Some(e) = get(&m) {
            return Some((e, CatalogMatch::Exact));
        }
        let base = strip_version(&m)?;
        get(&base).map(|e| (e, CatalogMatch::Base))
    }
}

/// Entfernt einen Datums- bzw. Versionsanhang: `-2024-08-06`, `-20250929`, `@20250929`,
/// `-latest`, `-v1:0`.
fn strip_version(m: &str) -> Option<String> {
    let mut s = m.to_owned();
    let before = s.clone();
    for suffix in ["-latest", "-v1:0", "-v1", "-v2:0"] {
        if let Some(r) = s.strip_suffix(suffix) {
            s = r.to_owned();
        }
    }
    let b = s.as_bytes();
    let n = b.len();
    let digits =
        |r: std::ops::Range<usize>| b.get(r).is_some_and(|x| x.iter().all(u8::is_ascii_digit));
    if n > 11 && (b[n - 9] == b'-' || b[n - 9] == b'@') && digits(n - 8..n) {
        s.truncate(n - 9);
    } else if n > 11
        && b[n - 11] == b'-'
        && b[n - 6] == b'-'
        && b[n - 3] == b'-'
        && digits(n - 10..n - 6)
        && digits(n - 5..n - 3)
        && digits(n - 2..n)
    {
        s.truncate(n - 11);
    }
    (s != before).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_catalog_is_valid() {
        let c = Catalog::builtin().unwrap();
        assert!(c.len() > 20);
        let (e, how) = c.lookup(ProviderKind::OpenAi, "gpt-5.4-mini").unwrap();
        assert_eq!(how, CatalogMatch::Exact);
        assert!(e.per_million_tokens.input.is_some());
        let (_, how) = c
            .lookup(ProviderKind::OpenAi, "gpt-5.4-mini-2026-03-17")
            .unwrap();
        assert!(matches!(how, CatalogMatch::Exact | CatalogMatch::Base));
        assert!(
            c.lookup(ProviderKind::Anthropic, "claude-sonnet-4-5-20250929")
                .is_some()
        );
        assert!(c.lookup(ProviderKind::OpenAi, "kein-modell").is_none());
    }

    #[test]
    fn strips_versions() {
        assert_eq!(
            strip_version("gpt-4o-2024-08-06").as_deref(),
            Some("gpt-4o")
        );
        assert_eq!(
            strip_version("claude-sonnet-4-5-20250929").as_deref(),
            Some("claude-sonnet-4-5")
        );
        assert_eq!(
            strip_version("claude-sonnet-4-5@20250929").as_deref(),
            Some("claude-sonnet-4-5")
        );
        assert_eq!(
            strip_version("mistral-large-latest").as_deref(),
            Some("mistral-large")
        );
        assert_eq!(strip_version("gpt-4o"), None);
    }

    #[test]
    fn parses_litellm_format() {
        let c = Catalog::parse_litellm(
            r#"{
              "sample_spec": {"litellm_provider": "openai"},
              "gpt-x": {"litellm_provider": "openai", "input_cost_per_token": 7.5e-07,
                        "output_cost_per_token": 4.5e-06, "cache_read_input_token_cost": 7.5e-08},
              "gemini/gemini-y": {"litellm_provider": "gemini", "input_cost_per_token": 3e-07,
                                  "output_cost_per_token": 2.5e-06},
              "other": {"litellm_provider": "cohere", "input_cost_per_token": 1e-06}
            }"#,
        )
        .unwrap();
        assert_eq!(c.len(), 2);
        let (e, _) = c.lookup(ProviderKind::OpenAi, "gpt-x").unwrap();
        assert_eq!(
            e.per_million_tokens.input.map(|a| a.to_string()).as_deref(),
            Some("0.75")
        );
        assert_eq!(
            e.per_million_tokens
                .cached_input
                .map(|a| a.to_string())
                .as_deref(),
            Some("0.075")
        );
        assert!(c.lookup(ProviderKind::Vertex, "gemini-y").is_some());
    }

    #[test]
    fn rejects_bad_catalogs() {
        assert!(Catalog::parse_aigw("version: x\ncurrency: EUR\nmodels: []\n").is_err());
        assert!(
            Catalog::parse_aigw(
                "version: x\ncurrency: USD\nmodels:\n  - {provider: openai, model: m, perMillionTokens: {input: \"-1\"}}\n"
            )
            .is_err()
        );
        assert!(Catalog::parse_litellm("{}").is_err());
    }
}
