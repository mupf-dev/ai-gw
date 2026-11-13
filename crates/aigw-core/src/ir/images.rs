// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Bilderzeugung (`POST /v1/images/generations`, AIGW-21).
//!
//! **Kein Nachladen von URLs:** Liefert ein Anbieter eine Bild-URL, wird sie unverändert an den
//! Client weitergegeben; das Gateway lädt sie nie selbst (SSRF-Schutz). Will der Client
//! `b64_json`, der Anbieter liefert aber nur URLs, bleibt es bei der URL (Verlust-Hinweis).

use serde::{Deserialize, Serialize};
use serde_json::Map;

use super::{Extra, Usage};
use crate::wire::WireFormat;

/// Gewünschte Rückgabe der Bilder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageResponseFormat {
    /// Vom Anbieter gehostete URL.
    Url,
    /// Base64 im JSON.
    B64Json,
}

/// Kanonische Anfrage zur Bilderzeugung.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageRequest {
    /// Angefragtes Modell.
    pub model: String,
    /// Beschreibung des Bildes.
    pub prompt: String,
    /// Anzahl der Bilder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
    /// Größe (`1024x1024`, `auto` …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    /// Qualität (`standard`, `hd`, `low`, `medium`, `high`, `auto`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
    /// Stil (`vivid`, `natural`; DALL·E 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    /// Rückgabe als URL oder Base64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ImageResponseFormat>,
    /// Endnutzer-Kennung.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Unbekannte Felder (`background`, `output_format`, `moderation` …).
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Extra,
    /// Ursprungsformat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<WireFormat>,
}

impl ImageRequest {
    /// Angeforderte Anzahl (Standard 1).
    pub fn count(&self) -> u32 {
        self.n.unwrap_or(1)
    }

    /// `true`, wenn `extra` beim Serialisieren nach `target` ausgegeben werden soll.
    pub fn keeps_extra_for(&self, target: WireFormat) -> bool {
        self.source == Some(target)
    }
}

/// Ein erzeugtes Bild.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneratedImage {
    /// Base64-Daten.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub b64_json: Option<String>,
    /// URL beim Anbieter (wird nie vom Gateway geladen).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Vom Modell überarbeiteter Prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revised_prompt: Option<String>,
    /// MIME-Typ, falls bekannt (z. B. `image/png`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
}

/// Kanonische Antwort der Bilderzeugung.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageResponse {
    /// Erstellungszeit (Unix-Sekunden).
    pub created: i64,
    /// Bilder.
    pub data: Vec<GeneratedImage>,
    /// Verbrauch (`images` = Anzahl; Tokens, falls der Anbieter sie meldet).
    pub usage: Usage,
    /// Unbekannte Felder (`background`, `output_format`, `size`, `quality` …).
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Extra,
    /// Ursprungsformat (steuert die Ausgabe von `extra`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<WireFormat>,
}
