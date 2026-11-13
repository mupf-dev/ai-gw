// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Roundtrip- und Kreuzkonvertierungstests der Wire-Formate über die IR.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use aigw_core::ir::{Content, PartKind, Role, StopReason, StreamEvent, ToolCallStart, Usage};
use aigw_core::wire::sse::SseParser;
use aigw_core::wire::{Losses, anthropic, openai};
use serde_json::{Value, json};

fn fixture(path: &str) -> Vec<u8> {
    let full = format!("{}/tests/fixtures/{path}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&full).unwrap_or_else(|e| panic!("{full}: {e}"))
}

fn as_json(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap()
}

// ---------------------------------------------------------------------------------------------
// Verlustfreie Roundtrips (gleiches Format)
// ---------------------------------------------------------------------------------------------

fn openai_request_roundtrip(path: &str) {
    let input = fixture(path);
    let ir = openai::decode_request(&input).unwrap();
    let mut losses = Losses::new();
    let out = openai::encode_request(ir, &mut losses).unwrap();
    assert!(losses.is_empty(), "{path}: Verluste {:?}", losses.items());
    assert_eq!(as_json(&out), as_json(&input), "{path}");
}

#[test]
fn openai_request_full_roundtrip_is_lossless() {
    openai_request_roundtrip("openai/request_full.json");
}

#[test]
fn openai_request_legacy_roundtrip_is_lossless() {
    openai_request_roundtrip("openai/request_legacy.json");
}

#[test]
fn openai_response_roundtrips_are_lossless() {
    for path in [
        "openai/response_tool_calls.json",
        "openai/response_text.json",
    ] {
        let input = fixture(path);
        let ir = openai::decode_response(&input).unwrap();
        let mut losses = Losses::new();
        let out = openai::encode_response(ir, &mut losses).unwrap();
        assert!(losses.is_empty(), "{path}: Verluste {:?}", losses.items());
        assert_eq!(as_json(&out), as_json(&input), "{path}");
    }
}

#[test]
fn anthropic_request_roundtrip_is_lossless() {
    let input = fixture("anthropic/request_full.json");
    let ir = anthropic::decode_request(&input).unwrap();
    let mut losses = Losses::new();
    let out = anthropic::encode_request(ir, &mut losses).unwrap();
    assert!(losses.is_empty(), "Verluste {:?}", losses.items());
    assert_eq!(as_json(&out), as_json(&input));
}

#[test]
fn anthropic_response_roundtrip_is_lossless() {
    let input = fixture("anthropic/response_tool_use.json");
    let ir = anthropic::decode_response(&input).unwrap();
    let usage = ir.usage.clone().unwrap();
    // IR-Semantik: Eingabe inklusive Cache-Lesen und -Schreiben.
    assert_eq!(usage.input_tokens, 125);
    assert_eq!(usage.cache_read_tokens, Some(100));
    assert_eq!(usage.cache_write_tokens, Some(5));
    let mut losses = Losses::new();
    let out = anthropic::encode_response(ir, &mut losses).unwrap();
    assert!(losses.is_empty(), "Verluste {:?}", losses.items());
    assert_eq!(as_json(&out), as_json(&input));
}

// ---------------------------------------------------------------------------------------------
// Kreuzkonvertierung (dokumentierte Verluste)
// ---------------------------------------------------------------------------------------------

#[test]
fn openai_request_to_anthropic() {
    let ir = openai::decode_request(&fixture("openai/request_full.json")).unwrap();
    let mut losses = Losses::new();
    let out = as_json(&anthropic::encode_request(ir, &mut losses).unwrap());

    // System + Developer → `system` als Blockliste.
    assert_eq!(
        out["system"],
        json!([
            {"type": "text", "text": "Du bist ein hilfreicher Assistent."},
            {"type": "text", "text": "Antworte knapp."}
        ])
    );
    let msgs = out["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 5);
    // Nutzerinhalte: Text, Bild (base64 + URL), PDF; Audio und unbekannter Teil entfallen.
    let user = msgs[0]["content"].as_array().unwrap();
    let types: Vec<&str> = user.iter().map(|b| b["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["text", "image", "image", "document"]);
    assert_eq!(
        user[1]["source"],
        json!({"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="})
    );
    assert_eq!(user[3]["source"]["media_type"], json!("application/pdf"));
    assert_eq!(user[3]["title"], json!("doc.pdf"));
    // Tool-Aufrufe → tool_use mit geparstem input.
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
    assert_eq!(msgs[1]["content"][0]["input"], json!({"city": "Berlin"}));
    // Zwei Tool-Nachrichten → eine Nutzernachricht mit zwei tool_result-Blöcken.
    assert_eq!(msgs[2]["role"], "user");
    assert_eq!(msgs[2]["content"].as_array().unwrap().len(), 2);
    assert_eq!(msgs[2]["content"][1]["tool_use_id"], "call_2");
    assert_eq!(out["max_tokens"], 512);
    assert_eq!(out["stop_sequences"], json!(["\n\n", "ENDE"]));
    assert_eq!(out["output_config"]["effort"], "medium");
    assert_eq!(out["output_config"]["format"]["type"], "json_schema");
    assert_eq!(out["metadata"], json!({"user_id": "user-42"}));
    assert_eq!(out["tool_choice"], json!({"type": "auto"}));
    assert_eq!(out["tools"][0]["input_schema"]["required"], json!(["city"]));
    assert!(out.get("n").is_none() && out.get("seed").is_none());

    let l = losses.items().join("\n");
    for expected in ["Audio", "unbekannte Felder", "seed", "detail", "metadata"] {
        assert!(l.contains(expected), "Verlust '{expected}' fehlt in:\n{l}");
    }
}

#[test]
fn openai_legacy_request_to_anthropic() {
    let ir = openai::decode_request(&fixture("openai/request_legacy.json")).unwrap();
    let mut losses = Losses::new();
    let out = as_json(&anthropic::encode_request(ir, &mut losses).unwrap());
    assert_eq!(out["max_tokens"], 100);
    assert_eq!(
        out["tool_choice"],
        json!({"type": "tool", "name": "get_weather", "disable_parallel_tool_use": true})
    );
    assert_eq!(out["stop_sequences"], json!(["ENDE"]));
    assert_eq!(
        out["output_config"]["format"],
        json!({"type": "json_schema", "schema": {"type": "object"}})
    );
    // Assistant: Text + tool_use; Ablehnung → Text.
    assert_eq!(out["messages"][1]["content"][0]["text"], "Mache ich.");
    assert_eq!(out["messages"][1]["content"][1]["type"], "tool_use");
    assert_eq!(
        out["messages"][2]["content"][0],
        json!({"type": "tool_result", "tool_use_id": "c1", "content": [{"type": "text", "text": "9 °C"}]})
    );
    assert_eq!(
        out["messages"][3]["content"][0]["text"],
        "Das kann ich nicht."
    );
}

#[test]
fn anthropic_request_to_openai() {
    let ir = anthropic::decode_request(&fixture("anthropic/request_full.json")).unwrap();
    let mut losses = Losses::new();
    let out = as_json(&openai::encode_request(ir, &mut losses).unwrap());

    let msgs = out["messages"].as_array().unwrap();
    let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(
        roles,
        [
            "system",
            "user",
            "assistant",
            "tool",
            "user",
            "assistant",
            "tool",
            "user"
        ]
    );
    assert_eq!(
        msgs[0]["content"],
        json!([{"type": "text", "text": "Du bist hilfreich."}])
    );
    assert_eq!(
        msgs[1]["content"][0]["image_url"]["url"],
        "data:image/png;base64,iVBORw0KGgo="
    );
    // Thinking entfällt, Text + Tool-Aufruf bleiben.
    assert_eq!(msgs[2]["content"], "Ich prüfe das Wetter.");
    let args: Value = serde_json::from_str(
        msgs[2]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(args, json!({"city": "Berlin", "units": {"temp": "C"}}));
    assert_eq!(msgs[3]["tool_call_id"], "toolu_1");
    assert_eq!(
        msgs[3]["content"],
        json!([{"type": "text", "text": "12 °C"}])
    );
    assert_eq!(
        msgs[4]["content"],
        json!([{"type": "text", "text": "Danke"}])
    );
    // Text-Dokument → Textteil, PDF → file, search_result entfällt.
    // Tool-Ergebnisse stehen vor dem restlichen Nutzerinhalt (OpenAI-Reihenfolge).
    assert_eq!(msgs[6]["content"], "Fehler!");
    let last_user = msgs[7]["content"].as_array().unwrap();
    assert_eq!(last_user[0], json!({"type": "text", "text": "Hallo Welt"}));
    assert_eq!(last_user[1]["type"], "file");
    assert_eq!(last_user.len(), 2);

    assert_eq!(out["max_completion_tokens"], 1024);
    assert_eq!(out["parallel_tool_calls"], false);
    assert_eq!(out["tool_choice"], "auto");
    assert_eq!(out["reasoning_effort"], "high");
    assert_eq!(out["user"], "user-42");
    assert_eq!(out["stop"], json!(["\n\nHuman:"]));
    assert_eq!(out["response_format"]["json_schema"]["name"], "response");
    assert_eq!(out["tools"].as_array().unwrap().len(), 1);
    assert!(out.get("top_k").is_none() && out.get("service_tier").is_none());

    let l = losses.items().join("\n");
    for expected in [
        "Reasoning",
        "cache_control",
        "top_k",
        "thinking",
        "is_error",
        "anbieterspezifisches Tool",
        "unbekannter Inhaltsteil",
    ] {
        assert!(l.contains(expected), "Verlust '{expected}' fehlt in:\n{l}");
    }
}

#[test]
fn anthropic_response_to_openai_and_back() {
    let ir = anthropic::decode_response(&fixture("anthropic/response_tool_use.json")).unwrap();
    let mut losses = Losses::new();
    let oa = openai::encode_response(ir, &mut losses).unwrap();
    let v = as_json(&oa);
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        v["choices"][0]["message"]["content"],
        "Ich rufe das Tool auf."
    );
    assert_eq!(v["usage"]["prompt_tokens"], 125);
    assert_eq!(v["usage"]["prompt_tokens_details"]["cached_tokens"], 100);
    assert_eq!(v["usage"]["total_tokens"], 165);

    let ir2 = openai::decode_response(&oa).unwrap();
    let mut losses2 = Losses::new();
    let back = as_json(&anthropic::encode_response(ir2, &mut losses2).unwrap());
    assert_eq!(back["stop_reason"], "tool_use");
    assert_eq!(back["content"][1]["input"], json!({"city": "Berlin"}));
    // Cache-Schreiben ist in OpenAI nicht darstellbar: 125 Eingabe, davon 100 gelesen.
    assert_eq!(back["usage"]["input_tokens"], 25);
    assert_eq!(back["usage"]["cache_read_input_tokens"], 100);
}

#[test]
fn openai_response_to_anthropic() {
    let ir = openai::decode_response(&fixture("openai/response_tool_calls.json")).unwrap();
    let mut losses = Losses::new();
    let v = as_json(&anthropic::encode_response(ir, &mut losses).unwrap());
    assert_eq!(v["type"], "message");
    assert_eq!(
        v["content"],
        json!([{"type": "tool_use", "id": "call_1", "name": "get_weather", "input": {"city": "Berlin"}}])
    );
    assert_eq!(v["stop_reason"], "tool_use");
    assert_eq!(
        v["usage"],
        json!({"input_tokens": 20, "output_tokens": 30, "cache_read_input_tokens": 100})
    );
}

#[test]
fn invalid_input_yields_invalid_request() {
    let e = openai::decode_request(b"{\"model\":\"x\"}").unwrap_err();
    assert_eq!(e.status(), 400);
    let e =
        openai::decode_request(br#"{"model":"x","messages":[{"role":"wizard","content":"x"}]}"#)
            .unwrap_err();
    assert!(e.message.contains("wizard"));
    assert_eq!(e.param.as_deref(), Some("messages[0]"));
    let e = anthropic::decode_request(br#"{"model":"x","messages":[]}"#).unwrap_err();
    assert_eq!(e.param.as_deref(), Some("max_tokens"));
    let e = anthropic::decode_request(
        br#"{"model":"x","max_tokens":1,"messages":[{"role":"user","content":[{"type":"text"}]}]}"#,
    )
    .unwrap_err();
    assert!(e.message.contains("text"), "{}", e.message);
    // Ungültige Tool-Argumente lassen sich nicht nach Anthropic übersetzen.
    let ir = openai::decode_request(
        br#"{"model":"x","messages":[{"role":"assistant","content":null,"tool_calls":[{"id":"a","type":"function","function":{"name":"f","arguments":"{kaputt"}}]}]}"#,
    )
    .unwrap();
    assert!(anthropic::encode_request(ir, &mut Losses::new()).is_err());
}

#[test]
fn ir_shapes_for_tool_messages() {
    let ir = openai::decode_request(&fixture("openai/request_full.json")).unwrap();
    let tool = &ir.messages[4];
    assert_eq!(tool.role, Role::Tool);
    let Content::Parts(parts) = &tool.content else {
        panic!("Teile erwartet")
    };
    assert!(
        matches!(&parts[0].kind, PartKind::ToolResult { tool_call_id, .. } if tool_call_id == "call_1")
    );
    assert_eq!(ir.extra.get("n"), Some(&json!(1)));
}

// ---------------------------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------------------------

fn sample_events() -> Vec<StreamEvent> {
    vec![
        StreamEvent::Start {
            id: "msg_1".into(),
            model: "m".into(),
            created: Some(1_763_000_000),
        },
        StreamEvent::Usage(Usage {
            input_tokens: 10,
            ..Usage::default()
        }),
        StreamEvent::ReasoningDelta {
            index: 0,
            text: "denke".into(),
        },
        StreamEvent::ReasoningSignature {
            index: 0,
            signature: "sig".into(),
        },
        StreamEvent::TextDelta {
            index: 1,
            text: "Hal".into(),
        },
        StreamEvent::TextDelta {
            index: 1,
            text: "lo".into(),
        },
        StreamEvent::ToolCallStart(ToolCallStart {
            index: 2,
            id: "t1".into(),
            name: "f".into(),
        }),
        StreamEvent::ToolCallDelta {
            index: 2,
            arguments: "{\"a\":".into(),
        },
        StreamEvent::ToolCallDelta {
            index: 2,
            arguments: "1}".into(),
        },
        StreamEvent::Stop {
            reason: StopReason::ToolUse,
            stop_sequence: None,
        },
        StreamEvent::Usage(Usage {
            input_tokens: 10,
            output_tokens: 7,
            ..Usage::default()
        }),
    ]
}

fn parse_sse(bytes: &[u8]) -> Vec<aigw_core::wire::sse::SseEvent> {
    let mut p = SseParser::new();
    p.push(bytes)
}

#[test]
fn anthropic_stream_encode_decode_roundtrip() {
    let mut enc = anthropic::StreamEncoder::new();
    let mut buf = Vec::new();
    for ev in sample_events() {
        enc.encode(ev, &mut buf);
    }
    enc.finish(&mut buf);
    let frames = parse_sse(&buf);
    let names: Vec<&str> = frames.iter().filter_map(|f| f.event.as_deref()).collect();
    assert_eq!(names.first(), Some(&"message_start"));
    assert_eq!(names.last(), Some(&"message_stop"));
    assert_eq!(
        names
            .iter()
            .filter(|n| **n == "content_block_start")
            .count(),
        3
    );
    assert_eq!(
        names.iter().filter(|n| **n == "content_block_stop").count(),
        3
    );

    let mut dec = anthropic::StreamDecoder::new();
    let mut events = Vec::new();
    for f in &frames {
        events.extend(dec.decode(&f.data).unwrap());
    }
    assert!(dec.is_done());
    // Identisch bis auf `created`, das Anthropic nicht überträgt.
    let mut expected = sample_events();
    expected[0] = StreamEvent::Start {
        id: "msg_1".into(),
        model: "m".into(),
        created: None,
    };
    assert_eq!(events, expected);
}

#[test]
fn openai_stream_encode_decode_roundtrip() {
    let mut enc = openai::StreamEncoder::new(true);
    let mut buf = Vec::new();
    for ev in sample_events() {
        enc.encode(ev, &mut buf);
    }
    enc.finish(&mut buf);
    let frames = parse_sse(&buf);
    assert_eq!(frames.last().unwrap().data, "[DONE]");
    assert!(frames.iter().all(|f| f.event.is_none()));

    let mut dec = openai::StreamDecoder::new();
    let mut events = Vec::new();
    for f in &frames {
        events.extend(dec.decode(&f.data).unwrap());
    }
    assert!(dec.is_done());
    // Reasoning entfällt in OpenAI; Blockindizes werden neu vergeben.
    let expected = vec![
        StreamEvent::Start {
            id: "msg_1".into(),
            model: "m".into(),
            created: Some(1_763_000_000),
        },
        StreamEvent::TextDelta {
            index: 0,
            text: "Hal".into(),
        },
        StreamEvent::TextDelta {
            index: 0,
            text: "lo".into(),
        },
        StreamEvent::ToolCallStart(ToolCallStart {
            index: 1,
            id: "t1".into(),
            name: "f".into(),
        }),
        StreamEvent::ToolCallDelta {
            index: 1,
            arguments: "{\"a\":".into(),
        },
        StreamEvent::ToolCallDelta {
            index: 1,
            arguments: "1}".into(),
        },
        StreamEvent::Stop {
            reason: StopReason::ToolUse,
            stop_sequence: None,
        },
        StreamEvent::Usage(Usage {
            input_tokens: 10,
            output_tokens: 7,
            ..Usage::default()
        }),
    ];
    assert_eq!(events, expected);
}

#[test]
fn openai_stream_without_include_usage_drops_usage_chunk() {
    let mut enc = openai::StreamEncoder::new(false);
    let mut buf = Vec::new();
    for ev in sample_events() {
        enc.encode(ev, &mut buf);
    }
    enc.finish(&mut buf);
    let text = String::from_utf8(buf).unwrap();
    assert!(!text.contains("\"usage\""));
}

#[test]
fn anthropic_sse_fixture_translates_to_openai_chunks() {
    let frames = parse_sse(&fixture("anthropic/stream_tool_use.sse"));
    let mut dec = anthropic::StreamDecoder::new();
    let mut enc = openai::StreamEncoder::new(true);
    let mut out = Vec::new();
    for f in &frames {
        for ev in dec.decode(&f.data).unwrap() {
            enc.encode(ev, &mut out);
        }
    }
    enc.finish(&mut out);

    let chunks: Vec<Value> = parse_sse(&out)
        .into_iter()
        .filter(|f| f.data != "[DONE]")
        .map(|f| serde_json::from_str(&f.data).unwrap())
        .collect();
    let text: String = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(text, "Ich prüfe das Wetter.");
    let args: String = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"].as_str())
        .collect();
    assert_eq!(
        serde_json::from_str::<Value>(&args).unwrap(),
        json!({"city": "Berlin"})
    );
    assert!(
        chunks
            .iter()
            .any(|c| c["choices"][0]["finish_reason"] == "tool_calls")
    );
    let usage = &chunks.last().unwrap()["usage"];
    assert_eq!(usage["prompt_tokens"], 35);
    assert_eq!(usage["completion_tokens"], 42);
    assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 10);
}

#[test]
fn openai_sse_fixture_translates_to_anthropic_events() {
    let frames = parse_sse(&fixture("openai/stream_tool_calls.sse"));
    let mut dec = openai::StreamDecoder::new();
    let mut enc = anthropic::StreamEncoder::new();
    let mut out = Vec::new();
    for f in &frames {
        for ev in dec.decode(&f.data).unwrap() {
            enc.encode(ev, &mut out);
        }
    }
    assert!(dec.is_done());
    enc.finish(&mut out);
    let events: Vec<(String, Value)> = parse_sse(&out)
        .into_iter()
        .map(|f| (f.event.unwrap(), serde_json::from_str(&f.data).unwrap()))
        .collect();
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop"
        ]
    );
    assert_eq!(events[5].1["content_block"]["type"], "tool_use");
    assert_eq!(events[5].1["index"], 1);
    assert_eq!(events[9].1["delta"]["stop_reason"], "tool_use");
    assert_eq!(events[9].1["usage"]["output_tokens"], 12);
    assert_eq!(events[9].1["usage"]["input_tokens"], 30);
}
