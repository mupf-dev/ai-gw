// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Anthropic Messages: Wire-Typen und Konverter.
//!
//! Inhaltsblöcke und Tool-Definitionen sind als JSON-Objekte modelliert und werden in
//! [`convert`] anhand ihres `type` in die IR übersetzt (`text`, `image`, `document`,
//! `tool_use`, `tool_result`, `thinking`, `redacted_thinking`); alle anderen Blöcke (Server-
//! Tools, Suchergebnisse …) bleiben als [`PartKind::Opaque`](crate::ir::PartKind::Opaque)
//! erhalten. `cache_control` wird je Block, je Tool und (als unbekanntes Feld) auf oberster
//! Ebene durchgereicht.

mod convert;
pub mod stream;

pub use convert::{
    DEFAULT_MAX_TOKENS, decode_error, decode_request, decode_response, encode_error, encode_models,
    encode_request, encode_response, error_status, error_type, request_from_wire, request_to_wire,
    response_from_wire, response_to_wire,
};
pub use stream::{StreamDecoder, StreamEncoder};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::TextOrParts;

/// Anfrage `POST /v1/messages`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessagesRequest {
    /// Modell.
    pub model: String,
    /// Nachrichten (`user`, `assistant`, `system` mitten in der Konversation).
    pub messages: Vec<AnthropicMessage>,
    /// Obergrenze der Ausgabe-Tokens (Pflichtfeld; als `Option`, um eine eigene
    /// Fehlermeldung liefern zu können).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// Systemprompt (String oder Textblöcke).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<TextOrParts<Map<String, Value>>>,
    /// Tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Map<String, Value>>>,
    /// Tool-Steuerung (`{"type":"auto"|"any"|"tool"|"none", …}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Map<String, Value>>,
    /// Temperatur.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Top-p.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Top-k.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u64>,
    /// Stoppsequenzen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    /// Thinking-Konfiguration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Map<String, Value>>,
    /// Ausgabe-Konfiguration (`effort`, `format` …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_config: Option<Map<String, Value>>,
    /// Metadaten (`user_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
    /// Streaming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Unbekannte Felder (`cache_control`, `service_tier`, `container`, `mcp_servers` …).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Nachricht.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnthropicMessage {
    /// Rolle.
    pub role: String,
    /// Inhalt.
    pub content: TextOrParts<Map<String, Value>>,
    /// Unbekannte Felder.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Antwort (`type: message`), auch Nutzlast von `message_start`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessagesResponse {
    /// ID.
    pub id: String,
    /// `message`.
    #[serde(rename = "type", default = "type_message")]
    pub kind: String,
    /// `assistant`.
    #[serde(default = "role_assistant")]
    pub role: String,
    /// Modell.
    pub model: String,
    /// Inhaltsblöcke.
    #[serde(default)]
    pub content: Vec<Map<String, Value>>,
    /// Grund für das Ende (immer ausgegeben, ggf. `null`).
    #[serde(default)]
    pub stop_reason: Option<String>,
    /// Stoppsequenz (immer ausgegeben, ggf. `null`).
    #[serde(default)]
    pub stop_sequence: Option<String>,
    /// Verbrauch.
    #[serde(default)]
    pub usage: Usage,
    /// Unbekannte Felder (`stop_details`, `container` …).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn type_message() -> String {
    "message".to_owned()
}

fn role_assistant() -> String {
    "assistant".to_owned()
}

/// Token-Verbrauch. `input_tokens` **ohne** Cache-Lesen/-Schreiben.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Nicht gecachte Eingabe-Tokens.
    #[serde(default)]
    pub input_tokens: u64,
    /// Ausgabe-Tokens.
    #[serde(default)]
    pub output_tokens: u64,
    /// In den Cache geschriebene Tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    /// Aus dem Cache gelesene Tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    /// Unbekannte Felder (`cache_creation`, `server_tool_use`, `service_tier` …).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// SSE-Ereignis (Feld `type` = Ereignisname).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Beginn; enthält die Nachricht ohne Inhalt.
    MessageStart {
        /// Nachricht.
        message: MessagesResponse,
    },
    /// Beginn eines Inhaltsblocks.
    ContentBlockStart {
        /// Blockindex.
        index: u32,
        /// Block (ohne Inhalt).
        content_block: Map<String, Value>,
    },
    /// Änderung eines Inhaltsblocks.
    ContentBlockDelta {
        /// Blockindex.
        index: u32,
        /// `text_delta`, `thinking_delta`, `signature_delta`, `input_json_delta` …
        delta: Map<String, Value>,
    },
    /// Ende eines Inhaltsblocks.
    ContentBlockStop {
        /// Blockindex.
        index: u32,
    },
    /// Ende der Nachricht (Grund und Usage).
    MessageDelta {
        /// Grund.
        delta: MessageDelta,
        /// Kumulative Usage.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    /// Stream-Ende.
    MessageStop,
    /// Keep-alive.
    Ping,
    /// Fehler.
    Error {
        /// Fehlerobjekt.
        error: ErrorObject,
    },
}

impl Event {
    /// SSE-Ereignisname.
    pub fn name(&self) -> &'static str {
        match self {
            Self::MessageStart { .. } => "message_start",
            Self::ContentBlockStart { .. } => "content_block_start",
            Self::ContentBlockDelta { .. } => "content_block_delta",
            Self::ContentBlockStop { .. } => "content_block_stop",
            Self::MessageDelta { .. } => "message_delta",
            Self::MessageStop => "message_stop",
            Self::Ping => "ping",
            Self::Error { .. } => "error",
        }
    }
}

/// Nutzlast von `message_delta`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MessageDelta {
    /// Grund.
    #[serde(default)]
    pub stop_reason: Option<String>,
    /// Stoppsequenz.
    #[serde(default)]
    pub stop_sequence: Option<String>,
}

/// `GET /v1/models` (Anthropic-Format).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelList {
    /// Modelle.
    pub data: Vec<Model>,
    /// Weitere Seiten.
    pub has_more: bool,
    /// Erste ID.
    pub first_id: Option<String>,
    /// Letzte ID.
    pub last_id: Option<String>,
}

/// Modell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Model {
    /// `model`.
    #[serde(rename = "type")]
    pub kind: String,
    /// ID.
    pub id: String,
    /// Anzeigename.
    pub display_name: String,
    /// RFC 3339.
    pub created_at: String,
}

/// Fehlerantwort `{"type":"error","error":{…},"request_id":…}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    /// `error`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Fehler.
    pub error: ErrorObject,
    /// Request-ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

/// Fehlerobjekt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorObject {
    /// Typ (`invalid_request_error` …).
    #[serde(rename = "type")]
    pub kind: String,
    /// Meldung.
    pub message: String,
}
