// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! OpenAI Audio ↔ IR: Transkription/Übersetzung (`multipart/form-data`) und Sprachausgabe
//! (JSON-Anfrage, Audio-Bytes als Antwort).

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::ir::audio::{
    AudioTask, SpeechRequest, TranscriptFormat, TranscriptionRequest, TranscriptionResponse,
};
use crate::ir::{ApiError, ErrorKind, Usage};
use crate::wire::multipart::{self, FormBuilder};
use crate::wire::{Losses, WireFormat, keep};

const FMT: WireFormat = WireFormat::OpenAiAudio;

fn field_text(p: &multipart::FormPart) -> Result<String, ApiError> {
    p.text().map(str::to_owned).ok_or_else(|| {
        ApiError::invalid_request(format!("Feld {} ist kein UTF-8-Text", p.name))
            .with_param(p.name.clone())
    })
}

/// Parst ein Upload-Formular (`multipart/form-data`) in die IR.
pub fn decode_transcription_request(
    content_type: &str,
    body: &Bytes,
    task: AudioTask,
) -> Result<TranscriptionRequest, ApiError> {
    let boundary = multipart::boundary(content_type).ok_or_else(|| {
        ApiError::invalid_request("Content-Type muss multipart/form-data mit boundary sein")
    })?;
    let parts =
        multipart::parse(body, boundary).map_err(|e| ApiError::invalid_request(e.to_string()))?;
    let mut req = TranscriptionRequest {
        task,
        model: String::new(),
        file: Bytes::new(),
        filename: String::new(),
        content_type: None,
        language: None,
        prompt: None,
        response_format: None,
        temperature: None,
        timestamp_granularities: Vec::new(),
        extra: Vec::new(),
        source: Some(FMT),
    };
    let mut have_file = false;
    for p in parts {
        match p.name.as_str() {
            "file" => {
                have_file = true;
                req.filename = p.filename.clone().unwrap_or_else(|| "audio".to_owned());
                req.content_type = p.content_type.clone();
                req.file = p.data;
            }
            "model" => req.model = field_text(&p)?,
            "language" => req.language = Some(field_text(&p)?),
            "prompt" => req.prompt = Some(field_text(&p)?),
            "response_format" => {
                req.response_format = Some(TranscriptFormat::parse(&field_text(&p)?));
            }
            "temperature" => {
                let t = field_text(&p)?;
                req.temperature = Some(t.trim().parse().map_err(|_| {
                    ApiError::invalid_request("temperature ist keine Zahl")
                        .with_param("temperature")
                })?);
            }
            "timestamp_granularities[]" | "timestamp_granularities" => {
                req.timestamp_granularities.push(field_text(&p)?);
            }
            "stream" => {
                if field_text(&p)?.trim() == "true" {
                    return Err(ApiError::invalid_request(
                        "stream: true wird für Transkriptionen (noch) nicht unterstützt",
                    )
                    .with_param("stream"));
                }
            }
            _ => {
                let v = field_text(&p)?;
                req.extra.push((p.name, v));
            }
        }
    }
    if !have_file || req.file.is_empty() {
        return Err(ApiError::invalid_request("Feld file fehlt oder ist leer").with_param("file"));
    }
    if req.model.trim().is_empty() {
        return Err(ApiError::invalid_request("Feld model fehlt").with_param("model"));
    }
    Ok(req)
}

/// IR → Upload-Formular für einen Anbieter im OpenAI-Format: (`Content-Type`, Body).
pub fn transcription_form(req: &TranscriptionRequest, losses: &mut Losses) -> (String, Bytes) {
    let mut f = FormBuilder::new();
    f.text("model", &req.model);
    if let Some(l) = &req.language {
        if req.task == AudioTask::Translate {
            losses.note("language wird bei Übersetzungen nicht übertragen");
        } else {
            f.text("language", l);
        }
    }
    if let Some(p) = &req.prompt {
        f.text("prompt", p);
    }
    if let Some(r) = &req.response_format {
        f.text("response_format", r.as_str());
    }
    if let Some(t) = req.temperature {
        f.text("temperature", &t.to_string());
    }
    for g in &req.timestamp_granularities {
        f.text("timestamp_granularities[]", g);
    }
    if req.keeps_extra_for(FMT) {
        for (k, v) in &req.extra {
            f.text(k, v);
        }
    } else if !req.extra.is_empty() {
        losses.note("audio: unbekannte Formularfelder verworfen");
    }
    f.file(
        "file",
        &req.filename,
        req.content_type
            .as_deref()
            .unwrap_or("application/octet-stream"),
        &req.file,
    );
    f.finish()
}

fn protocol(what: impl std::fmt::Display) -> ApiError {
    ApiError::new(
        ErrorKind::BadGateway,
        format!("Transkriptions-Antwort nicht lesbar: {what}"),
    )
    .with_code("upstream_protocol_error")
}

/// Liest die Usage einer Transkription (`{"type":"duration","seconds":…}` bzw.
/// `{"type":"tokens",…}`).
fn usage_from(u: Option<&Value>, duration: Option<f64>) -> Usage {
    let mut usage = Usage::default();
    let secs = |s: f64| (s.is_finite() && s >= 0.0).then(|| (s * 1000.0).round() as u64);
    if let Some(u) = u {
        usage.input_tokens = u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
        usage.output_tokens = u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
        usage.audio_ms = u.get("seconds").and_then(Value::as_f64).and_then(secs);
    }
    if usage.audio_ms.is_none() {
        usage.audio_ms = duration.and_then(secs);
    }
    usage
}

/// Parst die Antwort eines Anbieters im OpenAI-Format (`format` = angefragtes Format).
pub fn decode_transcription_response(
    body: &[u8],
    format: TranscriptFormat,
) -> Result<TranscriptionResponse, ApiError> {
    if format.is_plain_text() {
        let text = String::from_utf8(body.to_vec()).map_err(protocol)?;
        return Ok(TranscriptionResponse {
            format,
            text,
            language: None,
            duration_secs: None,
            usage: Usage::default(),
            extra: Map::new(),
        });
    }
    let mut obj: Map<String, Value> = serde_json::from_slice(body).map_err(protocol)?;
    let text = match obj.remove("text") {
        Some(Value::String(t)) => t,
        _ => return Err(protocol("Feld text fehlt")),
    };
    let language = match obj.remove("language") {
        Some(Value::String(l)) => Some(l),
        _ => None,
    };
    let duration_secs = obj.remove("duration").and_then(|d| d.as_f64());
    let usage = usage_from(obj.get("usage"), duration_secs);
    Ok(TranscriptionResponse {
        format,
        text,
        language,
        duration_secs,
        usage,
        extra: obj,
    })
}

/// Serialisiert eine IR-Antwort für den Client: (`Content-Type`, Body).
pub fn encode_transcription_response(
    resp: TranscriptionResponse,
    task: AudioTask,
) -> Result<(&'static str, Vec<u8>), ApiError> {
    let ct = resp.format.content_type();
    if resp.format.is_plain_text() {
        return Ok((ct, resp.text.into_bytes()));
    }
    let mut obj = Map::new();
    if resp.format == TranscriptFormat::VerboseJson {
        obj.insert("task".into(), Value::from(task.as_str()));
    }
    if let Some(l) = resp.language {
        obj.insert("language".into(), Value::from(l));
    }
    if let Some(d) = resp.duration_secs {
        obj.insert("duration".into(), Value::from(d));
    }
    obj.insert("text".into(), Value::from(resp.text));
    for (k, v) in resp.extra {
        obj.entry(k).or_insert(v);
    }
    let body =
        serde_json::to_vec(&obj).map_err(|e| ApiError::new(ErrorKind::Internal, e.to_string()))?;
    Ok((ct, body))
}

/// Anfrage `POST /v1/audio/speech`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeechWireRequest {
    /// Modell.
    pub model: String,
    /// Text.
    pub input: String,
    /// Stimme (String oder `{"id": …}`).
    pub voice: Value,
    /// Audioformat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<String>,
    /// Geschwindigkeit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f64>,
    /// Anweisungen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// `audio` oder `sse`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_format: Option<String>,
    /// Unbekannte Felder.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Parst einen Request-Body in die IR.
pub fn decode_speech_request(body: &[u8]) -> Result<SpeechRequest, ApiError> {
    let w: SpeechWireRequest = serde_json::from_slice(body)?;
    if let Some(f) = &w.stream_format
        && f != "audio"
        && f != "sse"
    {
        return Err(ApiError::invalid_request(format!(
            "stream_format {f:?} unbekannt (audio, sse)"
        ))
        .with_param("stream_format"));
    }
    Ok(SpeechRequest {
        model: w.model,
        input: w.input,
        voice: w.voice,
        response_format: w.response_format,
        speed: w.speed,
        instructions: w.instructions,
        stream_format: w.stream_format,
        extra: w.extra,
        source: Some(FMT),
    })
}

/// IR → Anfrage an einen Anbieter im OpenAI-Format.
pub fn speech_request_to_wire(req: SpeechRequest, losses: &mut Losses) -> SpeechWireRequest {
    let k = req.keeps_extra_for(FMT);
    SpeechWireRequest {
        model: req.model,
        input: req.input,
        voice: req.voice,
        response_format: req.response_format,
        speed: req.speed,
        instructions: req.instructions,
        stream_format: req.stream_format,
        extra: keep(req.extra, k, losses, "speech"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use serde_json::json;

    #[test]
    fn form_roundtrip() {
        let mut f = FormBuilder::new();
        f.text("model", "whisper-1")
            .text("response_format", "verbose_json")
            .text("timestamp_granularities[]", "word")
            .text("timestamp_granularities[]", "segment")
            .text("include[]", "logprobs")
            .file("file", "a.wav", "audio/wav", b"RIFFdata");
        let (ct, body) = f.finish();
        let req = decode_transcription_request(&ct, &body, AudioTask::Transcribe).unwrap();
        assert_eq!(req.model, "whisper-1");
        assert_eq!(req.format(), TranscriptFormat::VerboseJson);
        assert_eq!(req.timestamp_granularities, ["word", "segment"]);
        assert_eq!(req.extra, vec![("include[]".into(), "logprobs".into())]);
        assert_eq!(&req.file[..], b"RIFFdata");

        let mut losses = Losses::new();
        let (ct2, body2) = transcription_form(&req, &mut losses);
        let again = decode_transcription_request(&ct2, &body2, AudioTask::Transcribe).unwrap();
        assert_eq!(again.timestamp_granularities, req.timestamp_granularities);
        assert_eq!(again.extra, req.extra);
        assert_eq!(again.file, req.file);
        assert!(losses.is_empty());
    }

    #[test]
    fn rejects_missing_fields() {
        let mut f = FormBuilder::new();
        f.text("model", "whisper-1");
        let (ct, body) = f.finish();
        assert!(decode_transcription_request(&ct, &body, AudioTask::Transcribe).is_err());
        assert!(
            decode_transcription_request("application/json", &body, AudioTask::Translate).is_err()
        );
    }

    #[test]
    fn responses() {
        let body = json!({"task": "transcribe", "language": "german", "duration": 2.5,
                          "text": "Hallo", "segments": [], "usage": {"type": "duration", "seconds": 3}});
        let r = decode_transcription_response(
            body.to_string().as_bytes(),
            TranscriptFormat::VerboseJson,
        )
        .unwrap();
        assert_eq!(r.usage.audio_ms, Some(3000));
        assert_eq!(r.language.as_deref(), Some("german"));
        let (ct, out) = encode_transcription_response(r, AudioTask::Transcribe).unwrap();
        assert_eq!(ct, "application/json");
        let out: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(out, body);

        let r =
            decode_transcription_response(b"1\n00:00 --> 00:01\nHallo\n", TranscriptFormat::Srt)
                .unwrap();
        let (ct, out) = encode_transcription_response(r, AudioTask::Transcribe).unwrap();
        assert!(ct.starts_with("application/x-subrip"));
        assert_eq!(out, b"1\n00:00 --> 00:01\nHallo\n");

        let r = decode_transcription_response(
            br#"{"text":"x","usage":{"type":"tokens","input_tokens":5,"output_tokens":2}}"#,
            TranscriptFormat::Json,
        )
        .unwrap();
        assert_eq!((r.usage.input_tokens, r.usage.output_tokens), (5, 2));
    }

    #[test]
    fn speech_request() {
        let body = json!({"model": "tts-1", "input": "Hallo", "voice": "alloy",
                          "response_format": "wav", "foo": 1});
        let r = decode_speech_request(body.to_string().as_bytes()).unwrap();
        assert_eq!(r.media_type(), "audio/wav");
        let mut losses = Losses::new();
        assert_eq!(
            serde_json::to_value(speech_request_to_wire(r, &mut losses)).unwrap(),
            body
        );
        let bad = json!({"model": "m", "input": "x", "voice": "a", "stream_format": "x"});
        assert!(decode_speech_request(bad.to_string().as_bytes()).is_err());
    }
}
