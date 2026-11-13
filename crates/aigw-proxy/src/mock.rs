// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

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

use std::sync::Arc;

use aigw_core::ir::{
    ApiError, ChatRequest, ChatResponse, Choice, Content, ErrorKind, Message, ModelInfo, Part,
    PartKind, Role, StopReason, StreamEvent, ToolCallStart, ToolChoice, ToolKind, UpstreamError,
    Usage,
};
use aigw_core::resource::Id;
use aigw_core::snapshot::DeploymentCtx;
use aigw_core::types::ProviderKind;
use futures_util::stream;

use crate::provider::{BoxFuture, ChatProvider, Describe, EventStream};
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

impl ChatProvider for MockProvider {
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
