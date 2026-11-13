// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! OpenAI Chat Completions: Wire-Typen und Konverter; dazu die weiteren OpenAI-Endpunkte
//! (AIGW-21): [`embeddings`], [`images`], [`audio`] und die Responses API ([`responses`],
//! abgebildet auf die Chat-IR).
//!
//! Inhaltsteile (`content`-Listen) und Tool-Definitionen sind als JSON-Objekte modelliert
//! und werden in [`convert`] anhand ihres `type` direkt in die IR übersetzt; unbekannte
//! Typen bleiben als [`PartKind::Opaque`](crate::ir::PartKind::Opaque) erhalten.

pub mod audio;
mod convert;
pub mod embeddings;
pub mod images;
pub mod responses;
pub mod stream;

pub use convert::{
    decode_error, decode_request, decode_response, encode_error, encode_models, encode_request,
    encode_response, error_type, request_from_wire, request_to_wire, response_from_wire,
    response_to_wire,
};
pub use stream::{StreamDecoder, StreamEncoder};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::TextOrParts;

/// Anfrage `POST /v1/chat/completions`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatCompletionRequest {
    /// Modell.
    pub model: String,
    /// Nachrichten.
    pub messages: Vec<ChatMessage>,
    /// Tools (`{"type":"function","function":{…}}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Map<String, Value>>>,
    /// `"auto"`, `"none"`, `"required"` oder `{"type":"function","function":{"name":…}}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    /// Parallele Tool-Aufrufe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// Temperatur.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Top-p.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Frequency-Penalty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    /// Presence-Penalty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    /// Seed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    /// Veraltet, aber verbreitet (kompatible Anbieter).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// Obergrenze der Ausgabe-Tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u64>,
    /// Stoppsequenz(en).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<Stop>,
    /// Antwortformat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<Value>,
    /// Reasoning-Aufwand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Endnutzer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Metadaten.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
    /// Streaming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Streaming-Optionen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<Map<String, Value>>,
    /// Unbekannte Felder (`n`, `logprobs`, `logit_bias`, `service_tier`, `store` …).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `stop`: String oder Liste.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Stop {
    /// Einzelner String.
    One(String),
    /// Liste.
    Many(Vec<String>),
}

/// Nachricht einer Anfrage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// `system`, `developer`, `user`, `assistant`, `tool`.
    pub role: String,
    /// Inhalt (bei `assistant` mit Tool-Aufrufen ggf. `null`).
    #[serde(default)]
    pub content: Option<TextOrParts<Map<String, Value>>>,
    /// Teilnehmername.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Tool-Aufrufe (nur `assistant`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Bezug (nur `tool`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Ablehnung (nur `assistant`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    /// Unbekannte Felder.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Tool-Aufruf.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Aufruf-ID.
    pub id: String,
    /// Immer `function`.
    #[serde(rename = "type", default = "function_type")]
    pub kind: String,
    /// Funktion.
    pub function: FunctionCall,
    /// Unbekannte Felder.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn function_type() -> String {
    "function".to_owned()
}

/// Funktionsaufruf.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionCall {
    /// Name.
    pub name: String,
    /// Argumente als JSON-Text.
    #[serde(default)]
    pub arguments: String,
}

/// Antwort `chat.completion`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatCompletion {
    /// ID.
    pub id: String,
    /// `chat.completion`.
    #[serde(default = "object_completion")]
    pub object: String,
    /// Unix-Sekunden.
    #[serde(default)]
    pub created: i64,
    /// Modell.
    pub model: String,
    /// Varianten.
    pub choices: Vec<CompletionChoice>,
    /// Verbrauch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Unbekannte Felder (`system_fingerprint`, `service_tier` …).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn object_completion() -> String {
    "chat.completion".to_owned()
}

/// Variante einer Antwort.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionChoice {
    /// Index.
    #[serde(default)]
    pub index: u32,
    /// Nachricht.
    pub message: ResponseMessage,
    /// `stop`, `length`, `tool_calls`, `content_filter` (immer ausgegeben, ggf. `null`).
    #[serde(default)]
    pub finish_reason: Option<String>,
    /// Unbekannte Felder (`logprobs` …).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Nachricht einer Antwort.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseMessage {
    /// `assistant`.
    #[serde(default = "role_assistant")]
    pub role: String,
    /// Text (oder `null`).
    #[serde(default)]
    pub content: Option<String>,
    /// Ablehnung (oder `null`).
    #[serde(default)]
    pub refusal: Option<String>,
    /// Tool-Aufrufe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Unbekannte Felder (`annotations`, `audio` …).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn role_assistant() -> String {
    "assistant".to_owned()
}

/// Token-Verbrauch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Eingabe (inkl. gecachter Tokens).
    #[serde(default)]
    pub prompt_tokens: u64,
    /// Ausgabe (inkl. Reasoning).
    #[serde(default)]
    pub completion_tokens: u64,
    /// Summe.
    #[serde(default)]
    pub total_tokens: u64,
    /// Details zur Eingabe (`cached_tokens`, `audio_tokens`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<Map<String, Value>>,
    /// Details zur Ausgabe (`reasoning_tokens` …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens_details: Option<Map<String, Value>>,
    /// Unbekannte Felder.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Stream-Chunk `chat.completion.chunk`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatCompletionChunk {
    /// ID (in allen Chunks gleich; manche kompatiblen Anbieter lassen sie weg).
    #[serde(default)]
    pub id: String,
    /// `chat.completion.chunk`.
    #[serde(default = "object_chunk")]
    pub object: String,
    /// Unix-Sekunden.
    #[serde(default)]
    pub created: i64,
    /// Modell.
    #[serde(default)]
    pub model: String,
    /// Varianten (im Usage-Chunk leer).
    #[serde(default)]
    pub choices: Vec<ChunkChoice>,
    /// Verbrauch (nur letzter Chunk bei `include_usage`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Unbekannte Felder.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn object_chunk() -> String {
    "chat.completion.chunk".to_owned()
}

/// Variante eines Chunks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChunkChoice {
    /// Index.
    #[serde(default)]
    pub index: u32,
    /// Änderung.
    #[serde(default)]
    pub delta: ChunkDelta,
    /// Ende (sonst `null`).
    #[serde(default)]
    pub finish_reason: Option<String>,
    /// Unbekannte Felder.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Änderung in einem Chunk.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChunkDelta {
    /// Rolle (nur erster Chunk).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Textstück.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Ablehnungsstück.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    /// Tool-Aufruf-Stücke.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallChunk>>,
    /// Unbekannte Felder (`reasoning_content` kompatibler Anbieter …).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Tool-Aufruf-Stück.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallChunk {
    /// Index des Aufrufs.
    #[serde(default)]
    pub index: u32,
    /// ID (nur im ersten Stück).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// `function` (nur im ersten Stück).
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Funktion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<FunctionCallChunk>,
}

/// Funktionsaufruf-Stück.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionCallChunk {
    /// Name (nur im ersten Stück).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Argument-Fragment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

/// `GET /v1/models`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelList {
    /// `list`.
    pub object: String,
    /// Modelle.
    pub data: Vec<Model>,
}

/// Modell in der Liste.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Model {
    /// ID.
    pub id: String,
    /// `model`.
    pub object: String,
    /// Unix-Sekunden.
    pub created: i64,
    /// Eigentümer.
    pub owned_by: String,
}

/// Fehlerantwort `{"error":{…}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    /// Fehler.
    pub error: ErrorObject,
}

/// Fehlerobjekt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorObject {
    /// Meldung.
    pub message: String,
    /// Typ (`invalid_request_error` …).
    #[serde(rename = "type")]
    pub kind: String,
    /// Parameter (oder `null`).
    #[serde(default)]
    pub param: Option<String>,
    /// Code (oder `null`).
    #[serde(default)]
    pub code: Option<String>,
}
