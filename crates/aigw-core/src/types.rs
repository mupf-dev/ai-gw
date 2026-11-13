// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Gemeinsame Basistypen (Platzhalter, werden mit dem Architekturkonzept ausgebaut).

use std::fmt;

use serde::{Deserialize, Serialize};

/// Öffentlicher Modellname, wie ihn Clients anfragen (z. B. `gpt-4o`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelName(pub String);

impl fmt::Display for ModelName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Art eines Upstream-Anbieters (MVP-Umfang laut E5).
///
/// Serialisiert als `openai`, `anthropic`, `azure-openai`, `bedrock`, `vertex`, `mistral`,
/// `ollama`, `vllm`, `openai-compatible`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ProviderKind {
    /// OpenAI.
    #[serde(rename = "openai")]
    OpenAi,
    /// Anthropic.
    Anthropic,
    /// Azure OpenAI.
    #[serde(rename = "azure-openai")]
    AzureOpenAi,
    /// AWS Bedrock.
    Bedrock,
    /// Google Vertex AI / Gemini.
    Vertex,
    /// Mistral.
    Mistral,
    /// Ollama.
    Ollama,
    /// vLLM.
    Vllm,
    /// Generischer OpenAI-kompatibler Endpunkt.
    #[serde(rename = "openai-compatible")]
    OpenAiCompatible,
}

impl ProviderKind {
    /// Alle Anbieter.
    pub const ALL: [Self; 9] = [
        Self::OpenAi,
        Self::Anthropic,
        Self::AzureOpenAi,
        Self::Bedrock,
        Self::Vertex,
        Self::Mistral,
        Self::Ollama,
        Self::Vllm,
        Self::OpenAiCompatible,
    ];

    /// Bezeichner wie in YAML und Datenbank.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::AzureOpenAi => "azure-openai",
            Self::Bedrock => "bedrock",
            Self::Vertex => "vertex",
            Self::Mistral => "mistral",
            Self::Ollama => "ollama",
            Self::Vllm => "vllm",
            Self::OpenAiCompatible => "openai-compatible",
        }
    }

    /// Gegenstück zu [`ProviderKind::as_str`].
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_kind_names_match_serde() {
        for k in ProviderKind::ALL {
            let json = serde_json::to_string(&k).unwrap();
            assert_eq!(json, format!("\"{}\"", k.as_str()));
            assert_eq!(ProviderKind::parse(k.as_str()), Some(k));
        }
    }
}
