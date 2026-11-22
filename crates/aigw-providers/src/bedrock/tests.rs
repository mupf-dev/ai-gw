// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;

use aigw_core::ir::{FunctionTool, Reasoning, Thinking, Tool};
use aigw_core::resource::{
    CredentialData, Deployment, DeploymentSpec, JsonObject, ObjectMeta, new_id,
};
use aigw_core::wire::openai;

use super::eventstream::{HeaderValue as EsValue, Message as EsMessage};
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
            metadata: ObjectMeta::named("bedrock-1"),
            spec: DeploymentSpec {
                model_group: "g".into(),
                provider: ProviderKind::Bedrock,
                endpoint: None,
                provider_model: model.into(),
                credential: Some("aws".into()),
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

const CREDS: &[(&str, &str)] = &[
    ("accessKeyId", "AKIDEXAMPLE"),
    (
        "secretAccessKey",
        "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
    ),
];

fn adapter(model: &str) -> BedrockAdapter {
    BedrockAdapter::from_deployment(&dep(model, CREDS, json!({"region": "eu-central-1"}))).unwrap()
}

#[test]
fn config_requires_region_and_credentials() {
    assert!(matches!(
        BedrockAdapter::from_deployment(&dep("m", CREDS, json!({}))),
        Err(ConfigError::Missing { .. })
    ));
    assert!(matches!(
        BedrockAdapter::from_deployment(&dep("m", &[], json!({"region": "us-east-1"}))),
        Err(ConfigError::Missing { .. })
    ));
    assert!(matches!(
        BedrockAdapter::from_deployment(&dep("m", CREDS, json!({"region": "us east"}))),
        Err(ConfigError::Invalid { .. })
    ));
    let a = adapter("anthropic.claude-3-haiku-20240307-v1:0");
    assert_eq!(
        a.uri().to_string(),
        "https://bedrock-runtime.eu-central-1.amazonaws.com/model/anthropic.claude-3-haiku-20240307-v1%3A0/converse"
    );
    assert!(a.stream_uri().path().ends_with("/converse-stream"));
    // ARN als Modell-ID: ein Pfadsegment.
    let a = adapter("arn:aws:bedrock:eu-central-1:123:inference-profile/eu.anthropic.x");
    assert!(
        a.uri().path().contains(
            "arn%3Aaws%3Abedrock%3Aeu-central-1%3A123%3Ainference-profile%2Feu.anthropic.x"
        )
    );
}

#[test]
fn request_is_signed() {
    let a = BedrockAdapter::from_deployment(&dep(
        "anthropic.claude-v2:1",
        &[
            ("accessKeyId", "AKID"),
            ("secretAccessKey", "secret"),
            ("sessionToken", "tok"),
        ],
        json!({"region": "us-east-1"}),
    ))
    .unwrap();
    let req = ChatRequest {
        messages: vec![Message::text(Role::User, "hi")],
        stream: Some(true),
        ..ChatRequest::default()
    };
    let r = a.build_request(req, &mut Losses::new()).unwrap();
    let auth = r.headers()["authorization"].to_str().unwrap();
    assert!(
        auth.starts_with("AWS4-HMAC-SHA256 Credential=AKID/"),
        "{auth}"
    );
    assert!(auth.contains("/us-east-1/bedrock/aws4_request"));
    assert!(
        auth.contains("SignedHeaders=accept;content-type;host;x-amz-date;x-amz-security-token")
    );
    assert_eq!(r.headers()["x-amz-security-token"], "tok");
    assert_eq!(r.headers()["accept"], "application/vnd.amazon.eventstream");
    assert!(
        r.uri()
            .path()
            .ends_with("anthropic.claude-v2%3A1/converse-stream")
    );
}

#[test]
fn bearer_api_key() {
    let a = BedrockAdapter::from_deployment(&dep(
        "m",
        &[("apiKey", "bedrock-api-key")],
        json!({"region": "us-east-1"}),
    ))
    .unwrap();
    let r = a
        .build_request(
            ChatRequest {
                messages: vec![Message::text(Role::User, "hi")],
                ..ChatRequest::default()
            },
            &mut Losses::new(),
        )
        .unwrap();
    assert_eq!(r.headers()["authorization"], "Bearer bedrock-api-key");
    assert!(r.headers().get("x-amz-date").is_none());
}

#[test]
fn converse_mapping_from_openai_request() {
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
            {"role": "tool", "tool_call_id": "call_1", "content": "sonnig"},
            {"role": "user", "content": "Danke"}
        ],
        "tools": [{"type": "function", "function": {"name": "wetter", "description": "Wetter",
                   "parameters": {"type": "object", "properties": {"ort": {"type": "string"}}}}}],
        "tool_choice": "required",
        "max_tokens": 100, "temperature": 0.2, "top_p": 0.9, "stop": ["END"], "seed": 1
    }"#;
    let req = openai::decode_request(body).unwrap();
    let mut losses = Losses::new();
    let v = adapter("anthropic.claude")
        .converse_body(req, &mut losses)
        .unwrap();
    assert_eq!(v["system"], json!([{"text": "Sei knapp."}]));
    let msgs = v["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3, "{v:#}");
    assert_eq!(msgs[0]["content"][1]["image"]["format"], "png");
    assert_eq!(
        msgs[0]["content"][1]["image"]["source"]["bytes"],
        "iVBORw0KGgo="
    );
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(
        msgs[1]["content"][0]["toolUse"]["input"],
        json!({"ort": "Berlin"})
    );
    // Tool-Ergebnis und folgender Nutzertext in einer Nutzernachricht.
    assert_eq!(msgs[2]["role"], "user");
    assert_eq!(msgs[2]["content"][0]["toolResult"]["toolUseId"], "call_1");
    assert_eq!(
        msgs[2]["content"][0]["toolResult"]["content"][0]["text"],
        "sonnig"
    );
    assert_eq!(msgs[2]["content"][1]["text"], "Danke");
    assert_eq!(
        v["inferenceConfig"],
        json!({"maxTokens": 100, "temperature": 0.2, "topP": 0.9, "stopSequences": ["END"]})
    );
    assert_eq!(v["toolConfig"]["toolChoice"], json!({"any": {}}));
    assert_eq!(
        v["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"]["json"]["properties"]["ort"]["type"],
        "string"
    );
    assert!(losses.items().iter().any(|l| l.contains("seed")));
}

#[test]
fn cache_control_thinking_and_additional_fields() {
    let mut text = Part::text("langer Kontext");
    text.cache_control = Some(json!({"type": "ephemeral"}));
    let req = ChatRequest {
        messages: vec![Message {
            role: Role::User,
            content: Content::Parts(vec![text]),
            name: None,
            extra: Map::new(),
        }],
        tools: vec![Tool {
            kind: ToolKind::Function(FunctionTool {
                name: "f".into(),
                description: None,
                parameters: None,
                strict: None,
                extra: Map::new(),
            }),
            cache_control: Some(json!({"type": "ephemeral"})),
            extra: Map::new(),
        }],
        sampling: aigw_core::ir::Sampling {
            top_k: Some(5),
            ..Default::default()
        },
        reasoning: Some(Reasoning {
            effort: None,
            thinking: Some(Thinking {
                mode: "enabled".into(),
                budget_tokens: Some(2048),
                display: None,
                extra: Map::new(),
            }),
        }),
        ..ChatRequest::default()
    };
    let a = BedrockAdapter::from_deployment(&dep(
        "us.anthropic.claude-sonnet",
        CREDS,
        json!({"region": "us-east-1", "additionalModelRequestFields": {"anthropic_beta": ["x"]},
               "guardrailConfig": {"guardrailIdentifier": "g1", "guardrailVersion": "1"}}),
    ))
    .unwrap();
    let v = a.converse_body(req, &mut Losses::new()).unwrap();
    assert_eq!(
        v["messages"][0]["content"][1],
        json!({"cachePoint": {"type": "default"}})
    );
    assert_eq!(
        v["toolConfig"]["tools"][1],
        json!({"cachePoint": {"type": "default"}})
    );
    assert_eq!(
        v["additionalModelRequestFields"],
        json!({"top_k": 5, "thinking": {"type": "enabled", "budget_tokens": 2048}, "anthropic_beta": ["x"]})
    );
    assert_eq!(v["guardrailConfig"]["guardrailIdentifier"], "g1");
}

#[test]
fn tool_choice_none_drops_tools_without_history() {
    let req = ChatRequest {
        messages: vec![Message::text(Role::User, "hi")],
        tools: vec![Tool {
            kind: ToolKind::Function(FunctionTool {
                name: "f".into(),
                description: None,
                parameters: None,
                strict: None,
                extra: Map::new(),
            }),
            cache_control: None,
            extra: Map::new(),
        }],
        tool_choice: Some(ToolChoice::None),
        ..ChatRequest::default()
    };
    let v = adapter("m").converse_body(req, &mut Losses::new()).unwrap();
    assert!(v.get("toolConfig").is_none());
}

#[test]
fn image_url_is_rejected() {
    let req = ChatRequest {
        messages: vec![Message {
            role: Role::User,
            content: Content::Parts(vec![Part::new(PartKind::Image {
                source: MediaSource::Url {
                    url: "https://example.com/a.png".into(),
                },
                detail: None,
            })]),
            name: None,
            extra: Map::new(),
        }],
        ..ChatRequest::default()
    };
    let e = adapter("m")
        .converse_body(req, &mut Losses::new())
        .unwrap_err();
    assert_eq!(e.kind, aigw_core::ir::ErrorKind::InvalidRequest);
}

#[test]
fn parses_converse_response() {
    let body = json!({
        "output": {"message": {"role": "assistant", "content": [
            {"reasoningContent": {"reasoningText": {"text": "denke", "signature": "sig"}}},
            {"text": "Ich frage nach."},
            {"toolUse": {"toolUseId": "tu_1", "name": "wetter", "input": {"ort": "Berlin"}}}
        ]}},
        "stopReason": "tool_use",
        "usage": {"inputTokens": 10, "outputTokens": 5, "totalTokens": 15, "cacheReadInputTokens": 100, "cacheWriteInputTokens": 3},
        "metrics": {"latencyMs": 42}
    });
    let r = adapter("m")
        .parse_response(body.to_string().as_bytes())
        .unwrap();
    let c = &r.choices[0];
    assert_eq!(c.stop_reason, Some(StopReason::ToolUse));
    let Content::Parts(parts) = &c.message.content else {
        panic!()
    };
    assert!(matches!(&parts[0].kind, PartKind::Reasoning { signature: Some(s), .. } if s == "sig"));
    assert!(
        matches!(&parts[2].kind, PartKind::ToolCall { id, arguments, .. } if id == "tu_1" && arguments == r#"{"ort":"Berlin"}"#)
    );
    let u = r.usage.unwrap();
    assert_eq!((u.input_tokens, u.output_tokens), (113, 5));
    assert_eq!(
        (u.cache_read_tokens, u.cache_write_tokens),
        (Some(100), Some(3))
    );
    assert!(r.id.starts_with("msg_bdrk_"));
    assert_eq!(r.extra["metrics"]["latencyMs"], 42);

    let simple = json!({"output": {"message": {"role": "assistant", "content": [{"text": "Hallo"}]}},
                        "stopReason": "end_turn", "usage": {"inputTokens": 1, "outputTokens": 1}});
    let r = adapter("m")
        .parse_response(simple.to_string().as_bytes())
        .unwrap();
    assert_eq!(r.choices[0].message.content, Content::Text("Hallo".into()));
}

fn classify(status: u16, ty: Option<&str>, body: Value) -> UpstreamError {
    let mut h = HeaderMap::new();
    if let Some(t) = ty {
        h.insert("x-amzn-errortype", t.parse().unwrap());
    }
    classify_bedrock_error(
        StatusCode::from_u16(status).unwrap(),
        &h,
        body.to_string().as_bytes(),
    )
}

#[test]
fn error_classes() {
    let e = classify(
        429,
        Some("ThrottlingException:http://internal.amazon.com/coral/com.amazon.bedrock/"),
        json!({"message": "Too many requests, please wait before trying again."}),
    );
    assert_eq!(e.class, ErrorClass::RateLimit);
    assert!(e.retryable);
    assert_eq!(e.error.code.as_deref(), Some("ThrottlingException"));

    let e = classify(
        429,
        Some("ThrottlingException"),
        json!({"message": "Too many tokens per day, please wait before trying again."}),
    );
    assert_eq!(e.class, ErrorClass::Quota);
    assert!(!e.retryable);

    let e = classify(
        400,
        None,
        json!({"__type": "com.amazon.coral.validate#ValidationException",
               "message": "Input is too long for requested model."}),
    );
    assert_eq!(e.class, ErrorClass::ContextLength);
    assert_eq!(e.error.code.as_deref(), Some("context_length_exceeded"));

    let e = classify(
        400,
        Some("ValidationException"),
        json!({"message": "Malformed input request"}),
    );
    assert_eq!(e.class, ErrorClass::InvalidRequest);
    assert_eq!(e.error.status(), 400);

    let e = classify(
        403,
        Some("AccessDeniedException"),
        json!({"Message": "User arn:aws:iam::123:user/x is not authorized"}),
    );
    assert_eq!(e.class, ErrorClass::Auth);
    assert_eq!(e.error.status(), 502);
    assert!(!e.error.message.contains("arn:aws"));

    let e = classify(
        400,
        Some("ExpiredTokenException"),
        json!({"message": "expired"}),
    );
    assert_eq!(e.class, ErrorClass::Auth);

    let e = classify(
        503,
        Some("ServiceUnavailableException"),
        json!({"message": "x"}),
    );
    assert_eq!(e.class, ErrorClass::Overloaded);
    assert!(e.retryable);

    let e = classify(424, Some("ModelErrorException"), json!({"message": "x"}));
    assert_eq!(e.class, ErrorClass::ServerError);
    assert!(e.retryable);

    let e = classify(
        404,
        Some("ResourceNotFoundException"),
        json!({"message": "x"}),
    );
    assert_eq!(e.class, ErrorClass::NotFound);
}

fn event(ty: &str, payload: Value) -> Vec<u8> {
    EsMessage {
        headers: vec![
            (":event-type".into(), EsValue::String(ty.into())),
            (
                ":content-type".into(),
                EsValue::String("application/json".into()),
            ),
            (":message-type".into(), EsValue::String("event".into())),
        ],
        payload: payload.to_string().into_bytes(),
    }
    .encode()
}

fn exception(ty: &str, message: &str) -> Vec<u8> {
    EsMessage {
        headers: vec![
            (":exception-type".into(), EsValue::String(ty.into())),
            (
                ":content-type".into(),
                EsValue::String("application/json".into()),
            ),
            (":message-type".into(), EsValue::String("exception".into())),
        ],
        payload: json!({ "message": message }).to_string().into_bytes(),
    }
    .encode()
}

fn decode_all(bytes: &[u8], step: usize) -> Vec<StreamEvent> {
    let mut d = BedrockStreamDecoder::new("m".into());
    let mut out = Vec::new();
    for c in bytes.chunks(step) {
        d.push(c, &mut out);
    }
    d.finish(&mut out);
    out
}

#[test]
fn stream_events() {
    let mut s = Vec::new();
    s.extend(event(
        "messageStart",
        json!({"role": "assistant", "p": "abc"}),
    ));
    s.extend(event(
        "contentBlockDelta",
        json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"text": "hm"}}}),
    ));
    s.extend(event(
        "contentBlockDelta",
        json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"signature": "sig"}}}),
    ));
    s.extend(event(
        "contentBlockDelta",
        json!({"contentBlockIndex": 1, "delta": {"text": "Hal"}}),
    ));
    s.extend(event(
        "contentBlockDelta",
        json!({"contentBlockIndex": 1, "delta": {"text": "lo"}}),
    ));
    s.extend(event("contentBlockStop", json!({"contentBlockIndex": 1})));
    s.extend(event(
        "contentBlockStart",
        json!({"contentBlockIndex": 2, "start": {"toolUse": {"toolUseId": "tu_1", "name": "f"}}}),
    ));
    s.extend(event(
        "contentBlockDelta",
        json!({"contentBlockIndex": 2, "delta": {"toolUse": {"input": "{\"a\":"}}}),
    ));
    s.extend(event(
        "contentBlockDelta",
        json!({"contentBlockIndex": 2, "delta": {"toolUse": {"input": "1}"}}}),
    ));
    s.extend(event("messageStop", json!({"stopReason": "tool_use"})));
    s.extend(event(
        "metadata",
        json!({"usage": {"inputTokens": 7, "outputTokens": 3, "totalTokens": 10}, "metrics": {"latencyMs": 1}}),
    ));
    for step in [1, 5, 64, s.len()] {
        let ev = decode_all(&s, step);
        assert!(matches!(&ev[0], StreamEvent::Start { model, .. } if model == "m"));
        assert_eq!(
            ev[1],
            StreamEvent::ReasoningDelta {
                index: 0,
                text: "hm".into()
            }
        );
        assert_eq!(
            ev[2],
            StreamEvent::ReasoningSignature {
                index: 0,
                signature: "sig".into()
            }
        );
        assert_eq!(
            ev[3],
            StreamEvent::TextDelta {
                index: 1,
                text: "Hal".into()
            }
        );
        assert!(matches!(&ev[5], StreamEvent::ToolCallStart(t) if t.id == "tu_1" && t.index == 2));
        assert_eq!(
            ev[7],
            StreamEvent::ToolCallDelta {
                index: 2,
                arguments: "1}".into()
            }
        );
        assert_eq!(
            ev[8],
            StreamEvent::Stop {
                reason: StopReason::ToolUse,
                stop_sequence: None
            }
        );
        assert!(
            matches!(&ev[9], StreamEvent::Usage(u) if u.input_tokens == 7 && u.output_tokens == 3)
        );
        assert_eq!(ev.len(), 10, "Schrittweite {step}");
    }
}

#[test]
fn stream_exception_and_truncation() {
    let mut s = Vec::new();
    s.extend(event("messageStart", json!({"role": "assistant"})));
    s.extend(event(
        "contentBlockDelta",
        json!({"contentBlockIndex": 0, "delta": {"text": "a"}}),
    ));
    s.extend(exception(
        "modelStreamErrorException",
        "Model produced invalid sequence",
    ));
    s.extend(event(
        "contentBlockDelta",
        json!({"contentBlockIndex": 0, "delta": {"text": "b"}}),
    ));
    let ev = decode_all(&s, 7);
    let StreamEvent::Error(e) = ev.last().unwrap() else {
        panic!("{ev:?}")
    };
    assert_eq!(e.code.as_deref(), Some("modelStreamErrorException"));
    assert_eq!(e.status(), 502);
    assert_eq!(ev.len(), 3);

    let ev = decode_all(&exception("throttlingException", "slow down"), 3);
    assert!(matches!(&ev[0], StreamEvent::Error(e) if e.status() == 429));

    let ev = decode_all(
        &exception(
            "validationException",
            "Input is too long for requested model.",
        ),
        100,
    );
    assert!(
        matches!(&ev[0], StreamEvent::Error(e) if e.code.as_deref() == Some("context_length_exceeded"))
    );

    // Ende ohne messageStop
    let ev = decode_all(&event("messageStart", json!({})), 100);
    assert!(
        matches!(ev.last(), Some(StreamEvent::Error(e)) if e.code.as_deref() == Some("upstream_stream_truncated"))
    );

    // Kaputte Prüfsumme
    let mut bad = event("messageStart", json!({}));
    let n = bad.len();
    bad[n - 1] ^= 0xff;
    let ev = decode_all(&bad, 100);
    assert!(
        matches!(&ev[0], StreamEvent::Error(e) if e.code.as_deref() == Some("upstream_protocol_error"))
    );
}
