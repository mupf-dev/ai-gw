// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Audio (AIGW-21): Transkription/Übersetzung (`/v1/audio/transcriptions`,
//! `/v1/audio/translations`, `multipart/form-data`) und Sprachausgabe (`/v1/audio/speech`).
//!
//! Die Audiodaten werden als `Bytes` (Ausschnitt des Request-Bodys, ohne Kopie) geführt. Die
//! Antwort der Sprachausgabe ist ein Byte-Strom und wird ohne Puffern an den Client
//! weitergereicht.

use bytes::Bytes;
use serde_json::{Map, Value};

use super::{Extra, Usage};
use crate::wire::WireFormat;

/// Transkription (Sprache → Text in der Ausgangssprache) oder Übersetzung (→ Englisch).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioTask {
    /// `/v1/audio/transcriptions`.
    Transcribe,
    /// `/v1/audio/translations`.
    Translate,
}

impl AudioTask {
    /// Bezeichner (`transcribe`/`translate`, wie OpenAI `task`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Transcribe => "transcribe",
            Self::Translate => "translate",
        }
    }
}

/// Ausgabeformat einer Transkription.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum TranscriptFormat {
    /// `{"text": …}` (Standard).
    #[default]
    Json,
    /// Nur Text (`text/plain`).
    Text,
    /// Untertitel SRT.
    Srt,
    /// Untertitel WebVTT.
    Vtt,
    /// Ausführlich (Sprache, Dauer, Segmente, Wörter).
    VerboseJson,
    /// Anderes Format (z. B. `diarized_json`), JSON-Antwort wird unverändert übernommen.
    Other(String),
}

impl TranscriptFormat {
    /// Aus dem Formularwert.
    pub fn parse(s: &str) -> Self {
        match s {
            "json" => Self::Json,
            "text" => Self::Text,
            "srt" => Self::Srt,
            "vtt" => Self::Vtt,
            "verbose_json" => Self::VerboseJson,
            other => Self::Other(other.to_owned()),
        }
    }

    /// Formularwert.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Json => "json",
            Self::Text => "text",
            Self::Srt => "srt",
            Self::Vtt => "vtt",
            Self::VerboseJson => "verbose_json",
            Self::Other(s) => s,
        }
    }

    /// `true`, wenn die Antwort Text statt JSON ist.
    pub fn is_plain_text(&self) -> bool {
        matches!(self, Self::Text | Self::Srt | Self::Vtt)
    }

    /// `Content-Type` der Antwort an den Client.
    pub fn content_type(&self) -> &'static str {
        match self {
            Self::Text => "text/plain; charset=utf-8",
            Self::Srt => "application/x-subrip; charset=utf-8",
            Self::Vtt => "text/vtt; charset=utf-8",
            _ => "application/json",
        }
    }
}

/// Kanonische Anfrage einer Transkription/Übersetzung.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptionRequest {
    /// Transkription oder Übersetzung.
    pub task: AudioTask,
    /// Angefragtes Modell.
    pub model: String,
    /// Audiodaten.
    pub file: Bytes,
    /// Dateiname (der Anbieter erkennt das Format oft an der Endung).
    pub filename: String,
    /// MIME-Typ der Datei.
    pub content_type: Option<String>,
    /// Sprache der Eingabe (ISO-639-1).
    pub language: Option<String>,
    /// Kontext-Prompt.
    pub prompt: Option<String>,
    /// Ausgabeformat.
    pub response_format: Option<TranscriptFormat>,
    /// Temperatur.
    pub temperature: Option<f64>,
    /// `timestamp_granularities[]` (`word`, `segment`).
    pub timestamp_granularities: Vec<String>,
    /// Weitere Formularfelder (Name, Wert) in Reihenfolge, z. B. `include[]`,
    /// `chunking_strategy`.
    pub extra: Vec<(String, String)>,
    /// Ursprungsformat.
    pub source: Option<WireFormat>,
}

impl TranscriptionRequest {
    /// Gewähltes Ausgabeformat (Standard `json`).
    pub fn format(&self) -> TranscriptFormat {
        self.response_format.clone().unwrap_or_default()
    }

    /// `true`, wenn `extra` beim Serialisieren nach `target` ausgegeben werden soll.
    pub fn keeps_extra_for(&self, target: WireFormat) -> bool {
        self.source == Some(target)
    }
}

/// Kanonische Antwort einer Transkription/Übersetzung.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptionResponse {
    /// Format, in dem die Antwort vorliegt.
    pub format: TranscriptFormat,
    /// Text (bei `text`/`srt`/`vtt` der vollständige Body).
    pub text: String,
    /// Erkannte Sprache (`verbose_json`).
    pub language: Option<String>,
    /// Dauer der Eingabe in Sekunden (`verbose_json` bzw. Usage `duration`).
    pub duration_secs: Option<f64>,
    /// Verbrauch (`audio_ms`, ggf. Tokens bei `gpt-4o-transcribe`).
    pub usage: Usage,
    /// Weitere Felder der JSON-Antwort (`segments`, `words`, `logprobs`, `usage` …), in
    /// Reihenfolge.
    pub extra: Map<String, Value>,
}

/// Kanonische Anfrage der Sprachausgabe.
#[derive(Debug, Clone, PartialEq)]
pub struct SpeechRequest {
    /// Angefragtes Modell.
    pub model: String,
    /// Zu sprechender Text.
    pub input: String,
    /// Stimme (String oder Objekt `{"id": …}`).
    pub voice: Value,
    /// Audioformat (`mp3`, `opus`, `aac`, `flac`, `wav`, `pcm`).
    pub response_format: Option<String>,
    /// Geschwindigkeit (0,25–4,0).
    pub speed: Option<f64>,
    /// Anweisungen zur Sprechweise (`gpt-4o-mini-tts`).
    pub instructions: Option<String>,
    /// `audio` (Standard) oder `sse`.
    pub stream_format: Option<String>,
    /// Unbekannte Felder.
    pub extra: Extra,
    /// Ursprungsformat.
    pub source: Option<WireFormat>,
}

impl SpeechRequest {
    /// `true`, wenn `extra` beim Serialisieren nach `target` ausgegeben werden soll.
    pub fn keeps_extra_for(&self, target: WireFormat) -> bool {
        self.source == Some(target)
    }

    /// `Content-Type` des Audioformats (Standard `mp3`).
    pub fn media_type(&self) -> &'static str {
        if self.stream_format.as_deref() == Some("sse") {
            return "text/event-stream";
        }
        match self.response_format.as_deref().unwrap_or("mp3") {
            "opus" => "audio/ogg",
            "aac" => "audio/aac",
            "flac" => "audio/flac",
            "wav" => "audio/wav",
            "pcm" => "audio/pcm",
            _ => "audio/mpeg",
        }
    }
}
