// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Kanonische Zwischenform (IR) für Chat-Anfragen und -Antworten.
//!
//! Jede Anfrage wird – unabhängig vom Eingangsformat – vollständig in diese Form übersetzt und
//! für das Ziel neu serialisiert (Architektur §4, „immer IR“). Felder, die die IR nicht kennt,
//! landen in `extra` des jeweiligen Objekts; sie werden nur dann wieder ausgegeben, wenn das
//! Zielformat dem Ursprungsformat ([`ChatRequest::source`] bzw. [`ChatResponse::source`])
//! entspricht.
//!
//! Aufbau:
//! - [`ChatRequest`] – Nachrichten ([`Message`] mit [`Role`] und [`Content`]), Tools,
//!   Sampling, Limits, Antwortformat, Reasoning, Metadaten, Streaming-Optionen
//! - [`ChatResponse`] – eine oder mehrere [`Choice`]s und [`Usage`]
//! - [`StreamEvent`] – inkrementelle Ereignisse einer gestreamten Antwort
//! - [`ApiError`] – Fehler, die im Format des Eingangs an den Client gehen
//!
//! Erweiterbarkeit: Embeddings, Responses-API, Bilder und Audio bekommen später eigene
//! Anfrage-/Antworttypen neben [`ChatRequest`]; [`PartKind`] und [`MediaSource`] sind bereits
//! so angelegt, dass sie dort wiederverwendet werden können.

mod error;
mod stream;

pub use error::{ApiError, ErrorClass, ErrorKind, UpstreamError};
pub use stream::{StreamEvent, ToolCallStart};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::wire::WireFormat;

/// Unbekannte, formatspezifische Felder eines Objekts (Reihenfolge egal).
pub type Extra = Map<String, Value>;

/// Kanonische Chat-Anfrage.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    /// Angefragtes Modell (öffentlicher Name/Alias).
    pub model: String,
    /// Konversation in Reihenfolge. System-/Developer-Anweisungen sind Nachrichten mit
    /// [`Role::System`] bzw. [`Role::Developer`] (bei Anthropic das Feld `system`).
    pub messages: Vec<Message>,
    /// Verfügbare Tools.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    /// Steuerung der Tool-Nutzung.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// Parallele Tool-Aufrufe erlaubt (OpenAI `parallel_tool_calls`,
    /// Anthropic `tool_choice.disable_parallel_tool_use` invertiert).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// Sampling-Parameter.
    #[serde(default)]
    pub sampling: Sampling,
    /// Obergrenze der Ausgabe-Tokens (OpenAI `max_completion_tokens`/`max_tokens`,
    /// Anthropic `max_tokens`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// Stoppsequenzen.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    /// Gewünschtes Antwortformat (JSON-Modus, JSON-Schema).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    /// Reasoning-/Thinking-Einstellungen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
    /// Endnutzer-Kennung (OpenAI `user`, Anthropic `metadata.user_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Weitere Metadaten (OpenAI `metadata`, Anthropic `metadata` ohne `user_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Extra>,
    /// Antwort als Stream (`None` = Feld fehlte, gilt als `false`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Streaming-Optionen (OpenAI `stream_options`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    /// Hinweise für die verlustfreie Rückgabe ins Ursprungsformat.
    #[serde(default)]
    pub hints: RequestHints,
    /// Unbekannte Felder der obersten Ebene.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Extra,
    /// Format, aus dem die Anfrage stammt (steuert die Ausgabe von `extra`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<WireFormat>,
}

impl ChatRequest {
    /// `true`, wenn eine gestreamte Antwort angefordert ist.
    pub fn is_stream(&self) -> bool {
        self.stream.unwrap_or(false)
    }

    /// `true`, wenn `extra` beim Serialisieren nach `target` ausgegeben werden soll.
    pub fn keeps_extra_for(&self, target: WireFormat) -> bool {
        self.source == Some(target)
    }

    /// Text der letzten Nutzernachricht (für Mock-Anbieter, Cache-Schlüssel usw.).
    pub fn last_user_text(&self) -> Option<String> {
        self.messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| m.content.text())
    }
}

/// Formatdetails, die nur der verlustfreien Rückgabe ins Ursprungsformat dienen.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestHints {
    /// OpenAI: Anfrage nutzte das veraltete `max_tokens` statt `max_completion_tokens`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub legacy_max_tokens: bool,
    /// OpenAI: `stop` war ein einzelner String statt einer Liste.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stop_as_string: bool,
}

/// Rolle einer Nachricht.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Systemanweisung.
    System,
    /// Developer-Anweisung (OpenAI; Anthropic: wie System).
    Developer,
    /// Nutzer.
    User,
    /// Modell.
    Assistant,
    /// Tool-Ergebnis (OpenAI `role: tool`; bei Anthropic Teil einer Nutzernachricht).
    Tool,
}

/// Eine Nachricht der Konversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Rolle.
    pub role: Role,
    /// Inhalt.
    pub content: Content,
    /// Optionaler Name des Teilnehmers (OpenAI `name`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Unbekannte Felder der Nachricht.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Extra,
}

impl Message {
    /// Einfache Textnachricht.
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            content: Content::Text(text.into()),
            name: None,
            extra: Extra::new(),
        }
    }
}

/// Inhalt einer Nachricht: einfacher Text oder Liste von Teilen.
///
/// Die Unterscheidung bleibt erhalten, damit `"content": "…"` und `"content": [{…}]` beim
/// Rückweg ins gleiche Format unverändert bleiben.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Content {
    /// Reiner Text.
    Text(String),
    /// Strukturierte Teile.
    Parts(Vec<Part>),
}

impl Default for Content {
    fn default() -> Self {
        Self::Parts(Vec::new())
    }
}

impl Content {
    /// Verkettet alle Textteile (ohne Reasoning).
    pub fn text(&self) -> String {
        match self {
            Self::Text(t) => t.clone(),
            Self::Parts(parts) => {
                let mut out = String::new();
                for p in parts {
                    if let PartKind::Text { text } = &p.kind {
                        out.push_str(text);
                    }
                }
                out
            }
        }
    }

    /// Wandelt in eine Teile-Liste um (Text wird zu einem Textteil).
    pub fn into_parts(self) -> Vec<Part> {
        match self {
            Self::Text(t) => vec![Part::new(PartKind::Text { text: t })],
            Self::Parts(p) => p,
        }
    }

    /// `true`, wenn kein Inhalt vorhanden ist.
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Text(t) => t.is_empty(),
            Self::Parts(p) => p.is_empty(),
        }
    }
}

/// Ein Inhaltsteil mit gemeinsamen Zusatzfeldern.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Part {
    /// Art und Nutzdaten.
    pub kind: PartKind,
    /// Anthropic `cache_control` (wird an Anthropic-Ziele durchgereicht).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<Value>,
    /// Unbekannte Felder des Teils.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Extra,
}

impl Part {
    /// Teil ohne Zusatzfelder.
    pub fn new(kind: PartKind) -> Self {
        Self {
            kind,
            cache_control: None,
            extra: Extra::new(),
        }
    }

    /// Textteil.
    pub fn text(text: impl Into<String>) -> Self {
        Self::new(PartKind::Text { text: text.into() })
    }
}

/// Art eines Inhaltsteils.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PartKind {
    /// Text.
    Text {
        /// Inhalt.
        text: String,
    },
    /// Bild.
    Image {
        /// Quelle.
        source: MediaSource,
        /// OpenAI `detail` (`auto`/`low`/`high`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// Audio-Eingabe (OpenAI `input_audio`).
    Audio {
        /// Base64-Daten.
        data: String,
        /// Format (`wav`, `mp3` …).
        format: String,
    },
    /// Datei/Dokument (OpenAI `file`, Anthropic `document`).
    File {
        /// Quelle.
        source: MediaSource,
        /// Dateiname bzw. Titel.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
    },
    /// Tool-Aufruf des Modells.
    ToolCall {
        /// Aufruf-ID.
        id: String,
        /// Tool-Name.
        name: String,
        /// Argumente als JSON-Text (OpenAI liefert einen String, Anthropic ein Objekt).
        arguments: String,
    },
    /// Ergebnis eines Tool-Aufrufs.
    ToolResult {
        /// ID des zugehörigen Aufrufs.
        tool_call_id: String,
        /// Inhalt.
        content: Content,
        /// Fehlerkennzeichen (Anthropic `is_error`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    /// Reasoning/Thinking des Modells.
    Reasoning {
        /// Text (bei Anthropic ggf. leer oder Zusammenfassung).
        text: String,
        /// Signatur (Anthropic), muss unverändert zurückgegeben werden.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// Verschlüsseltes Reasoning (Anthropic `redacted_thinking`).
    RedactedReasoning {
        /// Undurchsichtige Daten.
        data: String,
    },
    /// Ablehnung durch das Modell (OpenAI `refusal`).
    Refusal {
        /// Begründungstext.
        text: String,
    },
    /// Teil, den die IR nicht kennt; wird nur ins Ursprungsformat zurückgegeben.
    Opaque {
        /// Ursprungsformat.
        format: WireFormat,
        /// Unveränderter Rohwert.
        value: Value,
    },
}

/// Quelle von Bild-/Datei-Daten.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MediaSource {
    /// URL (http/https).
    Url {
        /// Adresse.
        url: String,
    },
    /// Base64-kodierte Daten.
    Base64 {
        /// MIME-Typ, falls bekannt.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        media_type: Option<String>,
        /// Daten.
        data: String,
    },
    /// Klartext-Dokument (Anthropic `text`-Quelle).
    Text {
        /// MIME-Typ.
        media_type: String,
        /// Text.
        data: String,
    },
    /// Verweis auf eine hochgeladene Datei des Anbieters.
    FileId {
        /// Datei-ID.
        file_id: String,
    },
    /// Unbekannte Quelle (Rohwert, nur Ursprungsformat).
    Other {
        /// Rohwert.
        value: Value,
    },
}

/// Tool-Definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    /// Art und Nutzdaten.
    pub kind: ToolKind,
    /// Anthropic `cache_control`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<Value>,
    /// Unbekannte Felder der Tool-Hülle.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Extra,
}

/// Art eines Tools.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolKind {
    /// Funktion mit JSON-Schema-Parametern.
    Function(FunctionTool),
    /// Anbieterspezifisches Tool (z. B. Anthropic-Server-Tools), nur Ursprungsformat.
    Opaque {
        /// Ursprungsformat.
        format: WireFormat,
        /// Rohwert.
        value: Value,
    },
}

/// Funktions-Tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionTool {
    /// Name.
    pub name: String,
    /// Beschreibung.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON-Schema der Parameter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
    /// Strikte Schema-Einhaltung.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
    /// Unbekannte Felder der Funktionsdefinition (OpenAI `function`-Objekt).
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Extra,
}

/// Steuerung der Tool-Nutzung.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    /// Modell entscheidet.
    Auto,
    /// Keine Tools.
    None,
    /// Mindestens ein Tool (OpenAI `required`, Anthropic `any`).
    Required,
    /// Bestimmtes Tool.
    Tool {
        /// Tool-Name.
        name: String,
    },
}

/// Sampling-Parameter.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Sampling {
    /// Temperatur.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus-Sampling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Top-k (Anthropic und manche OpenAI-kompatible Anbieter).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u64>,
    /// Frequency-Penalty (OpenAI).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    /// Presence-Penalty (OpenAI).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    /// Seed (OpenAI).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
}

/// Gewünschtes Antwortformat.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    /// Freitext.
    Text,
    /// Beliebiges JSON-Objekt (OpenAI `json_object`).
    JsonObject,
    /// JSON nach Schema.
    JsonSchema {
        /// Schemaname (OpenAI Pflicht, Anthropic ohne).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Beschreibung.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// Schema.
        schema: Value,
        /// Strikt.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
    },
}

/// Reasoning-/Thinking-Einstellungen.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Reasoning {
    /// Aufwand (`minimal`, `low`, `medium`, `high`, `xhigh`, `max` …; OpenAI
    /// `reasoning_effort`, Anthropic `output_config.effort`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Anthropic `thinking`-Konfiguration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Thinking>,
}

/// Anthropic-Thinking-Konfiguration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Thinking {
    /// Modus (`adaptive`, `enabled`, `disabled`, `between_tools` …).
    pub mode: String,
    /// Token-Budget (nur ältere Modelle, Modus `enabled`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u64>,
    /// Anzeige (`summarized`, `omitted` …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    /// Unbekannte Felder.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Extra,
}

/// Streaming-Optionen.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StreamOptions {
    /// Usage am Ende des Streams senden (OpenAI).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_usage: Option<bool>,
    /// Unbekannte Felder.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Extra,
}

/// Kanonische Chat-Antwort (nicht gestreamt).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatResponse {
    /// Antwort-ID.
    pub id: String,
    /// Tatsächlich verwendetes Modell.
    pub model: String,
    /// Erstellungszeit (Unix-Sekunden, OpenAI `created`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<i64>,
    /// Antwortvarianten (Anthropic: genau eine).
    pub choices: Vec<Choice>,
    /// Token-Verbrauch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Unbekannte Felder der obersten Ebene.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Extra,
    /// Ursprungsformat (steuert die Ausgabe von `extra`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<WireFormat>,
}

/// Eine Antwortvariante.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    /// Index (OpenAI `n` > 1).
    pub index: u32,
    /// Antwortnachricht (Rolle `assistant`).
    pub message: Message,
    /// Grund für das Ende.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    /// Ausgelöste Stoppsequenz (Anthropic).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
    /// Unbekannte Felder der Variante (z. B. OpenAI `logprobs`).
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Extra,
}

/// Grund für das Ende der Generierung.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// Natürliches Ende (OpenAI `stop`, Anthropic `end_turn`).
    EndTurn,
    /// Token-Grenze erreicht (OpenAI `length`).
    MaxTokens,
    /// Stoppsequenz getroffen.
    StopSequence,
    /// Tool-Aufruf (OpenAI `tool_calls`, Anthropic `tool_use`).
    ToolUse,
    /// Inhaltsfilter (OpenAI `content_filter`).
    ContentFilter,
    /// Ablehnung (Anthropic `refusal`).
    Refusal,
    /// Pausiert (Anthropic `pause_turn`).
    PauseTurn,
    /// Sonstiger, formatspezifischer Grund.
    Other(String),
}

/// Token-Verbrauch.
///
/// `input_tokens` zählt **alle** Eingabe-Tokens einschließlich gecachter (OpenAI-Semantik);
/// bei Anthropic wird beim Ein-/Auslesen entsprechend addiert bzw. subtrahiert.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Eingabe-Tokens gesamt.
    pub input_tokens: u64,
    /// Ausgabe-Tokens gesamt (inkl. Reasoning).
    pub output_tokens: u64,
    /// Aus dem Cache gelesene Eingabe-Tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    /// In den Cache geschriebene Eingabe-Tokens (Anthropic).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    /// Davon Reasoning-Tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    /// Unbekannte, formatspezifische Felder.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Extra,
}

impl Usage {
    /// Summe aus Ein- und Ausgabe.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// Modellinformation für `GET /v1/models`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Öffentlicher Modellname.
    pub id: String,
    /// Anzeigename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Erstellungszeit (Unix-Sekunden).
    pub created: i64,
    /// Eigentümer (OpenAI `owned_by`).
    pub owned_by: String,
}
