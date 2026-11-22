// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Eingebauter Mock-Anbieter (`echo`), damit der Pfad Eingang → IR → Antwort → Encoder ohne
//! echte Anbieter testbar ist.
//!
//! Verhalten:
//! - Antworttext `Echo: <letzte Nutzernachricht>`.
//! - Sind Tools definiert, `tool_choice` nicht `none` und enthält die letzte Nutzernachricht
//!   das Wort `tool`, ruft das Modell das erste Funktions-Tool mit `{"echo": "<Text>"}` auf.
//! - Ist Reasoning angefordert, kommt vorab ein Reasoning-Block mit Signatur.
//! - Modell `mock-error` liefert einen Überlastungsfehler, `mock-stream-error` bricht einen
//!   Stream nach dem ersten Textstück mit Fehler ab. Als Deployment (Anbieterart `mock`)
//!   entscheidet `providerModel` statt des angefragten Modellnamens.
//! - Usage: Eingabe ≈ Zeichen / 4 aller Nachrichten, Ausgabe = Anzahl Wörter.
//! - Weitere Endpunkte (AIGW-21): Embeddings (deterministische, normierte Vektoren aus einem
//!   Hash des Textes, Dimension aus `dimensions`, Standard 8), Bilder (1×1-PNG als Base64
//!   bzw. Data-URL), Transkription (`Mock-Transkript (<n> Bytes)` in allen Formaten, Dauer
//!   = Bytes / 32 000 s) und Sprachausgabe (WAV mit Stille, 0,1 s je Zeichen, höchstens 2 s,
//!   in mehreren Stücken gestreamt).

use std::sync::Arc;

use aigw_core::ir::audio::{
    AudioTask, SpeechRequest, TranscriptFormat, TranscriptionRequest, TranscriptionResponse,
};
use aigw_core::ir::embeddings::{Embedding, EmbeddingInput, EmbeddingsRequest, EmbeddingsResponse};
use aigw_core::ir::images::{GeneratedImage, ImageRequest, ImageResponse, ImageResponseFormat};
use aigw_core::ir::{
    ApiError, ChatRequest, ChatResponse, Choice, Content, ErrorKind, Message, ModelInfo, Part,
    PartKind, Role, StopReason, StreamEvent, ToolCallStart, ToolChoice, ToolKind, UpstreamError,
    Usage,
};
use aigw_core::resource::Id;
use aigw_core::snapshot::DeploymentCtx;
use aigw_core::types::ProviderKind;
use bytes::Bytes;
use futures_util::stream;

use crate::provider::{
    BoxFuture, ByteStream, Capability, ChatProvider, Describe, EventStream, SpeechStream,
};
use crate::routes::StaticRoutes;

/// Mock-Anbieter.
#[derive(Debug, Clone, Default)]
pub struct MockProvider {
    deployment: Option<MockDeployment>,
}

#[derive(Debug, Clone)]
struct MockDeployment {
    name: String,
    id: Id,
    model_group: String,
    provider_model: String,
}

impl MockProvider {
    /// Neuer Mock-Anbieter (ohne Deployment; Verhalten nach angefragtem Modellnamen).
    pub fn new() -> Self {
        Self::default()
    }

    /// Mock-Anbieter für ein Deployment mit Anbieterart `mock`: Verhalten nach
    /// `providerModel` (`mock`, `mock-error`, `mock-stream-error`).
    pub fn for_deployment(dep: &DeploymentCtx) -> Self {
        Self {
            deployment: Some(MockDeployment {
                name: dep.deployment.metadata.name.clone(),
                id: dep.deployment.id,
                model_group: dep.deployment.spec.model_group.clone(),
                provider_model: dep.deployment.spec.provider_model.clone(),
            }),
        }
    }

    fn behaviour<'a>(&'a self, req: &'a ChatRequest) -> &'a str {
        self.deployment
            .as_ref()
            .map_or(req.model.as_str(), |d| d.provider_model.as_str())
    }
}

struct Plan {
    id: String,
    model: String,
    reasoning: bool,
    text: String,
    tool_call: Option<(String, String)>,
    usage: Usage,
}

fn plan(req: &ChatRequest, behaviour: &str) -> Result<Plan, ApiError> {
    if behaviour == "mock-error" {
        return Err(ApiError::new(
            ErrorKind::Overloaded,
            "Mock-Anbieter ist (absichtlich) überlastet",
        ));
    }
    let user = req.last_user_text().unwrap_or_default();
    let text = format!("Echo: {user}");
    let tool_call = if req.tool_choice != Some(ToolChoice::None) && user.contains("tool") {
        req.tools.iter().find_map(|t| match &t.kind {
            ToolKind::Function(f) => Some((
                f.name.clone(),
                serde_json::json!({ "echo": user }).to_string(),
            )),
            ToolKind::Opaque { .. } => None,
        })
    } else {
        None
    };
    let input_chars: usize = req.messages.iter().map(|m| m.content.text().len()).sum();
    let usage = Usage {
        input_tokens: u64::try_from(input_chars / 4).unwrap_or(u64::MAX).max(1),
        output_tokens: u64::try_from(text.split_whitespace().count()).unwrap_or(u64::MAX),
        ..Usage::default()
    };
    Ok(Plan {
        id: format!("mock_{}", uuid::Uuid::now_v7().simple()),
        model: req.model.clone(),
        reasoning: req.reasoning.is_some(),
        text,
        tool_call,
        usage,
    })
}

const REASONING: &str = "Mock-Überlegung.";
const SIGNATURE: &str = "mock-signature";

/// Modellnamen des Mock-Anbieters.
pub const MOCK_MODELS: [&str; 3] = ["mock", "mock-error", "mock-stream-error"];

/// Routing-Tabelle, die [`MOCK_MODELS`] (für `GET /v1/models`) und jeden anderen
/// Modellnamen auf den Mock-Anbieter abbildet.
pub fn mock_routes() -> StaticRoutes {
    let provider: Arc<dyn ChatProvider> = Arc::new(MockProvider::new());
    let mut routes = StaticRoutes::new();
    routes.set_fallback(provider.clone());
    for id in MOCK_MODELS {
        routes.insert(id, provider.clone());
        routes.set_model_info(ModelInfo {
            id: id.to_owned(),
            display_name: Some(format!("Mock ({id})")),
            created: 1_763_000_000,
            owned_by: "ai-gw".to_owned(),
        });
    }
    routes
}

/// 1×1-PNG (transparent).
pub const MOCK_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

fn overloaded(behaviour: &str) -> Result<(), UpstreamError> {
    if behaviour == "mock-error" {
        return Err(UpstreamError::from(ApiError::new(
            ErrorKind::Overloaded,
            "Mock-Anbieter ist (absichtlich) überlastet",
        )));
    }
    Ok(())
}

/// Deterministischer, normierter Vektor zu einem Text (FNV-1a + xorshift).
fn mock_vector(text: &str, dims: usize) -> Vec<f32> {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    let mut v: Vec<f32> = (0..dims)
        .map(|_| {
            h ^= h << 13;
            h ^= h >> 7;
            h ^= h << 17;
            ((h >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
        })
        .collect();
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

/// WAV-Kopf (PCM, mono, 8 kHz, 8 Bit) für `samples` Abtastwerte.
fn wav_header(samples: u32) -> Vec<u8> {
    let mut h = Vec::with_capacity(44);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&(36 + samples).to_le_bytes());
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes()); // PCM
    h.extend_from_slice(&1u16.to_le_bytes()); // mono
    h.extend_from_slice(&8000u32.to_le_bytes());
    h.extend_from_slice(&8000u32.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes());
    h.extend_from_slice(&8u16.to_le_bytes());
    h.extend_from_slice(b"data");
    h.extend_from_slice(&samples.to_le_bytes());
    h
}

impl ChatProvider for MockProvider {
    fn supports(&self, _cap: Capability) -> bool {
        true
    }

    fn embeddings(
        self: Arc<Self>,
        req: EmbeddingsRequest,
    ) -> BoxFuture<Result<EmbeddingsResponse, UpstreamError>> {
        let behaviour = self
            .deployment
            .as_ref()
            .map_or(req.model.as_str(), |d| d.provider_model.as_str());
        let result = overloaded(behaviour).map(|()| {
            let dims = req.dimensions.map_or(8, |d| d.clamp(1, 4096) as usize);
            let texts: Vec<String> = match &req.input {
                EmbeddingInput::Text(t) => t.clone(),
                EmbeddingInput::Tokens(t) => t
                    .iter()
                    .map(|v| v.iter().map(u32::to_string).collect::<Vec<_>>().join(" "))
                    .collect(),
            };
            EmbeddingsResponse {
                model: req.model.clone(),
                data: texts
                    .iter()
                    .zip(0u32..)
                    .map(|(t, index)| Embedding {
                        index,
                        vector: mock_vector(t, dims),
                    })
                    .collect(),
                usage: Usage {
                    input_tokens: u64::try_from(req.input.byte_len().div_ceil(4))
                        .unwrap_or(u64::MAX)
                        .max(1),
                    ..Usage::default()
                },
                usage_estimated: false,
                extra: Default::default(),
            }
        });
        Box::pin(async move { result })
    }

    fn images(
        self: Arc<Self>,
        req: ImageRequest,
    ) -> BoxFuture<Result<ImageResponse, UpstreamError>> {
        let behaviour = self
            .deployment
            .as_ref()
            .map_or(req.model.as_str(), |d| d.provider_model.as_str());
        let result = overloaded(behaviour).map(|()| {
            let n = req.count().clamp(1, 10);
            let url = req.response_format == Some(ImageResponseFormat::Url);
            let data = (0..n)
                .map(|_| GeneratedImage {
                    b64_json: (!url).then(|| MOCK_PNG_BASE64.to_owned()),
                    url: url.then(|| format!("data:image/png;base64,{MOCK_PNG_BASE64}")),
                    revised_prompt: Some(format!("Mock: {}", req.prompt)),
                    media_type: Some("image/png".to_owned()),
                })
                .collect();
            ImageResponse {
                created: aigw_core::wire::unix_now(),
                data,
                usage: Usage {
                    images: Some(u64::from(n)),
                    ..Usage::default()
                },
                extra: Default::default(),
                source: None,
            }
        });
        Box::pin(async move { result })
    }

    fn transcription(
        self: Arc<Self>,
        req: TranscriptionRequest,
    ) -> BoxFuture<Result<TranscriptionResponse, UpstreamError>> {
        let behaviour = self
            .deployment
            .as_ref()
            .map_or(req.model.as_str(), |d| d.provider_model.as_str());
        let result = overloaded(behaviour).map(|()| {
            let text = match req.task {
                AudioTask::Transcribe => format!("Mock-Transkript ({} Bytes)", req.file.len()),
                AudioTask::Translate => format!("Mock translation ({} bytes)", req.file.len()),
            };
            let secs = req.file.len() as f64 / 32_000.0;
            let format = req.format();
            let body = match format {
                TranscriptFormat::Srt => {
                    format!("1\n00:00:00,000 --> 00:00:01,000\n{text}\n")
                }
                TranscriptFormat::Vtt => {
                    format!("WEBVTT\n\n00:00:00.000 --> 00:00:01.000\n{text}\n")
                }
                _ => text,
            };
            let verbose = format == TranscriptFormat::VerboseJson;
            TranscriptionResponse {
                text: body,
                language: verbose.then(|| req.language.clone().unwrap_or_else(|| "german".into())),
                duration_secs: verbose.then_some(secs),
                usage: Usage {
                    audio_ms: Some((secs * 1000.0).round() as u64),
                    ..Usage::default()
                },
                extra: Default::default(),
                format,
            }
        });
        Box::pin(async move { result })
    }

    fn speech(
        self: Arc<Self>,
        req: SpeechRequest,
    ) -> BoxFuture<Result<SpeechStream, UpstreamError>> {
        let behaviour = self
            .deployment
            .as_ref()
            .map_or(req.model.as_str(), |d| d.provider_model.as_str());
        let result = overloaded(behaviour).map(|()| {
            // 0,1 s Stille (800 Abtastwerte) je Zeichen, höchstens 2 s.
            let chunks = req.input.chars().count().clamp(1, 20);
            let samples = u32::try_from(chunks * 800).unwrap_or(u32::MAX);
            let mut parts = vec![Ok(Bytes::from(wav_header(samples)))];
            parts.extend((0..chunks).map(|_| Ok(Bytes::from(vec![128u8; 800]))));
            SpeechStream {
                content_type: "audio/wav".to_owned(),
                body: Box::pin(stream::iter(parts)) as ByteStream,
            }
        });
        Box::pin(async move { result })
    }

    fn describe(&self) -> Describe<'_> {
        match &self.deployment {
            Some(d) => Describe {
                deployment: &d.name,
                deployment_id: Some(d.id),
                model_group: Some(&d.model_group),
                provider: Some(ProviderKind::Mock),
                provider_model: &d.provider_model,
            },
            None => Describe {
                deployment: "mock",
                deployment_id: None,
                model_group: None,
                provider: None,
                provider_model: "mock",
            },
        }
    }

    fn chat(self: Arc<Self>, req: ChatRequest) -> BoxFuture<Result<ChatResponse, UpstreamError>> {
        let result = plan(&req, self.behaviour(&req)).map(|p| {
            let mut parts = Vec::new();
            if p.reasoning {
                parts.push(Part::new(PartKind::Reasoning {
                    text: REASONING.to_owned(),
                    signature: Some(SIGNATURE.to_owned()),
                }));
            }
            parts.push(Part::text(p.text));
            let stop_reason = if let Some((name, arguments)) = p.tool_call {
                parts.push(Part::new(PartKind::ToolCall {
                    id: format!("call_{}", uuid::Uuid::now_v7().simple()),
                    name,
                    arguments,
                }));
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            };
            ChatResponse {
                id: p.id,
                model: p.model,
                created: Some(aigw_core::wire::unix_now()),
                choices: vec![Choice {
                    index: 0,
                    message: Message {
                        role: Role::Assistant,
                        content: Content::Parts(parts),
                        name: None,
                        extra: Default::default(),
                    },
                    stop_reason: Some(stop_reason),
                    stop_sequence: None,
                    extra: Default::default(),
                }],
                usage: Some(p.usage),
                extra: Default::default(),
                source: None,
            }
        });
        Box::pin(async move { result.map_err(UpstreamError::from) })
    }

    fn chat_stream(
        self: Arc<Self>,
        req: ChatRequest,
    ) -> BoxFuture<Result<EventStream, UpstreamError>> {
        let behaviour = self.behaviour(&req);
        let fail_mid_stream = behaviour == "mock-stream-error";
        let result = plan(&req, behaviour).map(|p| {
            let mut events = vec![
                StreamEvent::Start {
                    id: p.id,
                    model: p.model,
                    created: Some(aigw_core::wire::unix_now()),
                },
                StreamEvent::Usage(Usage {
                    input_tokens: p.usage.input_tokens,
                    ..Usage::default()
                }),
            ];
            let mut index = 0;
            if p.reasoning {
                events.push(StreamEvent::ReasoningDelta {
                    index,
                    text: REASONING.to_owned(),
                });
                events.push(StreamEvent::ReasoningSignature {
                    index,
                    signature: SIGNATURE.to_owned(),
                });
                index += 1;
            }
            // Text wortweise (inkl. folgendem Leerraum) als Deltas.
            let mut first = true;
            for piece in p.text.split_inclusive(' ') {
                events.push(StreamEvent::TextDelta {
                    index,
                    text: piece.to_owned(),
                });
                if first && fail_mid_stream {
                    events.push(StreamEvent::Error(ApiError::new(
                        ErrorKind::BadGateway,
                        "Mock-Anbieter: Verbindung abgebrochen",
                    )));
                    return stream::iter(events);
                }
                first = false;
            }
            let reason = if let Some((name, arguments)) = p.tool_call {
                index += 1;
                events.push(StreamEvent::ToolCallStart(ToolCallStart {
                    index,
                    id: format!("call_{}", uuid::Uuid::now_v7().simple()),
                    name,
                }));
                let mid = arguments.len() / 2;
                let split = (0..=mid)
                    .rev()
                    .find(|i| arguments.is_char_boundary(*i))
                    .unwrap_or(0);
                let (a, b) = arguments.split_at(split);
                for chunk in [a, b] {
                    events.push(StreamEvent::ToolCallDelta {
                        index,
                        arguments: chunk.to_owned(),
                    });
                }
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            };
            events.push(StreamEvent::Stop {
                reason,
                stop_sequence: None,
            });
            events.push(StreamEvent::Usage(p.usage));
            stream::iter(events)
        });
        Box::pin(async move {
            result
                .map(|s| Box::pin(s) as EventStream)
                .map_err(UpstreamError::from)
        })
    }
}
