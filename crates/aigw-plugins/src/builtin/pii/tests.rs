// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

#![allow(clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use aigw_core::ir::{
    ChatRequest, ChatResponse, Choice, Content, Message, Part, PartKind, Role, StopReason,
    StreamEvent,
};
use aigw_core::wire::WireFormat;
use http::HeaderMap;
use serde_json::json;

use super::*;
use crate::chain::{Chain, ChunkMode, ChunkRunner, Entry};
use crate::plugin::{Hook, ResponseHeaders};

fn plugin(cfg: Value) -> Arc<Pii> {
    let c: Config = serde_json::from_value(cfg).unwrap();
    Arc::new(Pii::from_config(c).unwrap())
}

fn info() -> CallInfo {
    CallInfo::new("req-1", WireFormat::OpenAiChat)
}

fn user(text: &str) -> Message {
    Message {
        role: Role::User,
        content: Content::Text(text.into()),
        name: None,
        extra: Default::default(),
    }
}

fn ready<T>(h: Hook<'_, T>) -> T {
    match h {
        Hook::Ready(r) => r.unwrap(),
        Hook::Pending(_) => panic!("unerwartet asynchron"),
    }
}

/// Anfrage-Phase; liefert Ergebnis, veränderte Anfrage und Meta.
fn run_request(p: &Pii, info: &CallInfo, msgs: Vec<Message>) -> (Flow, ChatRequest, Meta) {
    let headers = HeaderMap::new();
    let mut ctx = ReqCtx {
        info,
        request: ChatRequest {
            model: "m".into(),
            messages: msgs,
            ..ChatRequest::default()
        },
        client_headers: &headers,
        upstream_headers: HeaderMap::new(),
        response_headers: ResponseHeaders::default(),
        meta: Meta::new(),
    };
    let flow = ready(p.on_request(&mut ctx));
    (flow, ctx.request, ctx.meta)
}

fn response(text: &str) -> ChatResponse {
    ChatResponse {
        id: "r".into(),
        model: "m".into(),
        created: None,
        choices: vec![Choice {
            index: 0,
            message: Message {
                role: Role::Assistant,
                content: Content::Text(text.into()),
                name: None,
                extra: Default::default(),
            },
            stop_reason: Some(StopReason::EndTurn),
            stop_sequence: None,
            extra: Default::default(),
        }],
        usage: None,
        extra: Default::default(),
        source: None,
    }
}

fn hits(info: &CallInfo) -> Vec<(String, &'static str, u32)> {
    let mut v: Vec<_> = info
        .guardrails
        .snapshot()
        .into_iter()
        .map(|h| (h.category, h.action, h.count))
        .collect();
    v.sort();
    v
}

#[test]
fn config_validation() {
    let f = Factory;
    assert!(f.build(&json!({})).is_ok());
    assert!(f.build(&json!({"categories": {"name": "mask"}})).is_err());
    assert!(f.build(&json!({"categories": {}})).is_err());
    assert!(f.build(&json!({"unknown": 1})).is_err());
    assert!(f.build(&json!({"action": "explode"})).is_err());
    assert!(
        f.build(
            &json!({"categories": {"email": {"action": "pseudonymize", "direction": "output"}}})
        )
        .is_err()
    );
    assert!(f.build(&json!({"streamWindowBytes": 1})).is_err());
    assert!(
        f.build(
            &json!({"categories": {"creditCard": "reject", "email": {"action": "mask"}},
                        "allow": ["*@example.com"], "entropy": {"enabled": false}})
        )
        .is_ok()
    );
}

#[test]
fn request_modes() {
    let p = plugin(json!({"categories": {
        "email": "mask", "phone": "redact", "iban": "log-only", "ipAddress": "pseudonymize"}}));
    let info = info();
    let (flow, req, meta) = run_request(
        &p,
        &info,
        vec![
            user("Ich bin bob@example.com und alice@example.org, Tel +49 30 1234567"),
            user("Nochmal bob@example.com, IBAN DE89370400440532013000, Host 10.1.2.3"),
        ],
    );
    assert_eq!(flow, Flow::Continue);
    assert_eq!(
        req.messages[0].content.text(),
        "Ich bin [EMAIL_1] und [EMAIL_2], Tel [REDACTED]"
    );
    assert_eq!(
        req.messages[1].content.text(),
        "Nochmal [EMAIL_1], IBAN DE89370400440532013000, Host [IP_1]"
    );
    // Nur Pseudonyme werden für die Rückübersetzung gemerkt, Masken nicht.
    let map = &meta[META_KEY]["map"];
    assert_eq!(map, &json!({"[IP_1]": "10.1.2.3"}));
    assert_eq!(
        hits(&info),
        [
            ("email".to_owned(), "mask", 3),
            ("iban".to_owned(), "log-only", 1),
            ("ip_address".to_owned(), "pseudonymize", 1),
            ("phone".to_owned(), "redact", 1),
        ]
    );
}

#[test]
fn tool_arguments_and_results_are_scanned() {
    let p = plugin(json!({"action": "mask"}));
    let info = info();
    let msg = Message {
        role: Role::Assistant,
        content: Content::Parts(vec![
            Part::text("ok"),
            Part::new(PartKind::ToolCall {
                id: "c1".into(),
                name: "send".into(),
                arguments: r#"{"to":"bob@example.com"}"#.into(),
            }),
        ]),
        name: None,
        extra: Default::default(),
    };
    let (_, req, _) = run_request(&p, &info, vec![msg]);
    let Content::Parts(parts) = &req.messages[0].content else {
        panic!()
    };
    assert_eq!(
        parts[1].kind,
        PartKind::ToolCall {
            id: "c1".into(),
            name: "send".into(),
            arguments: r#"{"to":"[EMAIL_1]"}"#.into(),
        }
    );
}

#[test]
fn reject_names_category_but_not_value() {
    let p = plugin(json!({"categories": {"creditCard": "reject", "email": "mask"}}));
    let info = info();
    let (flow, _, _) = run_request(&p, &info, vec![user("Karte 4111 1111 1111 1111 bitte")]);
    let Flow::Reject(err) = flow else {
        panic!("{flow:?}")
    };
    assert_eq!(err.status(), 400);
    assert_eq!(err.code.as_deref(), Some("pii_detected"));
    assert!(err.message.contains("credit_card"));
    assert!(!err.message.contains("4111"));
    assert_eq!(hits(&info), [("credit_card".to_owned(), "reject", 1)]);
}

#[test]
fn allowlist_is_respected() {
    let p = plugin(json!({"allow": ["support@acme.example"]}));
    let (_, req, _) = run_request(
        &p,
        &info(),
        vec![user("support@acme.example oder bob@acme.example")],
    );
    assert_eq!(
        req.messages[0].content.text(),
        "support@acme.example oder [EMAIL_1]"
    );
}

#[test]
fn pseudonymize_roundtrip_in_json_response() {
    let p = plugin(json!({"action": "pseudonymize", "direction": "input"}));
    let info = info();
    let (_, req, mut meta) = run_request(&p, &info, vec![user("Schreib an bob@example.com")]);
    assert_eq!(req.messages[0].content.text(), "Schreib an [EMAIL_1]");
    let mut rh = ResponseHeaders::default();
    let mut ctx = RespCtx {
        info: &info,
        response: response("Gern, ich schreibe [EMAIL_1]. [EMAIL_9] bleibt."),
        response_headers: &mut rh,
        meta: &mut meta,
    };
    assert_eq!(ready(p.on_response(&mut ctx)), Flow::Continue);
    assert_eq!(
        ctx.response.choices[0].message.content.text(),
        "Gern, ich schreibe bob@example.com. [EMAIL_9] bleibt."
    );
}

fn stream(p: Arc<Pii>, info: CallInfo, meta: Meta, events: Vec<StreamEvent>) -> Vec<StreamEvent> {
    let chain = Arc::new(Chain::new(vec![Arc::new(Entry::new("pii", p))]));
    let mut r = ChunkRunner::new(chain, Arc::new(info), meta, ChunkMode::Live);
    let mut out = Vec::new();
    for ev in events {
        r.push(ev, &mut out);
    }
    r.finish(&mut out);
    out
}

fn deltas(parts: &[&str]) -> Vec<StreamEvent> {
    let mut v = vec![StreamEvent::Start {
        id: "s".into(),
        model: "m".into(),
        created: None,
    }];
    v.extend(parts.iter().map(|t| StreamEvent::TextDelta {
        index: 0,
        text: (*t).into(),
    }));
    v.push(StreamEvent::Stop {
        reason: StopReason::EndTurn,
        stop_sequence: None,
    });
    v
}

fn joined(evs: &[StreamEvent]) -> String {
    evs.iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn stream_restores_placeholders_across_chunk_boundaries() {
    let p = plugin(json!({"action": "pseudonymize", "direction": "input"}));
    let info0 = info();
    let (_, _, meta) = run_request(&p, &info0, vec![user("bob@example.com und +49 30 1234567")]);
    let out = stream(
        Arc::clone(&p),
        info(),
        meta,
        deltas(&[
            "Hallo [",
            "EMA",
            "IL_1], Tel. [PHO",
            "NE_1]",
            " und [Quelle 1] [",
            "x",
        ]),
    );
    assert_eq!(
        joined(&out),
        "Hallo bob@example.com, Tel. +49 30 1234567 und [Quelle 1] [x"
    );
    // Reihenfolge bleibt: Start zuerst, Stop zuletzt.
    assert!(matches!(out.first(), Some(StreamEvent::Start { .. })));
    assert!(matches!(out.last(), Some(StreamEvent::Stop { .. })));
    // Teilplatzhalter wird nicht vorzeitig ausgegeben.
    assert!(
        !out.iter()
            .any(|e| matches!(e, StreamEvent::TextDelta { text, .. } if text.contains("[EMA")))
    );
}

#[test]
fn stream_output_detection_spans_chunks() {
    let p = plugin(
        json!({"categories": {"email": "mask"}, "direction": "output",
                          "streamWindowBytes": 16}),
    );
    let info = info();
    let out = stream(
        Arc::clone(&p),
        info.clone(),
        Meta::new(),
        deltas(&[
            "Kontakt: bob@exa",
            "mple.com – ",
            "sonst nichts ",
            "weiter.",
        ]),
    );
    let text = joined(&out);
    assert_eq!(text, "Kontakt: [EMAIL_1] – sonst nichts weiter.");
    assert_eq!(hits(&info), [("email".to_owned(), "mask", 1)]);
}

#[test]
fn stream_output_reject_ends_stream() {
    let p = plugin(json!({"categories": {"creditCard": "reject"}, "direction": "output"}));
    let out = stream(
        p,
        info(),
        Meta::new(),
        deltas(&["Die Nummer ist 4111 ", "1111 1111 1111", " ok"]),
    );
    let last = out.last().unwrap();
    let StreamEvent::Error(e) = last else {
        panic!("{out:?}")
    };
    assert_eq!(e.code.as_deref(), Some("pii_detected"));
    assert!(!joined(&out).contains("4111"));
}

#[test]
fn stream_tool_call_arguments_are_restored() {
    let p = plugin(json!({"action": "pseudonymize", "direction": "input"}));
    let (_, _, meta) = run_request(&p, &info(), vec![user("an bob@example.com")]);
    let evs = vec![
        StreamEvent::ToolCallStart(aigw_core::ir::ToolCallStart {
            index: 0,
            id: "c".into(),
            name: "send".into(),
        }),
        StreamEvent::ToolCallDelta {
            index: 0,
            arguments: "{\"to\":\"[EMAI".into(),
        },
        StreamEvent::ToolCallDelta {
            index: 0,
            arguments: "L_1]\"}".into(),
        },
    ];
    let out = stream(p, info(), meta, evs);
    let args: String = out
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallDelta { arguments, .. } => Some(arguments.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(args, "{\"to\":\"bob@example.com\"}");
}
