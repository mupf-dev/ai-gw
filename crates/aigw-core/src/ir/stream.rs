// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Ereignisse einer gestreamten Antwort.
//!
//! Inhaltsblöcke tragen einen fortlaufenden `index` (wie Anthropic `content_block`), beginnend
//! bei 0. Ein neuer Index beendet implizit den vorherigen Block; Encoder schließen offene Blöcke
//! selbst. Die Reihenfolge ist: [`StreamEvent::Start`], beliebig viele Deltas,
//! optional [`StreamEvent::Stop`], optional [`StreamEvent::Usage`] (darf mehrfach kommen,
//! der letzte Wert gilt, Felder sind kumulativ). Das Ende des Streams signalisiert der
//! Aufrufer dem Encoder über dessen `finish`.
//!
//! **Mehrere Antwortvarianten** (`n > 1`, F12): [`StreamEvent::Choice`] schaltet die Variante
//! um; alle folgenden Ereignisse bis zum nächsten `Choice` gehören zu ihr (ohne `Choice` gilt
//! Variante 0). Blockindizes zählen je Variante, jede Variante hat ihr eigenes `Stop`. Die
//! Varianten dürfen sich abwechseln (verschränkt); `Start` und `Usage` (Summe über alle
//! Varianten) gelten für die ganze Antwort.

use serde::{Deserialize, Serialize};

use super::{ApiError, StopReason, Usage};

/// Ereignis eines Antwort-Streams.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// Beginn der Antwort.
    Start {
        /// Antwort-ID.
        id: String,
        /// Modell.
        model: String,
        /// Erstellungszeit (Unix-Sekunden).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created: Option<i64>,
    },
    /// Textstück im Block `index`.
    TextDelta {
        /// Blockindex.
        index: u32,
        /// Text.
        text: String,
    },
    /// Reasoning-Textstück im Block `index`.
    ReasoningDelta {
        /// Blockindex.
        index: u32,
        /// Text.
        text: String,
    },
    /// Signatur des Reasoning-Blocks `index` (Anthropic).
    ReasoningSignature {
        /// Blockindex.
        index: u32,
        /// Signatur.
        signature: String,
    },
    /// Vollständiger verschlüsselter Reasoning-Block (Anthropic `redacted_thinking`).
    RedactedReasoning {
        /// Blockindex.
        index: u32,
        /// Daten.
        data: String,
    },
    /// Beginn eines Tool-Aufrufs im Block `index`.
    ToolCallStart(ToolCallStart),
    /// Argument-Fragment (JSON-Text) des Tool-Aufrufs im Block `index`.
    ToolCallDelta {
        /// Blockindex.
        index: u32,
        /// JSON-Fragment.
        arguments: String,
    },
    /// Grund für das Ende.
    Stop {
        /// Grund.
        reason: StopReason,
        /// Ausgelöste Stoppsequenz.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_sequence: Option<String>,
    },
    /// Token-Verbrauch (kumulativ).
    Usage(Usage),
    /// Fehler mitten im Stream (beendet den Stream).
    Error(ApiError),
    /// Die folgenden Ereignisse gehören zur Antwortvariante `index` (nur bei `n > 1`).
    Choice {
        /// Index der Variante (0 … n-1).
        index: u32,
    },
}

/// Beginn eines Tool-Aufrufs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallStart {
    /// Blockindex.
    pub index: u32,
    /// Aufruf-ID.
    pub id: String,
    /// Tool-Name.
    pub name: String,
}
