// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Wire-Formate (JSON-Typen der Anbieter-/Client-APIs) und Konverter von und zur [IR](crate::ir).
//!
//! - [`openai`] – Chat Completions (Anfrage, Antwort, Stream-Chunk), Models-Liste, Fehler
//! - [`anthropic`] – Messages (Anfrage, Antwort, SSE-Ereignisse), Models-Liste, Fehler
//! - [`sse`] – Server-Sent-Events: Kodierung und inkrementeller Parser
//! - [`multipart`] – `multipart/form-data` lesen und schreiben (Audio-Uploads, AIGW-21)
//! - [`base64`] – Base64 (Standard-Alphabet) für Embeddings und Bilder
//!
//! Jedes Formatmodul bietet `decode_*` (Bytes → IR) und `encode_*` (IR → Wire-Typ/JSON) sowie
//! einen zustandsbehafteten Stream-Encoder (IR-Ereignisse → SSE) und -Decoder (SSE-Daten →
//! IR-Ereignisse).

pub mod anthropic;
pub mod base64;
pub mod multipart;
pub mod openai;
pub mod sse;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Wire-Format eines Endpunkts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WireFormat {
    /// OpenAI Chat Completions (und kompatible Anbieter).
    OpenAiChat,
    /// Anthropic Messages.
    AnthropicMessages,
    /// AWS Bedrock Converse (nur Zielformat von Anbieter-Adaptern).
    BedrockConverse,
    /// Google Gemini `generateContent` (Gemini API und Vertex AI; nur Zielformat).
    Gemini,
    /// OpenAI Responses API (`/v1/responses`; Eingangsformat, abgebildet auf die Chat-IR).
    OpenAiResponses,
    /// OpenAI Embeddings (`/v1/embeddings`).
    OpenAiEmbeddings,
    /// OpenAI Bilder (`/v1/images/generations`).
    OpenAiImages,
    /// OpenAI Audio (`/v1/audio/transcriptions`, `/translations`, `/speech`).
    OpenAiAudio,
    /// MCP-Tool-Aufruf über das MCP-Gateway (AIGW-41; nur Eingangsformat im Request-Log).
    Mcp,
}

/// Deserialisiert ein JSON-Objekt (bereits als Map gelesen) in einen Wire-Typ.
pub(crate) fn from_map<T: serde::de::DeserializeOwned>(
    map: Map<String, Value>,
) -> Result<T, serde_json::Error> {
    serde_json::from_value(Value::Object(map))
}

/// Liest das Feld `type` als String.
pub(crate) fn type_of(map: &Map<String, Value>) -> Option<&str> {
    map.get("type").and_then(Value::as_str)
}

/// Übernimmt `extra` nur, wenn es ausgegeben werden soll; sonst wird der Verlust notiert.
pub(crate) fn keep(
    extra: Map<String, Value>,
    keep: bool,
    losses: &mut Losses,
    ctx: &str,
) -> Map<String, Value> {
    if keep {
        extra
    } else {
        losses.extra(ctx, &extra);
        Map::new()
    }
}

/// Bei einer Konvertierung verworfene Informationen (für Warnungen im Log).
///
/// Leer, wenn Quell- und Zielformat gleich sind und alles abbildbar war.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Losses(Vec<String>);

impl Losses {
    /// Neue, leere Liste.
    pub fn new() -> Self {
        Self::default()
    }

    /// Notiert einen Verlust.
    pub fn note(&mut self, what: impl Into<String>) {
        self.0.push(what.into());
    }

    /// Notiert verworfene unbekannte Felder (falls vorhanden).
    pub fn extra(&mut self, ctx: &str, extra: &Map<String, Value>) {
        if !extra.is_empty() {
            let keys: Vec<&str> = extra.keys().map(String::as_str).collect();
            self.note(format!(
                "{ctx}: unbekannte Felder verworfen ({})",
                keys.join(", ")
            ));
        }
    }

    /// `true`, wenn nichts verloren ging.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Alle Einträge.
    pub fn items(&self) -> &[String] {
        &self.0
    }
}

/// Zerlegt eine Data-URL `data:<typ>;base64,<daten>` in (Typ, Daten).
pub(crate) fn split_data_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let media = meta.strip_suffix(";base64")?;
    Some((media, data))
}

/// Text oder Liste von Teilen (`"content": "…"` bzw. `"content": [ … ]`).
///
/// Eigener Deserializer statt `#[serde(untagged)]`, damit Fehler in Teilen mit ihrer
/// eigentlichen Ursache gemeldet werden.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum TextOrParts<P> {
    /// Einfacher Text.
    Text(String),
    /// Teile.
    Parts(Vec<P>),
}

impl<'de, P: Deserialize<'de>> Deserialize<'de> for TextOrParts<P> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<P>(std::marker::PhantomData<P>);
        impl<'de, P: Deserialize<'de>> serde::de::Visitor<'de> for V<P> {
            type Value = TextOrParts<P>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("String oder Liste von Inhaltsteilen")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(TextOrParts::Text(v.to_owned()))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(TextOrParts::Text(v))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut parts = Vec::with_capacity(seq.size_hint().unwrap_or(0));
                while let Some(p) = seq.next_element()? {
                    parts.push(p);
                }
                Ok(TextOrParts::Parts(parts))
            }
        }
        d.deserialize_any(V(std::marker::PhantomData))
    }
}

/// Aktuelle Unix-Zeit in Sekunden.
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
