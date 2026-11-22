// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Tokenizer je Modellgruppe und Prompt-Schätzung (F46, AIGW-71).
//!
//! - **Zuordnung:** `settings.tokenizer` der Modellgruppe (`auto` Standard, `o200k_base`,
//!   `cl100k_base`, `heuristic[:claude|:gemini]`, `hf:<datei>`); bei `auto` nach Anbieter und
//!   Modellname des ersten aktiven Deployments ([`aigw_tokenizer::auto`]), ohne Deployment
//!   nach dem Gruppennamen.
//! - **`hf:<datei>`** liegt relativ zu `limits.tokenizer_dir`. Geladen wird beim ersten
//!   Bedarf im Hintergrund (`spawn_blocking`); bis dahin – und bei Ladefehlern (einmal
//!   `warn`) – gilt die automatische Zuordnung.
//! - **Prompt-Schätzung:** Texte, Tool-Aufrufe/-Ergebnisse, Reasoning, Namen sowie Tool-
//!   Definitionen und Antwort-Schema (als JSON) werden gezählt; dazu je Nachricht 3 Tokens und
//!   3 Tokens für die Antworteinleitung (Chat-Format von OpenAI), je Bild 765 Tokens
//!   (`detail: low`: 85).

use std::path::PathBuf;
use std::sync::Arc;

use aigw_core::ir::{ChatRequest, Content, PartKind};
use aigw_core::snapshot::ModelGroupCtx;
use aigw_tokenizer::{HfTokenizer, Profile, Tokenizer, TokenizerSetting};
use dashmap::DashMap;

/// Tokens je Nachricht (Rollen-/Trennzeichen im Chat-Format).
const PER_MESSAGE: u64 = 3;
/// Tokens für die Einleitung der Antwort.
const REPLY_PRIMING: u64 = 3;
/// Bild mit `detail: high`/`auto` (1024×1024, OpenAI-Rechnung) bzw. `low`.
const IMAGE_TOKENS: u64 = 765;
const IMAGE_TOKENS_LOW: u64 = 85;

#[derive(Debug, Clone)]
enum HfState {
    Loading,
    Ready(Tokenizer),
    Failed,
}

/// Tokenizer der Modellgruppen (mit Cache der geladenen `tokenizer.json`).
#[derive(Debug, Default)]
pub(crate) struct Tokenizers {
    dir: Option<PathBuf>,
    hf: Arc<DashMap<String, HfState>>,
}

impl Tokenizers {
    /// `dir` = `limits.tokenizer_dir` (Verzeichnis der `tokenizer.json`-Dateien).
    pub(crate) fn new(dir: Option<PathBuf>) -> Self {
        Self {
            dir,
            hf: Arc::new(DashMap::new()),
        }
    }

    /// Lädt die eingebetteten BPE-Tabellen im Hintergrund vor (erste Anfrage ohne ≈ 20 ms
    /// Ladezeit).
    pub(crate) fn warm_up() {
        let _ = std::thread::Builder::new()
            .name("aigw-tokenizer-warmup".into())
            .spawn(|| {
                for e in aigw_tokenizer::Encoding::ALL {
                    let _ = e.load();
                }
            });
    }

    /// Tokenizer einer Modellgruppe (ohne Gruppe: Heuristik).
    pub(crate) fn for_group(&self, group: Option<&ModelGroupCtx>) -> Tokenizer {
        let Some(g) = group else {
            return Tokenizer::Heuristic(Profile::Generic);
        };
        let setting = match g
            .group
            .spec
            .settings
            .get("tokenizer")
            .and_then(serde_json::Value::as_str)
        {
            Some(s) => TokenizerSetting::parse(s).unwrap_or(TokenizerSetting::Auto),
            None => TokenizerSetting::Auto,
        };
        match setting {
            TokenizerSetting::Auto => auto_for(g),
            TokenizerSetting::Encoding(e) => Tokenizer::Bpe(e),
            TokenizerSetting::Heuristic(p) => Tokenizer::Heuristic(p),
            TokenizerSetting::HfFile(f) => self.hf(&f).unwrap_or_else(|| auto_for(g)),
        }
    }

    fn hf(&self, file: &str) -> Option<Tokenizer> {
        if let Some(s) = self.hf.get(file) {
            return match s.value() {
                HfState::Ready(t) => Some(t.clone()),
                HfState::Loading | HfState::Failed => None,
            };
        }
        let Some(dir) = &self.dir else {
            if self.hf.insert(file.to_owned(), HfState::Failed).is_none() {
                tracing::warn!(
                    file,
                    "settings.tokenizer = hf:… ohne limits.tokenizer_dir – automatische Zuordnung"
                );
            }
            return None;
        };
        // Nur der erste Aufrufer startet das Laden.
        if self.hf.insert(file.to_owned(), HfState::Loading).is_some() {
            return None;
        }
        let path = dir.join(file);
        let name: Arc<str> = Arc::from(file);
        let map = Arc::clone(&self.hf);
        let load = move || {
            let state = match HfTokenizer::from_file(&path) {
                Ok(t) => {
                    tracing::info!(file = %name, "tokenizer.json geladen");
                    HfState::Ready(Tokenizer::Hf(Arc::new(t), Arc::clone(&name)))
                }
                Err(e) => {
                    tracing::warn!(file = %name, error = %e,
                        "tokenizer.json nicht ladbar – automatische Zuordnung");
                    HfState::Failed
                }
            };
            map.insert(name.to_string(), state);
        };
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => drop(rt.spawn_blocking(load)),
            Err(_) => load(),
        }
        // Ohne Laufzeit wurde synchron geladen.
        match self.hf.get(file).map(|s| s.value().clone()) {
            Some(HfState::Ready(t)) => Some(t),
            _ => None,
        }
    }
}

/// Automatische Zuordnung nach dem ersten Deployment der Gruppe.
fn auto_for(g: &ModelGroupCtx) -> Tokenizer {
    match g.deployments.first() {
        Some(d) => aigw_tokenizer::auto(
            Some(d.deployment.spec.provider.as_str()),
            &d.deployment.spec.provider_model,
        ),
        None => aigw_tokenizer::auto(None, &g.group.metadata.name),
    }
}

fn content_tokens(tok: &Tokenizer, c: &Content) -> u64 {
    match c {
        Content::Text(t) => tok.count(t),
        Content::Parts(parts) => parts
            .iter()
            .map(|p| match &p.kind {
                PartKind::Text { text }
                | PartKind::Reasoning { text, .. }
                | PartKind::Refusal { text } => tok.count(text),
                PartKind::ToolCall {
                    name, arguments, ..
                } => tok.count(name) + tok.count(arguments),
                PartKind::ToolResult { content, .. } => content_tokens(tok, content),
                PartKind::Image { detail, .. } => {
                    if detail.as_deref() == Some("low") {
                        IMAGE_TOKENS_LOW
                    } else {
                        IMAGE_TOKENS
                    }
                }
                _ => 0,
            })
            .sum(),
    }
}

/// Geschätzte Eingabe-Tokens einer Chat-Anfrage.
pub(crate) fn request_tokens(tok: &Tokenizer, req: &ChatRequest) -> u64 {
    let mut n = REPLY_PRIMING;
    for m in &req.messages {
        n += PER_MESSAGE + content_tokens(tok, &m.content);
        if let Some(name) = &m.name {
            n += tok.count(name);
        }
    }
    for t in &req.tools {
        if let Ok(json) = serde_json::to_string(&t.kind) {
            n += tok.count(&json);
        }
    }
    if let Some(f) = &req.response_format
        && let Ok(json) = serde_json::to_string(f)
    {
        n += tok.count(&json);
    }
    n
}

/// Tokens eines Ausgabetexts; ist `text` nur ein Ausschnitt (Puffergrenze) von insgesamt
/// `total_bytes`, wird hochgerechnet.
pub(crate) fn output_tokens(tok: &Tokenizer, text: &str, total_bytes: usize) -> u64 {
    let n = tok.count(text);
    if total_bytes > text.len() && !text.is_empty() {
        let scaled = u128::from(n) * total_bytes as u128 / text.len() as u128;
        u64::try_from(scaled).unwrap_or(u64::MAX)
    } else {
        n
    }
}

#[cfg(test)]
mod tests {
    use aigw_core::ir::{Message, Role};
    use aigw_core::resource::{ModelGroupSpec, Object, ObjectMeta, new_id};

    use super::*;

    fn group(settings: serde_json::Value) -> ModelGroupCtx {
        ModelGroupCtx {
            group: Object {
                id: new_id(),
                metadata: ObjectMeta::named("gpt-4o"),
                spec: ModelGroupSpec {
                    settings: settings.as_object().cloned().unwrap_or_default(),
                    ..ModelGroupSpec::default()
                },
            },
            deployments: vec![],
        }
    }

    #[test]
    fn resolution() {
        let t = Tokenizers::new(None);
        assert_eq!(t.for_group(None).to_string(), "heuristic:generic");
        // Ohne Deployment: Gruppenname.
        assert_eq!(
            t.for_group(Some(&group(serde_json::json!({})))).to_string(),
            "o200k_base"
        );
        assert_eq!(
            t.for_group(Some(&group(
                serde_json::json!({"tokenizer": "heuristic:claude"})
            )))
            .to_string(),
            "heuristic:claude"
        );
        // hf ohne Verzeichnis → automatisch.
        assert_eq!(
            t.for_group(Some(&group(serde_json::json!({"tokenizer": "hf:x.json"}))))
                .to_string(),
            "o200k_base"
        );
    }

    #[test]
    fn hf_file_from_dir() {
        let dir = std::env::temp_dir().join(format!("aigw-tok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let json = r#"{"normalizer": null, "pre_tokenizer": {"type": "ByteLevel",
            "add_prefix_space": false, "use_regex": true},
            "model": {"type": "BPE", "vocab": {"a": 0, "b": 1, "ab": 2, "Ġ": 3},
                      "merges": ["a b"]}}"#;
        std::fs::write(dir.join("t.json"), json).unwrap();
        // Ohne Tokio-Laufzeit wird synchron geladen.
        let t = Tokenizers::new(Some(dir.clone()));
        let g = group(serde_json::json!({"tokenizer": "hf:t.json"}));
        let tok = t.for_group(Some(&g));
        assert_eq!(tok.to_string(), "hf:t.json");
        assert_eq!(tok.count("ab ab"), 3);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn request_estimate() {
        let tok = Tokenizer::Bpe(aigw_tokenizer::Encoding::O200kBase);
        let req = ChatRequest {
            model: "m".into(),
            messages: vec![
                Message::text(Role::System, "You are helpful."),
                Message::text(Role::User, "Hello, world!"),
            ],
            ..ChatRequest::default()
        };
        // 3 + 2×3 + 4 („You are helpful.“) + 4 („Hello, world!“)
        assert_eq!(request_tokens(&tok, &req), 17);
        assert_eq!(output_tokens(&tok, "Hello, world!", 26), 8);
    }
}
