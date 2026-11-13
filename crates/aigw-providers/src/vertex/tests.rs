// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;

use aigw_core::ir::{Reasoning, Thinking};
use aigw_core::resource::{
    CredentialData, Deployment, DeploymentSpec, JsonObject, ObjectMeta, SecretString, new_id,
};
use aigw_core::wire::{anthropic, openai};

use super::*;
use aigw_core::ir::StreamEvent;

fn dep(model: &str, creds: &[(&str, &str)], settings: Value) -> DeploymentCtx {
    let Value::Object(settings) = settings else {
        panic!()
    };
    let settings: JsonObject = settings;
    DeploymentCtx {
        deployment: Deployment {
            id: new_id(),
            metadata: ObjectMeta::named("gemini-1"),
            spec: DeploymentSpec {
                model_group: "g".into(),
                provider: ProviderKind::Vertex,
                endpoint: None,
                provider_model: model.into(),
                credential: Some("c".into()),
                weight: 1,
                priority: 0,
                capacity: Default::default(),
                enabled: true,
                settings,
            },
        },
        credential: Some(Arc::new(CredentialData(
            creds
                .iter()
                .map(|(k, v)| ((*k).to_owned(), SecretString::new(*v)))
                .collect::<BTreeMap<_, _>>(),
        ))),
    }
}

fn gemini(model: &str) -> VertexAdapter {
    VertexAdapter::from_deployment(&dep(model, &[("apiKey", "AIza-test")], json!({}))).unwrap()
}

#[test]
fn urls_and_modes() {
    let a = gemini("gemini-2.5-flash");
    assert_eq!(a.api(), GeminiApi::Gemini);
    assert_eq!(
        a.uri().to_string(),
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent"
    );
    assert_eq!(
        a.stream_uri().to_string(),
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
    );
    let r = a
        .build_request(
            ChatRequest {
                messages: vec![Message::text(Role::User, "hi")],
                ..ChatRequest::default()
            },
            &mut Losses::new(),
        )
        .unwrap();
    assert_eq!(r.headers()["x-goog-api-key"], "AIza-test");
    assert!(r.headers()["x-goog-api-key"].is_sensitive());

    let v = VertexAdapter::from_deployment(&dep(
        "gemini-2.5-pro",
        &[("accessToken", "ya29.test")],
        json!({"project": "p1", "location": "europe-west4"}),
    ))
    .unwrap();
    assert_eq!(v.api(), GeminiApi::Vertex);
    assert_eq!(
        v.uri().to_string(),
        "https://europe-west4-aiplatform.googleapis.com/v1/projects/p1/locations/europe-west4/publishers/google/models/gemini-2.5-pro:generateContent"
    );
    let r = v
        .build_request(
            ChatRequest {
                messages: vec![Message::text(Role::User, "hi")],
                ..ChatRequest::default()
            },
            &mut Losses::new(),
        )
        .unwrap();
    assert_eq!(r.headers()["authorization"], "Bearer ya29.test");

    let g = VertexAdapter::from_deployment(&dep(
        "gemini-3-pro",
        &[("accessToken", "t")],
        json!({"project": "p1"}),
    ))
    .unwrap();
    assert!(g.uri().to_string().starts_with(
        "https://aiplatform.googleapis.com/v1/projects/p1/locations/global/publishers/google/models/"
    ));

    // Eigene TokenSource ohne gespeicherte Zugangsdaten.
    let mut d = dep("gemini-2.5-pro", &[], json!({"project": "p1"}));
    d.credential = None;
    let t = VertexAdapter::with_token_source(&d, Arc::new(StaticToken(SecretString::new("wi"))))
        .unwrap();
    assert_eq!(t.api(), GeminiApi::Vertex);

    // Fehlende Angaben
    assert!(matches!(
        VertexAdapter::from_deployment(&dep("m", &[("accessToken", "t")], json!({}))),
        Err(ConfigError::Missing { .. })
    ));
    assert!(matches!(
        VertexAdapter::from_deployment(&dep("../x", &[("apiKey", "k")], json!({}))),
        Err(ConfigError::Invalid { .. })
    ));
    assert!(matches!(
        VertexAdapter::from_deployment(&dep("m", &[("apiKey", "k")], json!({"api": "foo"}))),
        Err(ConfigError::Invalid { .. })
    ));
}

#[test]
fn request_mapping_from_openai() {
    let body = br#"{
        "model": "x",
        "messages": [
            {"role": "system", "content": "Sei knapp."},
            {"role": "user", "content": [
                {"type": "text", "text": "Was ist das?"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}}
            ]},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_1", "type": "function", "function": {"name": "wetter", "arguments": "{\"ort\":\"Berlin\"}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_1", "content": "{\"temp\": 21}"},
            {"role": "user", "content": "Danke"}
        ],
        "tools": [{"type": "function", "function": {"name": "wetter",
                   "parameters": {"type": "object", "properties": {"ort": {"type": "string"}}, "additionalProperties": false}}}],
        "tool_choice": {"type": "function", "function": {"name": "wetter"}},
        "max_completion_tokens": 100, "temperature": 0.2, "seed": 7, "stop": "END",
        "response_format": {"type": "json_schema", "json_schema": {"name": "x", "schema": {"type": "object"}}},
        "reasoning_effort": "low"
    }"#;
    let req = openai::decode_request(body).unwrap();
    let mut losses = Losses::new();
    let v = gemini("gemini-3-pro-preview")
        .gemini_body(req, &mut losses)
        .unwrap();
    assert_eq!(
        v["systemInstruction"],
        json!({"parts": [{"text": "Sei knapp."}]})
    );
    let c = v["contents"].as_array().unwrap();
    assert_eq!(c.len(), 3, "{v:#}");
    assert_eq!(c[0]["role"], "user");
    assert_eq!(
        c[0]["parts"][1],
        json!({"inlineData": {"mimeType": "image/png", "data": "iVBORw0KGgo="}})
    );
    assert_eq!(c[1]["role"], "model");
    assert_eq!(
        c[1]["parts"][0],
        json!({"functionCall": {"name": "wetter", "args": {"ort": "Berlin"}}})
    );
    assert_eq!(
        c[2]["parts"][0],
        json!({"functionResponse": {"name": "wetter", "response": {"temp": 21}}})
    );
    assert_eq!(c[2]["parts"][1], json!({"text": "Danke"}));
    let g = &v["generationConfig"];
    assert_eq!(g["maxOutputTokens"], 100);
    assert_eq!(g["seed"], 7);
    assert_eq!(g["stopSequences"], json!(["END"]));
    assert_eq!(g["responseMimeType"], "application/json");
    assert_eq!(g["responseJsonSchema"], json!({"type": "object"}));
    assert_eq!(g["thinkingConfig"], json!({"thinkingLevel": "low"}));
    assert_eq!(
        v["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"]["additionalProperties"],
        false
    );
    assert_eq!(
        v["toolConfig"],
        json!({"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["wetter"]}})
    );
}

#[test]
fn thinking_mapping_for_gemini_2_and_anthropic_clients() {
    let req = ChatRequest {
        messages: vec![Message::text(Role::User, "x")],
        reasoning: Some(Reasoning {
            effort: Some("high".into()),
            thinking: None,
        }),
        ..ChatRequest::default()
    };
    let v = gemini("gemini-2.5-flash")
        .gemini_body(req, &mut Losses::new())
        .unwrap();
    assert_eq!(
        v["generationConfig"]["thinkingConfig"],
        json!({"thinkingBudget": 24576})
    );

    let req = ChatRequest {
        messages: vec![Message::text(Role::User, "x")],
        reasoning: Some(Reasoning {
            effort: None,
            thinking: Some(Thinking {
                mode: "enabled".into(),
                budget_tokens: Some(2000),
                display: None,
                extra: Map::new(),
            }),
        }),
        ..ChatRequest::default()
    };
    let v = gemini("gemini-2.5-flash")
        .gemini_body(req, &mut Losses::new())
        .unwrap();
    assert_eq!(
        v["generationConfig"]["thinkingConfig"],
        json!({"thinkingBudget": 2000, "includeThoughts": true})
    );
}

#[test]
fn tool_result_without_call_is_rejected() {
    let body = br#"{"model":"x","messages":[{"role":"tool","tool_call_id":"nope","content":"x"}]}"#;
    let req = openai::decode_request(body).unwrap();
    assert!(gemini("m").gemini_body(req, &mut Losses::new()).is_err());
}

#[test]
fn thought_signature_survives_roundtrip_via_tool_call_id() {
    let sig = "c2lnbmF0dXJlK2RhdGE/Pw=="; // enthält + / =
    let resp = json!({
        "candidates": [{"content": {"role": "model", "parts": [
            {"functionCall": {"name": "f", "args": {"a": 1}}, "thoughtSignature": sig}
        ]}, "finishReason": "STOP"}],
        "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 2}
    });
    let r = gemini("m")
        .parse_response(resp.to_string().as_bytes())
        .unwrap();
    assert_eq!(r.choices[0].stop_reason, Some(StopReason::ToolUse));
    let Content::Parts(parts) = &r.choices[0].message.content else {
        panic!()
    };
    let PartKind::ToolCall { id, .. } = &parts[0].kind else {
        panic!()
    };
    assert!(id.starts_with("call_") && id.contains(SIGNATURE_SEPARATOR));
    assert!(
        id.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
        "{id}"
    );

    // Der Client schickt die ID im nächsten Zug zurück (hier im Anthropic-Format).
    let next = json!({
        "model": "x", "max_tokens": 10,
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [{"type": "tool_use", "id": id, "name": "f", "input": {"a": 1}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": id, "content": "ok"}]}
        ]
    });
    let req = anthropic::decode_request(next.to_string().as_bytes()).unwrap();
    let v = gemini("m").gemini_body(req, &mut Losses::new()).unwrap();
    assert_eq!(v["contents"][1]["parts"][0]["thoughtSignature"], sig);
    assert_eq!(
        v["contents"][2]["parts"][0]["functionResponse"],
        json!({"name": "f", "response": {"result": "ok"}})
    );
}

#[test]
fn parses_response_with_thoughts_and_usage() {
    let resp = json!({
        "candidates": [{"content": {"role": "model", "parts": [
            {"text": "Ich überlege", "thought": true},
            {"text": "Antwort"}
        ]}, "finishReason": "STOP", "safetyRatings": []}],
        "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 4, "thoughtsTokenCount": 6,
                          "cachedContentTokenCount": 8, "totalTokenCount": 20},
        "modelVersion": "gemini-2.5-flash-001", "responseId": "resp-1"
    });
    let r = gemini("m")
        .parse_response(resp.to_string().as_bytes())
        .unwrap();
    assert_eq!(r.id, "resp-1");
    assert_eq!(r.model, "gemini-2.5-flash-001");
    let u = r.usage.unwrap();
    assert_eq!((u.input_tokens, u.output_tokens), (10, 10));
    assert_eq!(
        (u.cache_read_tokens, u.reasoning_tokens),
        (Some(8), Some(6))
    );
    assert_eq!(r.choices[0].message.content.text(), "Antwort");
    assert_eq!(r.choices[0].stop_reason, Some(StopReason::EndTurn));
}

#[test]
fn safety_blocks_are_content_filter_errors() {
    let blocked = json!({"promptFeedback": {"blockReason": "SAFETY"}, "usageMetadata": {"promptTokenCount": 5}});
    let e = gemini("m")
        .parse_response(blocked.to_string().as_bytes())
        .unwrap_err();
    assert_eq!(e.code.as_deref(), Some("content_filter"));

    let blocked = json!({"candidates": [{"finishReason": "PROHIBITED_CONTENT"}]});
    let e = gemini("m")
        .parse_response(blocked.to_string().as_bytes())
        .unwrap_err();
    assert_eq!(e.code.as_deref(), Some("content_filter"));

    // Teilantwort, dann blockiert → Endgrund content_filter.
    let partial =
        json!({"candidates": [{"content": {"parts": [{"text": "a"}]}, "finishReason": "SAFETY"}]});
    let r = gemini("m")
        .parse_response(partial.to_string().as_bytes())
        .unwrap();
    assert_eq!(r.choices[0].stop_reason, Some(StopReason::ContentFilter));
}

fn classify(status: u16, body: Value) -> UpstreamError {
    classify_gemini_error(
        StatusCode::from_u16(status).unwrap(),
        &HeaderMap::new(),
        body.to_string().as_bytes(),
    )
}

#[test]
fn error_classes() {
    let e = classify(
        429,
        json!({"error": {"code": 429, "message": "Resource has been exhausted (e.g. check quota).", "status": "RESOURCE_EXHAUSTED",
               "details": [{"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "37s"}]}}),
    );
    assert_eq!(e.class, ErrorClass::RateLimit);
    assert!(e.retryable);
    assert_eq!(e.retry_after, Some(std::time::Duration::from_secs(37)));

    let e = classify(
        429,
        json!({"error": {"code": 429, "message": "You exceeded your current quota", "status": "RESOURCE_EXHAUSTED",
               "details": [{"@type": "type.googleapis.com/google.rpc.QuotaFailure",
                            "violations": [{"quotaId": "GenerateRequestsPerDayPerProjectPerModel-FreeTier"}]}]}}),
    );
    assert_eq!(e.class, ErrorClass::Quota);
    assert!(!e.retryable);

    let e = classify(
        400,
        json!({"error": {"code": 400, "message": "The input token count (1200000) exceeds the maximum number of tokens allowed (1048576).", "status": "INVALID_ARGUMENT"}}),
    );
    assert_eq!(e.class, ErrorClass::ContextLength);

    let e = classify(
        400,
        json!({"error": {"code": 400, "message": "API key not valid. Please pass a valid API key.", "status": "INVALID_ARGUMENT",
               "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "API_KEY_INVALID"}]}}),
    );
    assert_eq!(e.class, ErrorClass::Auth);
    assert_eq!(e.error.status(), 502);

    let e = classify(
        403,
        json!({"error": {"code": 403, "message": "Permission denied on resource project p1", "status": "PERMISSION_DENIED"}}),
    );
    assert_eq!(e.class, ErrorClass::Auth);
    let e = classify(
        404,
        json!({"error": {"code": 404, "message": "models/x is not found", "status": "NOT_FOUND"}}),
    );
    assert_eq!(e.class, ErrorClass::NotFound);
    let e = classify(
        503,
        json!({"error": {"code": 503, "message": "The model is overloaded.", "status": "UNAVAILABLE"}}),
    );
    assert_eq!(e.class, ErrorClass::Overloaded);
    let e = classify(
        400,
        json!([{"error": {"code": 400, "message": "bad", "status": "INVALID_ARGUMENT"}}]),
    );
    assert_eq!(e.class, ErrorClass::InvalidRequest);
    assert_eq!(e.error.message, "bad");
}

fn sse(chunks: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    for c in chunks {
        out.extend_from_slice(format!("data: {c}\r\n\r\n").as_bytes());
    }
    out
}

fn decode(bytes: &[u8], step: usize) -> Vec<StreamEvent> {
    let mut d = GeminiStreamDecoder::new("m".into());
    let mut out = Vec::new();
    for c in bytes.chunks(step) {
        d.push(c, &mut out);
    }
    d.finish(&mut out);
    out
}

#[test]
fn stream_events() {
    let s = sse(&[
        json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "denke", "thought": true}]}}], "responseId": "r1", "modelVersion": "gemini-x"}),
        json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "Hal"}]}}]}),
        json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "lo"}]}}]}),
        json!({"candidates": [{"content": {"role": "model", "parts": [{"functionCall": {"name": "f", "args": {"a": 1}}}]},
                               "finishReason": "STOP"}],
               "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 3, "totalTokenCount": 8}}),
    ]);
    for step in [1, 9, s.len()] {
        let ev = decode(&s, step);
        assert!(
            matches!(&ev[0], StreamEvent::Start { id, model, .. } if id == "r1" && model == "gemini-x")
        );
        assert_eq!(
            ev[1],
            StreamEvent::ReasoningDelta {
                index: 0,
                text: "denke".into()
            }
        );
        assert_eq!(
            ev[2],
            StreamEvent::TextDelta {
                index: 1,
                text: "Hal".into()
            }
        );
        assert_eq!(
            ev[3],
            StreamEvent::TextDelta {
                index: 1,
                text: "lo".into()
            }
        );
        assert!(matches!(&ev[4], StreamEvent::ToolCallStart(t) if t.index == 2 && t.name == "f"));
        assert_eq!(
            ev[5],
            StreamEvent::ToolCallDelta {
                index: 2,
                arguments: r#"{"a":1}"#.into()
            }
        );
        assert_eq!(
            ev[6],
            StreamEvent::Stop {
                reason: StopReason::ToolUse,
                stop_sequence: None
            }
        );
        assert!(
            matches!(&ev[7], StreamEvent::Usage(u) if u.input_tokens == 5 && u.output_tokens == 3)
        );
        assert_eq!(ev.len(), 8, "Schrittweite {step}");
    }
}

#[test]
fn stream_blocks_errors_and_truncation() {
    let ev = decode(
        &sse(&[json!({"promptFeedback": {"blockReason": "SAFETY"}})]),
        5,
    );
    assert!(
        matches!(ev.last(), Some(StreamEvent::Error(e)) if e.code.as_deref() == Some("content_filter"))
    );

    let ev = decode(
        &sse(&[json!({"candidates": [{"finishReason": "SAFETY"}]})]),
        5,
    );
    assert!(
        matches!(ev.last(), Some(StreamEvent::Error(e)) if e.code.as_deref() == Some("content_filter"))
    );

    let ev = decode(
        &sse(&[
            json!({"candidates": [{"content": {"parts": [{"text": "a"}]}}]}),
            json!({"candidates": [{"finishReason": "SAFETY"}]}),
        ]),
        5,
    );
    assert!(matches!(
        ev.last(),
        Some(StreamEvent::Stop {
            reason: StopReason::ContentFilter,
            ..
        })
    ));

    let ev = decode(
        &sse(&[
            json!({"candidates": [{"content": {"parts": [{"text": "a"}]}}]}),
            json!({"error": {"code": 503, "message": "overloaded", "status": "UNAVAILABLE"}}),
        ]),
        5,
    );
    assert!(matches!(ev.last(), Some(StreamEvent::Error(e)) if e.status() == 503));

    let ev = decode(
        &sse(&[json!({"candidates": [{"content": {"parts": [{"text": "a"}]}}]})]),
        5,
    );
    assert!(
        matches!(ev.last(), Some(StreamEvent::Error(e)) if e.code.as_deref() == Some("upstream_stream_truncated"))
    );
}
