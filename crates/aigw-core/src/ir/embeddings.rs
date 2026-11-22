// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Embeddings (`POST /v1/embeddings`, AIGW-21).
//!
//! Die Vektoren liegen in der IR immer als `f32` vor; `encoding_format: base64` (OpenAI:
//! Little-Endian-`f32`, Base64) ist nur eine Frage der Ausgabe an den Client bzw. der
//! Anfrage an den Anbieter.

use serde::{Deserialize, Serialize};
use serde_json::Map;

use super::{Extra, Usage};
use crate::wire::WireFormat;

/// Eingabe einer Embedding-Anfrage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingInput {
    /// Texte (OpenAI: String oder Liste von Strings).
    Text(Vec<String>),
    /// Bereits tokenisierte Eingaben (OpenAI: Liste von Token-IDs bzw. Liste solcher Listen).
    Tokens(Vec<Vec<u32>>),
}

impl EmbeddingInput {
    /// Anzahl der Eingaben.
    pub fn len(&self) -> usize {
        match self {
            Self::Text(t) => t.len(),
            Self::Tokens(t) => t.len(),
        }
    }

    /// `true`, wenn keine Eingabe vorhanden ist.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Umfang in Bytes (Text) bzw. Anzahl Tokens × 4 (für Schätzungen).
    pub fn byte_len(&self) -> usize {
        match self {
            Self::Text(t) => t.iter().map(String::len).sum(),
            Self::Tokens(t) => t.iter().map(|v| v.len() * 4).sum(),
        }
    }
}

/// Ausgabeformat der Vektoren.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EncodingFormat {
    /// JSON-Zahlen.
    #[default]
    Float,
    /// Base64 der Little-Endian-`f32`-Werte.
    Base64,
}

/// Kanonische Embedding-Anfrage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingsRequest {
    /// Angefragtes Modell (öffentlicher Name/Alias).
    pub model: String,
    /// Eingaben.
    pub input: EmbeddingInput,
    /// `input` war ein einzelner String bzw. eine einzelne Token-Liste (nur für die Rückgabe
    /// ins Ursprungsformat).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub single_input: bool,
    /// Gewünschtes Ausgabeformat an den Client (`None` = Standard `float`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding_format: Option<EncodingFormat>,
    /// Gewünschte Dimension (Matryoshka-Modelle).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<u32>,
    /// Endnutzer-Kennung.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Unbekannte Felder (z. B. `input_type` für Cohere/kompatible Server).
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Extra,
    /// Ursprungsformat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<WireFormat>,
}

impl EmbeddingsRequest {
    /// `true`, wenn `extra` beim Serialisieren nach `target` ausgegeben werden soll.
    pub fn keeps_extra_for(&self, target: WireFormat) -> bool {
        self.source == Some(target)
    }
}

/// Ein Vektor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Embedding {
    /// Position der Eingabe.
    pub index: u32,
    /// Werte.
    pub vector: Vec<f32>,
}

/// Kanonische Embedding-Antwort.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingsResponse {
    /// Modell (wie vom Anbieter gemeldet).
    pub model: String,
    /// Vektoren in Reihenfolge der Eingaben.
    pub data: Vec<Embedding>,
    /// Verbrauch (`input_tokens`; `output_tokens` = 0).
    pub usage: Usage,
    /// `true`, wenn die Usage geschätzt ist (Anbieter meldet keine Tokens).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub usage_estimated: bool,
    /// Unbekannte Felder.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Extra,
}

impl EmbeddingsResponse {
    /// Hängt eine Teilantwort an (Anbieter, die je Aufruf nur eine Eingabe annehmen):
    /// Indizes werden um `offset` verschoben, Usage addiert.
    pub fn merge(&mut self, other: Self, offset: u32) {
        self.data.extend(other.data.into_iter().map(|mut e| {
            e.index += offset;
            e
        }));
        self.usage.input_tokens = self
            .usage
            .input_tokens
            .saturating_add(other.usage.input_tokens);
        self.usage_estimated |= other.usage_estimated;
    }
}
