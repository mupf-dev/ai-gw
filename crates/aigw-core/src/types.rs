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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ProviderKind {
    /// OpenAI.
    OpenAi,
    /// Anthropic.
    Anthropic,
    /// Azure OpenAI.
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
    OpenAiCompatible,
}
