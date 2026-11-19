// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

#![allow(clippy::unwrap_used, clippy::panic)]

use std::sync::Mutex;
use std::time::{Duration, Instant};

use aigw_core::ir::{
    ChatResponse, Choice, FunctionTool, Tool, ToolCallStart, Usage,
};
use aigw_core::observe::PluginCalls;
use aigw_core::resource::CredentialData;
use aigw_core::wire::WireFormat;
use http::HeaderMap;
use serde_json::json;

use super::*;
use crate::NoHost;
use crate::chain::{Chain, ChunkMode, ChunkRunner, Entry};
use crate::host::{HttpCall, HttpReply};
use crate::plugin::{BoxFuture, ResponseHeaders};

/// Gateway-Host mit vorbereiteten Antworten; merkt sich die Anfragen.
#[derive(Debug, Default)]
struct FakeHost {
    replies: Mutex<Vec<ChatResponse>>,
    seen: Mutex<Vec<(String, ChatRequest)>>,
}

impl PluginHost for FakeHost {
    fn credential(&self, _name: &str) -> Option<Arc<CredentialData>> {
        None
    }

    fn check_endpoint(&self, _url: &str) -> Result<(), String> {
        Ok(())
    }

    fn public_only(&self) -> Arc<dyn PluginHost> {
        Arc::new(NoHost::default())
    }

    fn post_json(&self, _call: HttpCall) -> BoxFuture<'static, Result<HttpReply, PluginError>> {
        Box::pin(async { Err(PluginError::new("nein")) })
    }

    fn chat(
        &self,
        model_group: &str,
        req: ChatRequest,
        calls: &PluginCalls,
    ) -> BoxFuture<'static, Result<ChatResponse, PluginError>> {
        self.seen
            .lock()
            .unwrap()
            .push((model_group.to_owned(), req));
        let r = self.replies.lock().unwrap().pop();
        if let Some(r) = &r {
            calls.record(aigw_core::observe::PluginCall {
                model_group: model_group.to_owned(),
                provider: None,
                provider_model: None,
                usage: r.usage.clone().unwrap_or_default(),
            });
        }
        Box::pin(async move { r.ok_or_else(|| PluginError::new("keine Antwort")) })
    }
}

fn guard_with(cfg: Value, host: Arc<dyn PluginHost>) -> Arc<SchemaGuard> {
    let c: Config = serde_json::from_value(cfg).unwrap();
    Arc::new(SchemaGuard::from_config(c, host).unwrap())
}

fn guard(cfg: Value) -> Arc<SchemaGuard> {
    guard_with(cfg, Arc::new(NoHost::default()))
}

fn build_err(cfg: Value) -> String {
    match Factory::new(Arc::new(NoHost::default())).build(&cfg) {
        Ok(_) => panic!("Konfiguration {cfg} sollte ungültig sein"),
        Err(e) => e,
    }
}

fn info(stream: bool) -> CallInfo {
    let mut i = CallInfo::new("req-1", WireFormat::OpenAiChat);
    i.model_group = Some("gpt".into());
    i.stream = stream;
    i
}

fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {"name": {"type": "string"}, "age": {"type": "integer", "minimum": 0}},
        "required": ["name", "age"],
        "additionalProperties": false
    })
}

fn request(format: Option<ResponseFormat>) -> ChatRequest {
    ChatRequest {
        model: "gpt".into(),
        messages: vec![Message::text(Role::User, "Wer bist du?")],
        response_format: format,
        ..ChatRequest::default()
    }
}

fn json_schema(s: Value) -> Option<ResponseFormat> {
    Some(ResponseFormat::JsonSchema {
        name: Some("person".into()),
        description: None,
        schema: s,
        strict: Some(true),
    })
}

fn response(text: &str) -> ChatResponse {
    ChatResponse {
        id: "r".into(),
        model: "m".into(),
        created: None,
        choices: vec![Choice {
            index: 0,
            message: Message::text(Role::Assistant, text),
            stop_reason: Some(StopReason::EndTurn),
            stop_sequence: None,
            extra: Default::default(),
        }],
        usage: Some(Usage {
            input_tokens: 10,
            output_tokens: 7,
            ..Usage::default()
        }),
        extra: Default::default(),
        source: None,
    }
}

fn tool_response(name: &str, args: &str) -> ChatResponse {
    let mut r = response("");
    r.choices[0].message.content = Content::Parts(vec![Part::new(PartKind::ToolCall {
        id: "call_1".into(),
        name: name.into(),
        arguments: args.into(),
    })]);
    r.choices[0].stop_reason = Some(StopReason::ToolUse);
    r
}

fn ready<T>(h: Hook<'_, T>) -> T {
    match h {
        Hook::Ready(r) => r.unwrap(),
        Hook::Pending(_) => panic!("unerwartet asynchron"),
    }
}

async fn resolve<T>(h: Hook<'_, T>) -> T {
    match h {
        Hook::Ready(r) => r.unwrap(),
        Hook::Pending(f) => f.await.unwrap(),
    }
}

fn run_request(p: &SchemaGuard, info: &CallInfo, req: ChatRequest) -> (Flow, Meta) {
    let headers = HeaderMap::new();
    let mut ctx = ReqCtx {
        info,
        request: req,
        client_headers: &headers,
        upstream_headers: HeaderMap::new(),
        response_headers: ResponseHeaders::default(),
        meta: Meta::new(),
    };
    let flow = ready(p.on_request(&mut ctx));
    (flow, ctx.meta)
}

async fn run_response(
    p: &SchemaGuard,
    info: &CallInfo,
    meta: &mut Meta,
    resp: ChatResponse,
) -> (Flow, ChatResponse, ResponseHeaders) {
    let mut headers = ResponseHeaders::default();
    let mut ctx = RespCtx {
        info,
        response: resp,
        response_headers: &mut headers,
        meta,
    };
    let flow = resolve(p.on_response(&mut ctx)).await;
    let resp = ctx.response;
    (flow, resp, headers)
}

fn hits(info: &CallInfo) -> Vec<(String, &'static str, u32)> {
    info.guardrails
        .snapshot()
        .into_iter()
        .map(|h| (h.category, h.action, h.count))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Konfiguration und Schema-Schutz
// ---------------------------------------------------------------------------------------------

#[test]
fn config_validation() {
    assert!(Factory::new(Arc::new(NoHost::default())).build(&json!({})).is_ok());
    assert!(build_err(json!({"action": "retry", "maxRetries": 4})).contains("maxRetries"));
    assert!(build_err(json!({"action": "retry", "maxRetries": 0})).contains("maxRetries"));
    assert!(build_err(json!({"action": "reject", "maxRetries": 2})).contains("nur mit"));
    assert!(build_err(json!({"action": "drop"})).contains("unknown variant"));
    assert!(build_err(json!({"tools": "some"})).contains("unknown variant"));
    assert!(build_err(json!({"limits": {"maxSchemaDepth": 1}})).contains("maxSchemaDepth"));
    assert!(build_err(json!({"limits": {"maxSchemaBytes": 10}})).contains("maxSchemaBytes"));
    assert!(build_err(json!({"limits": {"maxOutputBytes": 1}})).contains("maxOutputBytes"));
    assert!(build_err(json!({"unbekannt": 1})).contains("unknown field"));
    let g = guard(json!({"action": "retry", "maxRetries": 3, "stream": "buffer"}));
    assert_eq!(g.max_retries, 3);
    assert!(g.needs_buffered_output());
    assert!(!guard(json!({})).needs_buffered_output());
}

#[test]
fn schema_limits_and_external_refs() {
    let l = Limits::default();
    assert!(compile(&schema(), &l).is_ok());
    // Externe Verweise werden nie aufgelöst.
    for r in [
        json!({"$ref": "https://example.com/schema.json"}),
        json!({"properties": {"a": {"$ref": "file:///etc/passwd"}}}),
        json!({"$defs": {"x": {"$dynamicRef": "other.json#meta"}}}),
        json!({"$ref": 5}),
    ] {
        let e = compile(&r, &l).unwrap_err();
        assert!(e.contains("dokumentintern"), "{e}");
    }
    // Interne Verweise sind erlaubt.
    let local = json!({"$defs": {"n": {"type": "integer"}}, "type": "object",
                       "properties": {"a": {"$ref": "#/$defs/n"}}});
    let v = compile(&local, &l).unwrap();
    assert!(v.is_valid(&json!({"a": 1})));
    assert!(!v.is_valid(&json!({"a": "x"})));
    // Unbekanntes Meta-Schema wird nicht nachgeladen.
    assert!(compile(&json!({"$schema": "https://evil.example/meta"}), &l).is_err());
    // Größe und Tiefe.
    let big = json!({"enum": vec!["x".repeat(100); 1000]});
    assert!(compile(&big, &l).unwrap_err().contains("größer"));
    let mut deep = json!({"type": "string"});
    for _ in 0..40 {
        deep = json!({"items": deep});
    }
    assert!(compile(&deep, &l).unwrap_err().contains("tiefer"));
    assert!(compile(&json!("kein Schema"), &l).is_err());
    assert!(compile(&json!({"type": 7}), &l).unwrap_err().contains("ungültig"));
}

#[test]
fn patterns_use_linear_regex_engine() {
    // Klassisches ReDoS-Muster: mit Backtracking exponentiell, mit `regex` linear.
    let s = json!({"type": "string", "pattern": "^(a+)+$"});
    let v = compile(&s, &Limits::default()).unwrap();
    let evil = format!("{}!", "a".repeat(50_000));
    let t = Instant::now();
    assert!(!v.is_valid(&json!(evil)));
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    // Rückverweise (nur Backtracking) sind nicht übersetzbar.
    assert!(compile(&json!({"pattern": "(a)\\1"}), &Limits::default()).is_err());
}

// ---------------------------------------------------------------------------------------------
// Anfrage-Phase
// ---------------------------------------------------------------------------------------------

#[test]
fn request_without_structured_output_is_untouched() {
    let g = guard(json!({}));
    let i = info(false);
    let (flow, meta) = run_request(&g, &i, request(None));
    assert_eq!(flow, Flow::Continue);
    assert!(meta.is_empty());
    let (_, meta) = run_request(&g, &i, request(Some(ResponseFormat::Text)));
    assert!(meta.is_empty());
    // json_object nur mit jsonObject (Standard an).
    let (_, meta) = run_request(&g, &i, request(Some(ResponseFormat::JsonObject)));
    assert!(meta.contains_key(META_KEY));
    let g = guard(json!({"jsonObject": false}));
    let (_, meta) = run_request(&g, &i, request(Some(ResponseFormat::JsonObject)));
    assert!(meta.is_empty());
}

#[test]
fn bad_request_schema_rejected_or_skipped() {
    let i = info(false);
    let bad = json_schema(json!({"$ref": "https://example.com/s.json"}));
    let (flow, _) = run_request(&guard(json!({})), &i, request(bad.clone()));
    let Flow::Block(e) = flow else {
        panic!("{flow:?}")
    };
    assert_eq!(e.kind, ErrorKind::InvalidRequest);
    assert_eq!(e.code.as_deref(), Some("response_schema_rejected"));
    assert_eq!(hits(&i), [("schema-rejected".into(), "reject", 1)]);
    let i = info(false);
    let (flow, meta) = run_request(&guard(json!({"invalidSchema": "skip"})), &i, request(bad));
    assert_eq!(flow, Flow::Continue);
    assert!(meta.is_empty());
    assert_eq!(hits(&i), [("schema-rejected".into(), "skip", 1)]);
}

#[test]
fn final_request_requested_only_for_retry() {
    let req = || request(json_schema(schema()));
    let (_, meta) = run_request(&guard(json!({})), &info(false), req());
    assert!(!meta.contains_key(FINAL_REQUEST_META));
    let g = guard(json!({"action": "retry"}));
    let (_, meta) = run_request(&g, &info(false), req());
    assert_eq!(meta.get(FINAL_REQUEST_META), Some(&Value::Null));
    // Live-Stream: kein Retry möglich.
    let (_, meta) = run_request(&g, &info(true), req());
    assert!(!meta.contains_key(FINAL_REQUEST_META));
    let g = guard(json!({"action": "retry", "stream": "buffer"}));
    let (_, meta) = run_request(&g, &info(true), req());
    assert!(meta.contains_key(FINAL_REQUEST_META));
}

fn tool(name: &str, params: Value) -> Tool {
    Tool {
        kind: ToolKind::Function(FunctionTool {
            name: name.into(),
            description: None,
            parameters: Some(params),
            strict: None,
            extra: Default::default(),
        }),
        cache_control: None,
        extra: Default::default(),
    }
}

fn tool_request(choice: Option<ToolChoice>) -> ChatRequest {
    let mut r = request(None);
    r.tools = vec![
        tool("person", schema()),
        tool("other", json!({"type": "object"})),
    ];
    r.tool_choice = choice;
    r
}

#[test]
fn tool_modes() {
    let forced = Some(ToolChoice::Tool {
        name: "person".into(),
    });
    let g = guard(json!({}));
    assert_eq!(
        g.spec_of(&tool_request(forced.clone()))
            .tools
            .keys()
            .collect::<Vec<_>>(),
        ["person"]
    );
    assert_eq!(g.spec_of(&tool_request(Some(ToolChoice::Required))).tools.len(), 2);
    assert!(g.spec_of(&tool_request(Some(ToolChoice::Auto))).is_empty());
    assert!(g.spec_of(&tool_request(None)).is_empty());
    let all = guard(json!({"tools": "all"}));
    assert_eq!(all.spec_of(&tool_request(None)).tools.len(), 2);
    let off = guard(json!({"tools": "off"}));
    assert!(off.spec_of(&tool_request(forced)).is_empty());
}

// ---------------------------------------------------------------------------------------------
// Antwort-Phase
// ---------------------------------------------------------------------------------------------

async fn check(cfg: Value, req: ChatRequest, resp: ChatResponse) -> (Flow, ChatResponse, ResponseHeaders, CallInfo) {
    let g = guard(cfg);
    let i = info(false);
    let (_, mut meta) = run_request(&g, &i, req);
    let (flow, resp, headers) = run_response(&g, &i, &mut meta, resp).await;
    (flow, resp, headers, i)
}

#[tokio::test]
async fn valid_output_passes() {
    let (flow, _, h, i) = check(
        json!({}),
        request(json_schema(schema())),
        response(" {\"name\": \"Ada\", \"age\": 36}\n"),
    )
    .await;
    assert_eq!(flow, Flow::Continue);
    assert!(h.is_empty());
    assert!(hits(&i).is_empty());
}

#[tokio::test]
async fn invalid_output_rejected_with_category() {
    for (text, cat) in [
        ("Hier ist das JSON: {\"name\": \"Ada\"}", "invalid-json"),
        ("```json\n{\"name\": \"Ada\", \"age\": 36}\n```", "invalid-json"),
        ("{\"name\": \"Ada\"}", "schema-mismatch"),
        ("{\"name\": \"Ada\", \"age\": -1}", "schema-mismatch"),
        ("{\"name\": \"Ada\", \"age\": 1, \"x\": 1}", "schema-mismatch"),
    ] {
        let (flow, _, _, i) = check(json!({}), request(json_schema(schema())), response(text)).await;
        let Flow::Block(e) = flow else {
            panic!("{text}: {flow:?}")
        };
        assert_eq!(e.kind, ErrorKind::BadGateway);
        assert_eq!(e.code.as_deref(), Some("schema_validation_failed"));
        // Fehlermeldung ohne Inhalte der Ausgabe.
        assert!(!e.message.contains("Ada"), "{}", e.message);
        assert_eq!(hits(&i), [(cat.into(), "reject", 1)], "{text}");
    }
}

#[tokio::test]
async fn json_object_only_checks_json() {
    let req = || request(Some(ResponseFormat::JsonObject));
    let (flow, ..) = check(json!({}), req(), response("{\"beliebig\": [1, 2]}")).await;
    assert_eq!(flow, Flow::Continue);
    for bad in ["[1, 2]", "kein json", "\"text\""] {
        let (flow, ..) = check(json!({}), req(), response(bad)).await;
        assert!(matches!(flow, Flow::Block(_)), "{bad}: {flow:?}");
    }
}

#[tokio::test]
async fn flag_counts_and_sets_header() {
    let (flow, resp, h, i) = check(
        json!({"action": "flag"}),
        request(json_schema(schema())),
        response("nope"),
    )
    .await;
    assert_eq!(flow, Flow::Continue);
    assert_eq!(resp.choices[0].message.content.text(), "nope");
    assert_eq!(h.set.get(HEADER).unwrap(), "invalid");
    assert_eq!(hits(&i), [("invalid-json".into(), "flag", 1)]);
}

#[tokio::test]
async fn refusal_tool_calls_and_filtered_are_not_text_checked() {
    let req = || request(json_schema(schema()));
    let mut r = response("");
    r.choices[0].message.content = Content::Parts(vec![Part::new(PartKind::Refusal {
        text: "Dazu kann ich nichts sagen.".into(),
    })]);
    assert_eq!(check(json!({}), req(), r).await.0, Flow::Continue);
    let mut r = response("abgeschnitten");
    r.choices[0].stop_reason = Some(StopReason::ContentFilter);
    assert_eq!(check(json!({}), req(), r).await.0, Flow::Continue);
    // Tool-Aufruf ohne Tool-Erzwingung: Text-Schema gilt nicht.
    assert_eq!(
        check(json!({}), req(), tool_response("other", "{}")).await.0,
        Flow::Continue
    );
}

#[tokio::test]
async fn forced_tool_arguments_checked() {
    let forced = || {
        tool_request(Some(ToolChoice::Tool {
            name: "person".into(),
        }))
    };
    let ok = tool_response("person", "{\"name\": \"Ada\", \"age\": 3}");
    assert_eq!(check(json!({}), forced(), ok).await.0, Flow::Continue);
    let bad = tool_response("person", "{\"name\": 5}");
    let (flow, _, _, i) = check(json!({}), forced(), bad).await;
    assert!(matches!(flow, Flow::Block(_)), "{flow:?}");
    assert_eq!(hits(&i), [("tool-arguments".into(), "reject", 1)]);
    let (flow, ..) = check(json!({}), forced(), tool_response("person", "{kaputt")).await;
    assert!(matches!(flow, Flow::Block(_)));
}

#[tokio::test]
async fn output_size_limit() {
    let big = format!("{{\"name\": \"{}\", \"age\": 1}}", "x".repeat(2048));
    let (flow, _, _, i) = check(
        json!({"limits": {"maxOutputBytes": 1024}}),
        request(json_schema(schema())),
        response(&big),
    )
    .await;
    assert!(matches!(flow, Flow::Block(_)));
    assert_eq!(hits(&i), [("output-too-large".into(), "reject", 1)]);
}

// ---------------------------------------------------------------------------------------------
// Retry
// ---------------------------------------------------------------------------------------------

/// Anfrage-Phase mit Retry; legt die endgültige Anfrage wie die Pipeline in Meta ab.
fn retry_meta(g: &SchemaGuard, i: &CallInfo, req: ChatRequest) -> Meta {
    let (flow, mut meta) = run_request(g, i, req.clone());
    assert_eq!(flow, Flow::Continue);
    assert!(meta.contains_key(FINAL_REQUEST_META));
    meta.insert(FINAL_REQUEST_META.into(), serde_json::to_value(&req).unwrap());
    meta
}

#[tokio::test]
async fn retry_succeeds_and_keeps_first_usage() {
    let host = Arc::new(FakeHost::default());
    let mut fixed = response("{\"name\": \"Ada\", \"age\": 36}");
    fixed.usage = Some(Usage {
        input_tokens: 99,
        output_tokens: 9,
        ..Usage::default()
    });
    host.replies.lock().unwrap().push(fixed);
    let g = guard_with(json!({"action": "retry"}), host.clone());
    let i = info(false);
    let mut meta = retry_meta(&g, &i, request(json_schema(schema())));
    let (flow, resp, h) = run_response(&g, &i, &mut meta, response("{\"name\": 1}")).await;
    let Flow::Respond(new) = flow else {
        panic!("{flow:?}")
    };
    assert_eq!(new.choices[0].message.content.text(), "{\"name\": \"Ada\", \"age\": 36}");
    // Usage der Client-Antwort = erster Aufruf; die Wiederholung ist ein Plugin-Aufruf.
    assert_eq!(new.usage.as_ref().unwrap().input_tokens, 10);
    assert_eq!(resp.usage.as_ref().unwrap().input_tokens, 10);
    let calls = i.plugin_calls.take();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].usage.input_tokens, 99);
    assert_eq!(h.set.get(HEADER).unwrap(), "retried=1");
    assert_eq!(hits(&i), [("schema-mismatch".into(), "retry", 1)]);
    // Wiederholung: gleiche Modellgruppe, nicht gestreamt, mit Antwort + Korrektur.
    let seen = host.seen.lock().unwrap();
    let (group, req) = &seen[0];
    assert_eq!(group, "gpt");
    assert_eq!(req.stream, Some(false));
    assert_eq!(req.messages.len(), 3);
    assert_eq!(req.messages[1].role, Role::Assistant);
    assert_eq!(req.messages[1].content.text(), "{\"name\": 1}");
    assert_eq!(req.messages[2].role, Role::User);
    assert!(req.messages[2].content.text().contains("JSON"));
    assert!(req.response_format.is_some());
}

#[tokio::test]
async fn retry_exhausted_rejects() {
    let host = Arc::new(FakeHost::default());
    for _ in 0..5 {
        host.replies.lock().unwrap().push(response("immer noch kein JSON"));
    }
    let g = guard_with(json!({"action": "retry", "maxRetries": 2}), host.clone());
    let i = info(false);
    let mut meta = retry_meta(&g, &i, request(json_schema(schema())));
    let (flow, ..) = run_response(&g, &i, &mut meta, response("nope")).await;
    assert!(matches!(flow, Flow::Block(_)), "{flow:?}");
    assert_eq!(host.seen.lock().unwrap().len(), 2);
    assert_eq!(i.plugin_calls.take().len(), 2);
    assert_eq!(
        hits(&i),
        [
            ("invalid-json".into(), "retry", 2),
            ("invalid-json".into(), "reject", 1)
        ]
    );
    // Die zweite Wiederholung enthält beide Fehlversuche.
    assert_eq!(host.seen.lock().unwrap()[1].1.messages.len(), 5);
}

#[tokio::test]
async fn retry_host_error_and_missing_request_reject() {
    // Host ohne Antwort.
    let g = guard_with(json!({"action": "retry"}), Arc::new(FakeHost::default()));
    let i = info(false);
    let mut meta = retry_meta(&g, &i, request(json_schema(schema())));
    let (flow, ..) = run_response(&g, &i, &mut meta, response("nope")).await;
    assert!(matches!(flow, Flow::Block(_)));
    // Ohne endgültige Anfrage (z. B. Pipeline ohne Unterstützung): ablehnen statt raten.
    let (_, mut meta) = run_request(&g, &i, request(json_schema(schema())));
    let (flow, ..) = run_response(&g, &i, &mut meta, response("nope")).await;
    assert!(matches!(flow, Flow::Block(_)));
}

#[tokio::test]
async fn retry_tool_call_sends_tool_results_and_rejects_unknown_tools() {
    let host = Arc::new(FakeHost::default());
    host.replies
        .lock()
        .unwrap()
        .push(tool_response("person", "{\"name\": \"Ada\", \"age\": 1}"));
    let g = guard_with(json!({"action": "retry"}), host.clone());
    let i = info(false);
    let req = tool_request(Some(ToolChoice::Tool {
        name: "person".into(),
    }));
    let mut meta = retry_meta(&g, &i, req.clone());
    let (flow, ..) = run_response(&g, &i, &mut meta, tool_response("person", "{}")).await;
    assert!(matches!(flow, Flow::Respond(_)), "{flow:?}");
    let seen = host.seen.lock().unwrap();
    let msgs = &seen[0].1.messages;
    assert_eq!(msgs[2].role, Role::Tool);
    let Content::Parts(p) = &msgs[2].content else {
        panic!()
    };
    assert!(matches!(&p[0].kind, PartKind::ToolResult { tool_call_id, is_error: Some(true), .. } if tool_call_id == "call_1"));
    drop(seen);
    // Wiederholung ruft ein nicht angebotenes Tool auf → ungültig.
    host.replies
        .lock()
        .unwrap()
        .push(tool_response("rm_rf", "{}"));
    let i = info(false);
    let mut meta = retry_meta(&g, &i, req);
    let (flow, ..) = run_response(&g, &i, &mut meta, tool_response("person", "{}")).await;
    assert!(matches!(flow, Flow::Block(_)), "{flow:?}");
    assert!(hits(&i).contains(&("unknown-tool".into(), "reject", 1)));
}

/// Inneres Ausgabe-Plugin: zählt Aufrufe und merkt sich den zuletzt gesehenen Text.
#[derive(Debug, Default)]
struct Inner {
    seen: Mutex<Vec<String>>,
}

impl Plugin for Inner {
    fn name(&self) -> &'static str {
        "inner"
    }

    fn phases(&self) -> Phases {
        Phases::OUTPUT
    }

    fn on_response<'a>(&'a self, ctx: &'a mut RespCtx<'_>) -> Hook<'a, Flow> {
        self.seen
            .lock()
            .unwrap()
            .push(ctx.response.choices[0].message.content.text());
        Hook::ok(Flow::Continue)
    }
}

#[tokio::test]
async fn retry_reruns_inner_output_plugins() {
    let host = Arc::new(FakeHost::default());
    host.replies
        .lock()
        .unwrap()
        .push(response("{\"name\": \"Ada\", \"age\": 36}"));
    let g = guard_with(json!({"action": "retry"}), host);
    let inner = Arc::new(Inner::default());
    // Reihenfolge der Anfrage: schema-guard außen, inner innen (Antwort: inner zuerst).
    let chain = Chain::new(vec![
        Arc::new(Entry::new("sg", g.clone())),
        Arc::new(Entry::new("inner", inner.clone())),
    ]);
    let i = info(false);
    let mut meta = retry_meta(&g, &i, request(json_schema(schema())));
    let mut headers = ResponseHeaders::default();
    let mut ctx = RespCtx {
        info: &i,
        response: response("kaputt"),
        response_headers: &mut headers,
        meta: &mut meta,
    };
    assert_eq!(chain.run_response(&mut ctx).await, Flow::Continue);
    assert_eq!(
        ctx.response.choices[0].message.content.text(),
        "{\"name\": \"Ada\", \"age\": 36}"
    );
    assert_eq!(
        *inner.seen.lock().unwrap(),
        ["kaputt", "{\"name\": \"Ada\", \"age\": 36}"]
    );
}

#[tokio::test]
async fn retry_with_multiple_choices_rejects() {
    let host = Arc::new(FakeHost::default());
    let g = guard_with(json!({"action": "retry"}), host.clone());
    let i = info(false);
    let mut req = request(json_schema(schema()));
    req.n = Some(2);
    let mut meta = retry_meta(&g, &i, req);
    let mut resp = response("nope");
    resp.choices.push(resp.choices[0].clone());
    let (flow, ..) = run_response(&g, &i, &mut meta, resp).await;
    assert!(matches!(flow, Flow::Block(_)));
    assert!(host.seen.lock().unwrap().is_empty());
}

// ---------------------------------------------------------------------------------------------
// Stream (live)
// ---------------------------------------------------------------------------------------------

fn stream_run(cfg: Value, req: ChatRequest, events: Vec<StreamEvent>) -> (Vec<StreamEvent>, CallInfo) {
    let g = guard(cfg);
    let i = info(true);
    let (_, meta) = run_request(&g, &i, req);
    let i = Arc::new(i);
    let chain = Arc::new(Chain::new(vec![Arc::new(Entry::new("sg", g))]));
    let mut runner = ChunkRunner::new(chain, Arc::clone(&i), meta, ChunkMode::Live);
    let mut out = Vec::new();
    for ev in events {
        runner.push(ev, &mut out);
    }
    runner.finish(&mut out);
    let info = Arc::try_unwrap(i).unwrap_or_else(|a| (*a).clone());
    (out, info)
}

fn text_stream(parts: &[&str]) -> Vec<StreamEvent> {
    let mut v = vec![StreamEvent::Start {
        id: "s".into(),
        model: "m".into(),
        created: None,
    }];
    for p in parts {
        v.push(StreamEvent::TextDelta {
            index: 0,
            text: (*p).into(),
        });
    }
    v.push(StreamEvent::Stop {
        reason: StopReason::EndTurn,
        stop_sequence: None,
    });
    v.push(StreamEvent::Usage(Usage::default()));
    v
}

fn stop_reason(out: &[StreamEvent]) -> Option<StopReason> {
    out.iter().find_map(|e| match e {
        StreamEvent::Stop { reason, .. } => Some(reason.clone()),
        _ => None,
    })
}

#[test]
fn live_stream_checked_at_stop() {
    let req = || request(json_schema(schema()));
    let (out, i) = stream_run(json!({}), req(), text_stream(&["{\"name\":", " \"Ada\", \"age\"", ": 3}"]));
    assert_eq!(stop_reason(&out), Some(StopReason::EndTurn));
    assert!(hits(&i).is_empty());
    let (out, i) = stream_run(json!({}), req(), text_stream(&["{\"name\":", " 5}"]));
    assert_eq!(stop_reason(&out), Some(StopReason::ContentFilter));
    assert_eq!(hits(&i), [("schema-mismatch".into(), "reject", 1)]);
    // flag: Stream unverändert, nur gezählt.
    let (out, i) = stream_run(json!({"action": "flag"}), req(), text_stream(&["x"]));
    assert_eq!(stop_reason(&out), Some(StopReason::EndTurn));
    assert_eq!(hits(&i), [("invalid-json".into(), "flag", 1)]);
    // Ohne strukturierte Ausgabe: nichts zu tun.
    let (out, i) = stream_run(json!({}), request(None), text_stream(&["x"]));
    assert_eq!(stop_reason(&out), Some(StopReason::EndTurn));
    assert!(hits(&i).is_empty());
}

#[test]
fn live_stream_tool_arguments() {
    let req = tool_request(Some(ToolChoice::Tool {
        name: "person".into(),
    }));
    let events = |args: &[&str]| {
        let mut v = vec![StreamEvent::ToolCallStart(ToolCallStart {
            index: 0,
            id: "c1".into(),
            name: "person".into(),
        })];
        for a in args {
            v.push(StreamEvent::ToolCallDelta {
                index: 0,
                arguments: (*a).into(),
            });
        }
        v.push(StreamEvent::Stop {
            reason: StopReason::ToolUse,
            stop_sequence: None,
        });
        v
    };
    let (out, _) = stream_run(json!({}), req.clone(), events(&["{\"name\":\"A\",", "\"age\":1}"]));
    assert_eq!(stop_reason(&out), Some(StopReason::ToolUse));
    let (out, i) = stream_run(json!({}), req, events(&["{\"age\":", "\"x\"}"]));
    assert_eq!(stop_reason(&out), Some(StopReason::ContentFilter));
    assert_eq!(hits(&i), [("tool-arguments".into(), "reject", 1)]);
}

#[test]
fn registered_for_admin_and_gitops_validation() {
    let reg = crate::Registry::for_validation(&aigw_core::upstream::UpstreamPolicy::strict());
    let obj = |v: Value| v.as_object().unwrap().clone();
    assert!(reg.validate("schema-guard", &obj(json!({"action": "retry", "maxRetries": 2}))).is_ok());
    assert!(reg.validate("schema-guard", &obj(json!({"action": "retry", "maxRetries": 9}))).is_err());
    let eff = reg.effective("schema-guard", &obj(json!({}))).unwrap();
    assert_eq!(eff.fail_mode, crate::FailMode::Closed);
    assert_eq!(eff.timeout, crate::binding::MAX_TIMEOUT);
    assert!(crate::Registry::with_builtin().names().any(|n| n == "schema-guard"));
}
