// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! OpenAI Embeddings (`POST /v1/embeddings`) ↔ IR.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::ir::embeddings::{
    Embedding, EmbeddingInput, EmbeddingsRequest, EmbeddingsResponse, EncodingFormat,
};
use crate::ir::{ApiError, ErrorKind, Usage};
use crate::wire::{Losses, WireFormat, base64, keep};

const FMT: WireFormat = WireFormat::OpenAiEmbeddings;

/// Anfrage `POST /v1/embeddings`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingsWireRequest {
    /// Modell.
    pub model: String,
    /// String, Liste von Strings, Token-Liste oder Liste von Token-Listen.
    pub input: Value,
    /// `float` oder `base64`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding_format: Option<String>,
    /// Dimension.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<u32>,
    /// Endnutzer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Unbekannte Felder.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn bad_input() -> ApiError {
    ApiError::invalid_request(
        "input muss ein String, eine Liste von Strings, eine Token-Liste oder eine Liste von \
         Token-Listen sein",
    )
    .with_param("input")
}

fn tokens(v: &[Value]) -> Option<Vec<u32>> {
    v.iter()
        .map(|t| t.as_u64().and_then(|n| u32::try_from(n).ok()))
        .collect()
}

/// `input` → IR (`single` = einzelne Eingabe).
fn input_from_wire(v: Value) -> Result<(EmbeddingInput, bool), ApiError> {
    match v {
        Value::String(s) => Ok((EmbeddingInput::Text(vec![s]), true)),
        Value::Array(items) if items.is_empty() => Err(bad_input()),
        Value::Array(items) => match items.first() {
            Some(Value::String(_)) => items
                .into_iter()
                .map(|i| match i {
                    Value::String(s) => Ok(s),
                    _ => Err(bad_input()),
                })
                .collect::<Result<Vec<_>, _>>()
                .map(|t| (EmbeddingInput::Text(t), false)),
            Some(Value::Number(_)) => tokens(&items)
                .map(|t| (EmbeddingInput::Tokens(vec![t]), true))
                .ok_or_else(bad_input),
            Some(Value::Array(_)) => items
                .iter()
                .map(|i| i.as_array().and_then(|a| tokens(a)))
                .collect::<Option<Vec<_>>>()
                .map(|t| (EmbeddingInput::Tokens(t), false))
                .ok_or_else(bad_input),
            _ => Err(bad_input()),
        },
        _ => Err(bad_input()),
    }
}

/// IR-Eingabe → `input`.
pub fn input_to_wire(input: &EmbeddingInput, single: bool) -> Value {
    match input {
        EmbeddingInput::Text(t) if single && t.len() == 1 => Value::from(t[0].as_str()),
        EmbeddingInput::Text(t) => Value::from(t.clone()),
        EmbeddingInput::Tokens(t) if single && t.len() == 1 => Value::from(t[0].clone()),
        EmbeddingInput::Tokens(t) => Value::from(t.clone()),
    }
}

/// Parst einen Request-Body in die IR.
pub fn decode_embeddings_request(body: &[u8]) -> Result<EmbeddingsRequest, ApiError> {
    let w: EmbeddingsWireRequest = serde_json::from_slice(body)?;
    let (input, single_input) = input_from_wire(w.input)?;
    let encoding_format = match w.encoding_format.as_deref() {
        None => None,
        Some("float") => Some(EncodingFormat::Float),
        Some("base64") => Some(EncodingFormat::Base64),
        Some(other) => {
            return Err(ApiError::invalid_request(format!(
                "encoding_format {other:?} unbekannt (float, base64)"
            ))
            .with_param("encoding_format"));
        }
    };
    Ok(EmbeddingsRequest {
        model: w.model,
        input,
        single_input,
        encoding_format,
        dimensions: w.dimensions,
        user: w.user,
        extra: w.extra,
        source: Some(FMT),
    })
}

/// IR → Anfrage an einen Anbieter im OpenAI-Format. `upstream_format` ist das Format, in dem
/// der Anbieter die Vektoren liefern soll (`None` = Feld weglassen).
pub fn embeddings_request_to_wire(
    req: EmbeddingsRequest,
    upstream_format: Option<EncodingFormat>,
    losses: &mut Losses,
) -> EmbeddingsWireRequest {
    let k = req.keeps_extra_for(FMT);
    EmbeddingsWireRequest {
        input: input_to_wire(&req.input, req.single_input),
        model: req.model,
        encoding_format: upstream_format.map(|f| match f {
            EncodingFormat::Float => "float".to_owned(),
            EncodingFormat::Base64 => "base64".to_owned(),
        }),
        dimensions: req.dimensions,
        user: req.user,
        extra: keep(req.extra, k, losses, "embeddings"),
    }
}

/// Antwort `list` mit Vektoren.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingsWireResponse {
    /// `list`.
    #[serde(default = "object_list")]
    pub object: String,
    /// Vektoren.
    pub data: Vec<EmbeddingObject>,
    /// Modell.
    #[serde(default)]
    pub model: String,
    /// Verbrauch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<EmbeddingsUsage>,
    /// Unbekannte Felder.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn object_list() -> String {
    "list".to_owned()
}

/// Ein Vektor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingObject {
    /// `embedding`.
    #[serde(default = "object_embedding")]
    pub object: String,
    /// Position.
    #[serde(default)]
    pub index: u32,
    /// Zahlenliste oder Base64-String.
    pub embedding: EmbeddingValue,
}

/// Vektor als Zahlenliste (`f32`, kürzeste Darstellung) oder Base64-String.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmbeddingValue {
    /// Zahlen.
    Float(Vec<f32>),
    /// Base64 der Little-Endian-`f32`-Werte.
    Base64(String),
}

fn object_embedding() -> String {
    "embedding".to_owned()
}

/// Verbrauch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingsUsage {
    /// Eingabe-Tokens.
    #[serde(default)]
    pub prompt_tokens: u64,
    /// Summe (= Eingabe).
    #[serde(default)]
    pub total_tokens: u64,
}

/// Vektor aus Zahlenliste oder Base64.
pub fn vector_from_wire(v: EmbeddingValue) -> Option<Vec<f32>> {
    match v {
        EmbeddingValue::Float(v) => Some(v),
        EmbeddingValue::Base64(s) => base64::decode_f32(&s),
    }
}

fn protocol(what: &str) -> ApiError {
    ApiError::new(
        ErrorKind::BadGateway,
        format!("Embedding-Antwort nicht lesbar: {what}"),
    )
    .with_code("upstream_protocol_error")
}

/// Parst eine Embedding-Antwort im OpenAI-Format (Vektoren als Zahlen oder Base64).
pub fn decode_embeddings_response(body: &[u8]) -> Result<EmbeddingsResponse, ApiError> {
    let w: EmbeddingsWireResponse =
        serde_json::from_slice(body).map_err(|e| protocol(&e.to_string()))?;
    let mut data = w
        .data
        .into_iter()
        .map(|d| {
            vector_from_wire(d.embedding)
                .map(|vector| Embedding {
                    index: d.index,
                    vector,
                })
                .ok_or_else(|| protocol("embedding ist weder Zahlenliste noch Base64"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    data.sort_by_key(|e| e.index);
    let usage = w.usage.map(|u| Usage {
        input_tokens: u.prompt_tokens,
        ..Usage::default()
    });
    Ok(EmbeddingsResponse {
        model: w.model,
        data,
        usage_estimated: usage.is_none(),
        usage: usage.unwrap_or_default(),
        extra: w.extra,
    })
}

/// Serialisiert eine IR-Antwort für den Client.
pub fn encode_embeddings_response(
    resp: EmbeddingsResponse,
    format: EncodingFormat,
    losses: &mut Losses,
) -> Result<Vec<u8>, ApiError> {
    losses.extra("embeddings response", &resp.extra);
    let wire = EmbeddingsWireResponse {
        object: object_list(),
        data: resp
            .data
            .into_iter()
            .map(|e| EmbeddingObject {
                object: object_embedding(),
                index: e.index,
                embedding: match format {
                    EncodingFormat::Float => EmbeddingValue::Float(e.vector),
                    EncodingFormat::Base64 => EmbeddingValue::Base64(base64::encode_f32(&e.vector)),
                },
            })
            .collect(),
        model: resp.model,
        usage: Some(EmbeddingsUsage {
            prompt_tokens: resp.usage.input_tokens,
            total_tokens: resp.usage.input_tokens,
        }),
        extra: Map::new(),
    };
    serde_json::to_vec(&wire).map_err(|e| ApiError::new(ErrorKind::Internal, e.to_string()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use serde_json::json;

    #[test]
    fn input_forms() {
        for (input, single, n) in [
            (json!("a"), true, 1),
            (json!(["a", "b"]), false, 2),
            (json!([1, 2, 3]), true, 1),
            (json!([[1], [2, 3]]), false, 2),
        ] {
            let body = json!({"model": "m", "input": input.clone(), "foo": 1});
            let r = decode_embeddings_request(body.to_string().as_bytes()).unwrap();
            assert_eq!(r.single_input, single);
            assert_eq!(r.input.len(), n);
            assert_eq!(input_to_wire(&r.input, r.single_input), input);
            assert_eq!(r.extra["foo"], 1);
        }
        for bad in [json!([]), json!(["a", 1]), json!(5), json!([[1], "a"])] {
            let body = json!({"model": "m", "input": bad});
            assert!(decode_embeddings_request(body.to_string().as_bytes()).is_err());
        }
        let body = json!({"model": "m", "input": "a", "encoding_format": "int8"});
        assert!(decode_embeddings_request(body.to_string().as_bytes()).is_err());
    }

    #[test]
    fn response_float_and_base64() {
        let b64 = base64::encode_f32(&[0.5, -1.0]);
        let body = json!({"object": "list", "model": "text-embedding-3-small",
            "data": [{"object": "embedding", "index": 1, "embedding": b64},
                     {"object": "embedding", "index": 0, "embedding": [0.25, 0.75]}],
            "usage": {"prompt_tokens": 7, "total_tokens": 7}});
        let r = decode_embeddings_response(body.to_string().as_bytes()).unwrap();
        assert_eq!(r.data[0].vector, vec![0.25, 0.75]);
        assert_eq!(r.data[1].vector, vec![0.5, -1.0]);
        assert_eq!(r.usage.input_tokens, 7);
        assert!(!r.usage_estimated);

        let mut losses = Losses::new();
        let out: Value = serde_json::from_slice(
            &encode_embeddings_response(r.clone(), EncodingFormat::Float, &mut losses).unwrap(),
        )
        .unwrap();
        assert_eq!(out["data"][1]["embedding"], json!([0.5, -1.0]));
        assert_eq!(out["usage"], json!({"prompt_tokens": 7, "total_tokens": 7}));
        let out: Value = serde_json::from_slice(
            &encode_embeddings_response(r, EncodingFormat::Base64, &mut losses).unwrap(),
        )
        .unwrap();
        assert_eq!(out["data"][1]["embedding"], json!(b64));
    }
}
