// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! OpenAI Bilderzeugung (`POST /v1/images/generations`) ↔ IR.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::ir::images::{GeneratedImage, ImageRequest, ImageResponse, ImageResponseFormat};
use crate::ir::{ApiError, ErrorKind, Usage};
use crate::wire::{Losses, WireFormat, keep, unix_now};

const FMT: WireFormat = WireFormat::OpenAiImages;

/// Anfrage `POST /v1/images/generations`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageWireRequest {
    /// Modell.
    pub model: String,
    /// Prompt.
    pub prompt: String,
    /// Anzahl.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
    /// Größe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    /// Qualität.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
    /// Stil.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    /// `url` oder `b64_json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ImageResponseFormat>,
    /// Endnutzer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Unbekannte Felder.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Parst einen Request-Body in die IR.
pub fn decode_image_request(body: &[u8]) -> Result<ImageRequest, ApiError> {
    let w: ImageWireRequest = serde_json::from_slice(body)?;
    if w.extra.get("stream").and_then(Value::as_bool) == Some(true) {
        return Err(ApiError::invalid_request(
            "stream: true wird für /v1/images/generations nicht unterstützt",
        )
        .with_param("stream"));
    }
    Ok(ImageRequest {
        model: w.model,
        prompt: w.prompt,
        n: w.n,
        size: w.size,
        quality: w.quality,
        style: w.style,
        response_format: w.response_format,
        user: w.user,
        extra: w.extra,
        source: Some(FMT),
    })
}

/// IR → Anfrage an einen Anbieter im OpenAI-Format.
pub fn image_request_to_wire(req: ImageRequest, losses: &mut Losses) -> ImageWireRequest {
    let k = req.keeps_extra_for(FMT);
    ImageWireRequest {
        model: req.model,
        prompt: req.prompt,
        n: req.n,
        size: req.size,
        quality: req.quality,
        style: req.style,
        response_format: req.response_format,
        user: req.user,
        extra: keep(req.extra, k, losses, "images"),
    }
}

/// Antwort.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageWireResponse {
    /// Unix-Sekunden.
    #[serde(default)]
    pub created: i64,
    /// Bilder.
    #[serde(default)]
    pub data: Vec<ImageObject>,
    /// Verbrauch (`gpt-image-*`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Map<String, Value>>,
    /// Unbekannte Felder (`background`, `output_format`, `size`, `quality` …).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Ein Bild.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageObject {
    /// Base64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub b64_json: Option<String>,
    /// URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Überarbeiteter Prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revised_prompt: Option<String>,
}

/// Parst eine Antwort im OpenAI-Format.
pub fn decode_image_response(body: &[u8]) -> Result<ImageResponse, ApiError> {
    let w: ImageWireResponse = serde_json::from_slice(body).map_err(|e| {
        ApiError::new(
            ErrorKind::BadGateway,
            format!("Bild-Antwort nicht lesbar: {e}"),
        )
        .with_code("upstream_protocol_error")
    })?;
    let mut usage = Usage::default();
    if let Some(u) = &w.usage {
        let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        usage.input_tokens = n("input_tokens");
        usage.output_tokens = n("output_tokens");
    }
    usage.images = Some(u64::try_from(w.data.len()).unwrap_or(u64::MAX));
    let mut extra = w.extra;
    if let Some(u) = w.usage {
        extra.insert("usage".into(), Value::Object(u));
    }
    Ok(ImageResponse {
        created: if w.created > 0 { w.created } else { unix_now() },
        data: w
            .data
            .into_iter()
            .map(|d| GeneratedImage {
                b64_json: d.b64_json,
                url: d.url,
                revised_prompt: d.revised_prompt,
                media_type: None,
            })
            .collect(),
        usage,
        extra,
        source: Some(FMT),
    })
}

/// Serialisiert eine IR-Antwort für den Client. Fehlt die Usage des Anbieters, wird nur die
/// Anzahl der Bilder im Request-Log geführt (kein `usage`-Feld in der Antwort).
pub fn encode_image_response(
    resp: ImageResponse,
    requested: Option<ImageResponseFormat>,
    losses: &mut Losses,
) -> Result<Vec<u8>, ApiError> {
    let k = resp.source == Some(FMT);
    let mut extra = keep(resp.extra, k, losses, "images response");
    let usage = match extra.remove("usage") {
        Some(Value::Object(u)) => Some(u),
        _ if resp.usage.input_tokens > 0 || resp.usage.output_tokens > 0 => {
            let mut u = Map::new();
            u.insert("input_tokens".into(), resp.usage.input_tokens.into());
            u.insert("output_tokens".into(), resp.usage.output_tokens.into());
            u.insert("total_tokens".into(), resp.usage.total_tokens().into());
            Some(u)
        }
        _ => None,
    };
    let data = resp
        .data
        .into_iter()
        .map(|d| {
            match requested {
                Some(ImageResponseFormat::B64Json) if d.b64_json.is_none() && d.url.is_some() => {
                    losses.note(
                        "b64_json angefordert, Anbieter lieferte nur eine URL (nicht nachgeladen)",
                    );
                }
                Some(ImageResponseFormat::Url) if d.url.is_none() && d.b64_json.is_some() => {
                    losses.note("url angefordert, Anbieter lieferte Base64");
                }
                _ => {}
            }
            ImageObject {
                b64_json: d.b64_json,
                url: d.url,
                revised_prompt: d.revised_prompt,
            }
        })
        .collect();
    let wire = ImageWireResponse {
        created: resp.created,
        data,
        usage,
        extra,
    };
    serde_json::to_vec(&wire).map_err(|e| ApiError::new(ErrorKind::Internal, e.to_string()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use serde_json::json;

    #[test]
    fn request_roundtrip_keeps_extra() {
        let body = json!({"model": "dall-e-3", "prompt": "Katze", "n": 1, "size": "1024x1024",
                          "response_format": "b64_json", "background": "transparent"});
        let r = decode_image_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(r.response_format, Some(ImageResponseFormat::B64Json));
        let mut losses = Losses::new();
        let w = serde_json::to_value(image_request_to_wire(r, &mut losses)).unwrap();
        assert_eq!(w, body);
        assert!(losses.is_empty());
        let body = json!({"model": "m", "prompt": "p", "stream": true});
        assert!(decode_image_request(body.to_string().as_bytes()).is_err());
    }

    #[test]
    fn response_with_usage_and_url() {
        let body = json!({"created": 5, "data": [{"url": "https://example.invalid/a.png",
            "revised_prompt": "eine Katze"}, {"b64_json": "AAAA"}],
            "usage": {"input_tokens": 3, "output_tokens": 100, "total_tokens": 103},
            "output_format": "png"});
        let r = decode_image_response(body.to_string().as_bytes()).unwrap();
        assert_eq!(r.usage.images, Some(2));
        assert_eq!(r.usage.output_tokens, 100);
        let mut losses = Losses::new();
        let out: Value = serde_json::from_slice(
            &encode_image_response(r, Some(ImageResponseFormat::B64Json), &mut losses).unwrap(),
        )
        .unwrap();
        assert_eq!(out, body);
        assert_eq!(losses.items().len(), 1);
    }
}
