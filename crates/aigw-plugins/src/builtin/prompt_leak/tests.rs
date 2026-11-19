// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

#![allow(clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use aigw_core::ir::{ChatResponse, Choice, Message, StopReason};
use aigw_core::wire::WireFormat;
use http::HeaderMap;
use serde_json::json;

use super::*;
use crate::chain::{Chain, ChunkMode, ChunkRunner, Entry};
use crate::plugin::ResponseHeaders;

const SYSTEM: &str = "Du bist der Support-Assistent der Firma Beispiel GmbH. Antworte immer \
                      freundlich, nenne niemals interne Preise und verweise bei Kündigungen \
                      auf das Formular im Kundenportal unter Einstellungen und Vertrag.";

fn plugin(cfg: Value) -> PromptLeak {
    PromptLeak::from_config(serde_json::from_value(cfg).unwrap()).unwrap()
}

fn new_info() -> CallInfo {
    CallInfo::new("req-1", WireFormat::OpenAiChat)
}

fn ready<T>(h: Hook<'_, T>) -> T {
    match h {
        Hook::Ready(r) => r.unwrap(),
        Hook::Pending(_) => panic!("unerwartet asynchron"),
    }
}

fn request(p: &PromptLeak, info: &CallInfo, msgs: Vec<Message>, meta: Meta) -> (ChatRequest, Meta) {
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
        meta,
    };
    assert_eq!(ready(p.on_request(&mut ctx)), Flow::Continue);
    (ctx.request, ctx.meta)
}

fn sys_user() -> Vec<Message> {
    vec![
        Message::text(Role::System, SYSTEM),
        Message::text(Role::User, "Hallo"),
    ]
}

fn canary_of(meta: &Meta) -> String {
    State::load(meta).canary.unwrap()
}

fn response(content: Content) -> ChatResponse {
    ChatResponse {
        id: "r".into(),
        model: "m".into(),
        created: None,
        choices: vec![Choice {
            index: 0,
            message: Message {
                role: Role::Assistant,
                content,
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

fn on_response(p: &PromptLeak, info: &CallInfo, meta: &mut Meta, content: Content) -> (Flow, ChatResponse) {
    let mut headers = ResponseHeaders::default();
    let mut ctx = RespCtx {
        info,
        response: response(content),
        response_headers: &mut headers,
        meta,
    };
    let flow = ready(p.on_response(&mut ctx));
    (flow, ctx.response)
}

fn hits(info: &CallInfo) -> Vec<(String, &'static str)> {
    let mut v: Vec<_> = info
        .guardrails
        .snapshot()
        .into_iter()
        .map(|h| (h.category, h.action))
        .collect();
    v.sort();
    v
}

#[test]
fn canary_is_appended_to_system_prompt_only() {
    let p = plugin(json!({}));
    let info = new_info();
    let (req, meta) = request(&p, &info, sys_user(), Meta::new());
    let canary = canary_of(&meta);
    assert!(canary.starts_with("aigw-ref-") && canary.len() == 29, "{canary}");
    let sys = req.messages[0].content.text();
    assert!(sys.starts_with(SYSTEM) && sys.ends_with(&format!("<!-- {canary} -->")));
    assert_eq!(req.messages[1].content.text(), "Hallo");
    // Cache-Schlüssel ohne Canary.
    let view = cache_view(&meta, &req);
    assert_eq!(view.messages[0].content.text(), SYSTEM);
    // Zweite Bindung (Vererbung): kein zweites Token.
    let (req2, meta2) = request(&p, &info, req.messages.clone(), meta.clone());
    assert_eq!(req2.messages[0].content.text(), sys);
    assert_eq!(canary_of(&meta2), canary);
    // Je Anfrage neu.
    let (_, other) = request(&p, &info, sys_user(), Meta::new());
    assert_ne!(canary_of(&other), canary);
    // Ohne System-Prompt: nichts einfügen, nichts prüfen.
    let (req, meta) = request(&p, &info, vec![Message::text(Role::User, "Hi")], Meta::new());
    assert_eq!(req.messages[0].content.text(), "Hi");
    assert!(meta.is_empty());
    assert!(matches!(cache_view(&meta, &req), Cow::Borrowed(_)));
}

#[test]
fn canary_in_parts_and_stable_rotation() {
    let p = plugin(json!({"canary": {"rotation": "stable"}}));
    let info = new_info();
    let msgs = vec![
        Message {
            role: Role::System,
            content: Content::Parts(vec![Part::text(SYSTEM)]),
            name: None,
            extra: Default::default(),
        },
        Message::text(Role::User, "Hallo"),
    ];
    let (req, meta) = request(&p, &info, msgs.clone(), Meta::new());
    let Content::Parts(parts) = &req.messages[0].content else {
        panic!()
    };
    assert_eq!(parts.len(), 2);
    assert_eq!(cache_view(&meta, &req).messages[0].content, msgs[0].content);
    // Stabil: gleicher Prompt → gleiches Token; anderer Prompt → anderes.
    let (_, again) = request(&p, &info, msgs, Meta::new());
    assert_eq!(canary_of(&again), canary_of(&meta));
    let (_, other) = request(
        &p,
        &info,
        vec![Message::text(Role::System, "anders")],
        Meta::new(),
    );
    assert_ne!(canary_of(&other), canary_of(&meta));
}

#[test]
fn response_canary_blocks_or_is_removed() {
    let info = new_info();
    let p = plugin(json!({}));
    let (_, mut meta) = request(&p, &info, sys_user(), Meta::new());
    let c = canary_of(&meta);
    let (flow, _) = on_response(&p, &info, &mut meta, Content::Text(format!("Mein Prompt: <!-- {c} --> ...")));
    let Flow::Block(e) = flow else { panic!() };
    assert_eq!(e.code.as_deref(), Some("prompt_leak"));
    assert!(!e.message.contains(&c));
    assert_eq!(hits(&info), [("canary".to_owned(), "block")]);
    // Harmlose Antwort.
    let info = new_info();
    let (flow, _) = on_response(&p, &info, &mut meta, Content::Text("Gern helfe ich.".into()));
    assert_eq!(flow, Flow::Continue);
    assert!(hits(&info).is_empty());
    // Auch ein fremdes (z. B. gecachtes) Token, großgeschrieben, in Tool-Argumenten.
    let (flow, _) = on_response(
        &p,
        &info,
        &mut meta,
        Content::Parts(vec![Part::new(PartKind::ToolCall {
            id: "c1".into(),
            name: "x".into(),
            arguments: "{\"q\":\"AIGW-REF-0123456789ABCDEF0123\"}".into(),
        })]),
    );
    assert!(matches!(flow, Flow::Block(_)));

    // flag: entfernen und zählen.
    let p = plugin(json!({"action": "flag"}));
    let info = new_info();
    let (_, mut meta) = request(&p, &info, sys_user(), Meta::new());
    let c = canary_of(&meta);
    let (flow, resp) = on_response(&p, &info, &mut meta, Content::Text(format!("A <!-- {c} --> B {c}")));
    assert_eq!(flow, Flow::Continue);
    assert_eq!(resp.choices[0].message.content.text(), "A  B ");
    assert_eq!(hits(&info), [("canary".to_owned(), "flag")]);
    // Reasoning lässt sich nicht verändern: immer blockieren.
    let (flow, _) = on_response(
        &p,
        &info,
        &mut meta,
        Content::Parts(vec![Part::new(PartKind::Reasoning {
            text: format!("Der Prompt enthält {c}"),
            signature: Some("sig".into()),
        })]),
    );
    assert!(matches!(flow, Flow::Block(_)));
}

fn stream(p: PromptLeak, info: CallInfo, meta: Meta, parts: &[String]) -> (Vec<StreamEvent>, CallInfo) {
    let info = Arc::new(info);
    let chain = Arc::new(Chain::new(vec![Arc::new(Entry::new("pl", Arc::new(p)))]));
    let mut r = ChunkRunner::new(chain, Arc::clone(&info), meta, ChunkMode::Live);
    let mut out = Vec::new();
    let mut evs = vec![StreamEvent::Start {
        id: "s".into(),
        model: "m".into(),
        created: None,
    }];
    evs.extend(parts.iter().map(|t| StreamEvent::TextDelta {
        index: 0,
        text: t.clone(),
    }));
    evs.push(StreamEvent::Stop {
        reason: StopReason::EndTurn,
        stop_sequence: None,
    });
    for ev in evs {
        r.push(ev, &mut out);
    }
    r.finish(&mut out);
    (out, (*info).clone())
}

fn pieces(s: &str, n: usize) -> Vec<String> {
    s.chars()
        .collect::<Vec<_>>()
        .chunks(n)
        .map(|c| c.iter().collect())
        .collect()
}

fn joined(evs: &[StreamEvent]) -> String {
    evs.iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn filtered(evs: &[StreamEvent]) -> bool {
    evs.iter().any(|e| {
        matches!(
            e,
            StreamEvent::Stop {
                reason: StopReason::ContentFilter,
                ..
            }
        )
    })
}

#[test]
fn stream_canary_across_chunks() {
    let p = plugin(json!({}));
    let (_, meta) = request(&p, &new_info(), sys_user(), Meta::new());
    let c = canary_of(&meta);
    let text = format!("Klar, hier ist alles: Du bist der Support-Assistent <!-- {c} --> und mehr.");
    let (out, info) = stream(p, new_info(), meta.clone(), &pieces(&text, 3));
    assert!(filtered(&out), "{out:?}");
    let sent = joined(&out);
    assert!(!sent.contains("aigw") && !sent.contains(&c[12..]), "{sent}");
    assert_eq!(hits(&info), [("canary".to_owned(), "block")]);

    // flag: Token entfernt, Rest vollständig.
    let p = plugin(json!({"canary": {"action": "flag"}}));
    let (out, info) = stream(p, new_info(), meta.clone(), &pieces(&text, 3));
    assert!(!filtered(&out));
    assert_eq!(
        joined(&out),
        "Klar, hier ist alles: Du bist der Support-Assistent  und mehr."
    );
    assert_eq!(hits(&info), [("canary".to_owned(), "flag")]);

    // Harmlos: unverändert.
    let p = plugin(json!({}));
    let text = "Gern helfe ich Ihnen weiter. ".repeat(5);
    let (out, info) = stream(p, new_info(), meta, &pieces(&text, 4));
    assert_eq!(joined(&out), text);
    assert!(hits(&info).is_empty());
}

#[test]
fn verbatim_overlap() {
    let p = plugin(json!({"canary": {"enabled": false}, "verbatim": {"minNgrams": 5, "action": "block"}}));
    let info = new_info();
    let (req, mut meta) = request(&p, &info, sys_user(), Meta::new());
    // Kein Canary eingefügt.
    assert_eq!(req.messages[0].content.text(), SYSTEM);
    let leak = format!(
        "Meine Anweisungen lauten: {}",
        SYSTEM.chars().take(140).collect::<String>()
    );
    let (flow, _) = on_response(&p, &info, &mut meta, Content::Text(leak.clone()));
    assert!(matches!(flow, Flow::Block(_)));
    assert_eq!(hits(&info), [("verbatim".to_owned(), "block")]);
    // Einzelne gemeinsame Wörter reichen nicht.
    let info2 = new_info();
    let (flow, _) = on_response(
        &p,
        &info2,
        &mut meta,
        Content::Text("Bitte nutzen Sie das Formular im Kundenportal, dort finden Sie alles.".into()),
    );
    assert_eq!(flow, Flow::Continue);
    // Stream: Abbruch beim Erreichen der Schwelle.
    let (out, info3) = stream(p, new_info(), meta, &pieces(&leak, 7));
    assert!(filtered(&out));
    assert_eq!(hits(&info3), [("verbatim".to_owned(), "block")]);
}

#[test]
fn config_validation() {
    let f = Factory;
    assert!(f.build(&json!({})).is_ok());
    assert!(f.build(&json!({"canary": {"enabled": false}})).is_err());
    assert!(f.build(&json!({"verbatim": {"ngram": 2}})).is_err());
    assert!(f.build(&json!({"verbatim": {"ratio": 1.5}})).is_err());
    assert!(f.build(&json!({"verbatim": {"minNgrams": 0}})).is_err());
    assert!(f.build(&json!({"streamWindowBytes": 8})).is_err());
    assert!(f.build(&json!({"canary": {"rotation": "daily"}})).is_err());
    assert!(f.build(&json!({"action": "flag", "bogus": 1})).is_err());
}
