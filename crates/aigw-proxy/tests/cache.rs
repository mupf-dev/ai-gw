// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Antwort-Cache im Hot Path (AIGW-39/40): Header `x-aigw-cache`, Wiedergabe als Stream im
//! Eingangsformat, Client-Steuerung, Regeln für das Speichern, Usage 0 bei Treffern,
//! semantische Treffer über einen Test-Embedder.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use aigw_cache::Embedder;
use aigw_cache::vector::memory::{MemoryIndex, MemoryOptions};
use aigw_core::config::CacheConfig;
use aigw_core::ir::{ChatRequest, ChatResponse, UpstreamError};
use aigw_core::observe::{CacheOutcome, RequestObserver, RequestRecord};
use aigw_core::wire::sse::SseParser;
use aigw_plugins::{Chain, Entry, PluginChains, PluginFactory, builtin};
use aigw_proxy::mock::MockProvider;
use aigw_proxy::{
    BoxFuture, CACHE_HEADER, ChatProvider, Describe, EventStream, ProxyState, ResponseCache,
    StaticRoutes,
};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{HeaderMap, Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------------------------
// Hilfen

#[derive(Debug)]
struct Counting {
    mock: Arc<MockProvider>,
    calls: AtomicUsize,
}

impl ChatProvider for Counting {
    fn describe(&self) -> Describe<'_> {
        self.mock.describe()
    }

    fn chat(self: Arc<Self>, req: ChatRequest) -> BoxFuture<Result<ChatResponse, UpstreamError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Arc::clone(&self.mock).chat(req)
    }

    fn chat_stream(
        self: Arc<Self>,
        req: ChatRequest,
    ) -> BoxFuture<Result<EventStream, UpstreamError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Arc::clone(&self.mock).chat_stream(req)
    }
}

#[derive(Debug, Default)]
struct Records(Mutex<Vec<RequestRecord>>);

impl RequestObserver for Records {
    fn record(&self, r: RequestRecord) {
        self.0.lock().unwrap().push(r);
    }
}

impl Records {
    fn last(&self) -> RequestRecord {
        self.0
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("kein Datensatz")
    }
}

/// Test-Embedding (Wortzähler über 64 Buckets).
#[derive(Debug)]
struct WordEmbedder;

#[async_trait::async_trait]
impl Embedder for WordEmbedder {
    async fn embed(&self, _model: &str, text: &str) -> Result<Vec<f32>, String> {
        let mut v = vec![0f32; 64];
        for w in text
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
        {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in w.bytes() {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x100_0000_01b3);
            }
            v[(h % 64) as usize] += 1.0;
        }
        Ok(v)
    }
}

struct Setup {
    addr: SocketAddr,
    upstream: Arc<Counting>,
    records: Arc<Records>,
    cache: Arc<ResponseCache>,
}

async fn setup(policy: Option<Value>, semantic: bool) -> Setup {
    let upstream = Arc::new(Counting {
        mock: Arc::new(MockProvider::new()),
        calls: AtomicUsize::new(0),
    });
    let mut routes = StaticRoutes::new();
    routes.set_fallback(Arc::clone(&upstream) as Arc<dyn ChatProvider>);
    let mut cache = ResponseCache::new(&CacheConfig {
        replay_chunk_chars: 4,
        ..CacheConfig::default()
    });
    if semantic {
        cache = cache.with_memory_index(Arc::new(MemoryIndex::new(MemoryOptions::default())));
    }
    let cache = Arc::new(cache);
    if semantic {
        cache.set_embedder(Arc::new(WordEmbedder));
    }
    let chain = match policy {
        Some(p) => Chain::new(vec![Arc::new(Entry::new(
            "cache",
            builtin::cache::Factory.build(&p).unwrap(),
        ))]),
        None => Chain::new(Vec::new()),
    };
    let records = Arc::new(Records::default());
    let state = ProxyState {
        plugins: Arc::new(PluginChains::fixed(chain)),
        cache: Some(Arc::clone(&cache)),
        observer: records.clone(),
        ..ProxyState::with_routes(Arc::new(routes))
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, aigw_proxy::router(state))
            .await
            .unwrap();
    });
    Setup {
        addr,
        upstream,
        records,
        cache,
    }
}

impl Setup {
    fn calls(&self) -> usize {
        self.upstream.calls.load(Ordering::SeqCst)
    }
}

struct Resp {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Resp {
    fn cache(&self) -> Option<&str> {
        self.headers.get(CACHE_HEADER).and_then(|v| v.to_str().ok())
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }

    /// (Text, Anzahl Text-Chunks) eines SSE-Streams (OpenAI oder Anthropic).
    fn sse_text(&self) -> (String, usize) {
        let mut out = String::new();
        let mut n = 0;
        for f in SseParser::new().push(&self.body) {
            let Ok(v) = serde_json::from_str::<Value>(&f.data) else {
                continue;
            };
            if let Some(t) = v["choices"][0]["delta"]["content"].as_str() {
                out.push_str(t);
                n += 1;
            }
            if v["type"] == "content_block_delta"
                && let Some(t) = v["delta"]["text"].as_str()
            {
                out.push_str(t);
                n += 1;
            }
        }
        (out, n)
    }
}

async fn post(addr: SocketAddr, path: &str, headers: &[(&str, &str)], body: Value) -> Resp {
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let mut req = Request::builder()
        .method(Method::POST)
        .uri(format!("http://{addr}{path}"))
        .header("content-type", "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let res = client
        .request(
            req.body(Full::new(Bytes::from(serde_json::to_vec(&body).unwrap())))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    Resp {
        status,
        headers,
        body,
    }
}

fn openai(text: &str, stream: bool) -> Value {
    json!({"model": "mock", "stream": stream, "max_tokens": 100, "temperature": 0,
           "messages": [{"role": "system", "content": "Sei knapp."}, {"role": "user", "content": text}]})
}

fn anthropic(text: &str, stream: bool) -> Value {
    json!({"model": "mock", "stream": stream, "max_tokens": 100, "temperature": 0,
           "system": "Sei knapp.",
           "messages": [{"role": "user", "content": text}]})
}

fn openai_text(v: &Value) -> String {
    v["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

async fn settle() {
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
}

// ---------------------------------------------------------------------------------------------
// Tests

#[tokio::test]
async fn exact_hit_skips_upstream_and_counts_no_tokens() {
    let s = setup(Some(json!({})), false).await;
    let a = post(
        s.addr,
        "/v1/chat/completions",
        &[],
        openai("Hallo Welt", false),
    )
    .await;
    assert_eq!(a.status, StatusCode::OK);
    assert_eq!(a.cache(), Some("miss"));
    assert_eq!(s.calls(), 1);
    let rec = s.records.last();
    assert_eq!(rec.cache, Some(CacheOutcome::Miss));
    assert!(rec.usage.total_tokens() > 0);

    let b = post(
        s.addr,
        "/v1/chat/completions",
        &[],
        openai("Hallo Welt", false),
    )
    .await;
    assert_eq!(b.status, StatusCode::OK);
    assert_eq!(b.cache(), Some("hit-exact"));
    assert_eq!(s.calls(), 1, "Treffer darf den Upstream nicht aufrufen");
    assert_eq!(openai_text(&b.json()), openai_text(&a.json()));
    let rec = s.records.last();
    assert_eq!(rec.cache, Some(CacheOutcome::HitExact));
    assert_eq!(
        rec.usage.total_tokens(),
        0,
        "Treffer verbrauchen keine Upstream-Tokens"
    );
    assert!(!rec.usage_estimated);
    assert_eq!(rec.model_group.as_deref(), Some("mock"));
    assert!(rec.deployment_id.is_none());

    // Gleiche Frage im Anthropic-Format: derselbe Eintrag, Antwort im Anthropic-Format.
    let c = post(s.addr, "/v1/messages", &[], anthropic("Hallo Welt", false)).await;
    assert_eq!(c.cache(), Some("hit-exact"));
    assert_eq!(c.json()["type"], "message");
    assert_eq!(
        c.json()["content"][0]["text"],
        openai_text(&a.json()).as_str()
    );
    assert_eq!(s.calls(), 1);
    assert_eq!(s.cache.metrics().lookups(CacheOutcome::HitExact), 2);
}

#[tokio::test]
async fn streamed_answer_is_stored_and_replayed_as_chunked_stream() {
    let s = setup(Some(json!({})), false).await;
    let a = post(
        s.addr,
        "/v1/chat/completions",
        &[],
        openai("Erzähl etwas", true),
    )
    .await;
    assert_eq!(a.cache(), Some("miss"));
    let (text, _) = a.sse_text();
    assert!(!text.is_empty());
    settle().await;

    let b = post(
        s.addr,
        "/v1/chat/completions",
        &[],
        openai("Erzähl etwas", true),
    )
    .await;
    assert_eq!(b.cache(), Some("hit-exact"));
    let (replayed, chunks) = b.sse_text();
    assert_eq!(replayed, text);
    assert!(chunks > 1, "simuliertes Chunking: {chunks}");
    assert!(String::from_utf8_lossy(&b.body).contains("[DONE]"));
    assert_eq!(s.calls(), 1);
    assert_eq!(s.records.last().usage.total_tokens(), 0);

    // Als Anthropic-Stream wiedergegeben.
    let c = post(s.addr, "/v1/messages", &[], anthropic("Erzähl etwas", true)).await;
    assert_eq!(c.cache(), Some("hit-exact"));
    assert_eq!(c.sse_text().0, text);
    assert!(String::from_utf8_lossy(&c.body).contains("message_stop"));
    // Und nicht gestreamt.
    let d = post(
        s.addr,
        "/v1/chat/completions",
        &[],
        openai("Erzähl etwas", false),
    )
    .await;
    assert_eq!(d.cache(), Some("hit-exact"));
    assert_eq!(openai_text(&d.json()), text);
    assert_eq!(s.calls(), 1);
}

#[tokio::test]
async fn temperature_and_client_control() {
    let s = setup(Some(json!({})), false).await;
    // temperature > 0 (bzw. ohne Angabe): umgehen, nichts speichern.
    let mut hot = openai("heiß", false);
    hot["temperature"] = json!(0.7);
    for _ in 0..2 {
        let r = post(s.addr, "/v1/chat/completions", &[], hot.clone()).await;
        assert_eq!(r.cache(), Some("bypass"));
    }
    assert_eq!(s.calls(), 2);
    assert_eq!(s.records.last().cache, Some(CacheOutcome::Bypass));

    // no-cache: nicht nachschlagen, aber speichern.
    let r = post(
        s.addr,
        "/v1/chat/completions",
        &[("cache-control", "no-cache")],
        openai("frisch", false),
    )
    .await;
    assert_eq!(r.cache(), Some("bypass"));
    let r = post(
        s.addr,
        "/v1/chat/completions",
        &[("cache-control", "no-cache")],
        openai("frisch", false),
    )
    .await;
    assert_eq!(r.cache(), Some("bypass"));
    assert_eq!(s.calls(), 4);
    let r = post(s.addr, "/v1/chat/completions", &[], openai("frisch", false)).await;
    assert_eq!(r.cache(), Some("hit-exact"));
    assert_eq!(s.calls(), 4);

    // no-store: nachschlagen, nicht speichern.
    let r = post(
        s.addr,
        "/v1/chat/completions",
        &[("cache-control", "no-store")],
        openai("geheim", false),
    )
    .await;
    assert_eq!(r.cache(), Some("miss"));
    let r = post(s.addr, "/v1/chat/completions", &[], openai("geheim", false)).await;
    assert_eq!(r.cache(), Some("miss"));
    assert_eq!(s.calls(), 6);

    // x-aigw-cache-ttl: 0 → nicht speichern.
    post(
        s.addr,
        "/v1/chat/completions",
        &[("x-aigw-cache-ttl", "0")],
        openai("ttl", false),
    )
    .await;
    let r = post(s.addr, "/v1/chat/completions", &[], openai("ttl", false)).await;
    assert_eq!(r.cache(), Some("miss"));
    // Gleich nochmal: jetzt gespeichert.
    let r = post(s.addr, "/v1/chat/completions", &[], openai("ttl", false)).await;
    assert_eq!(r.cache(), Some("hit-exact"));
}

#[tokio::test]
async fn non_zero_temperature_can_be_allowed() {
    let s = setup(Some(json!({"cacheNonZeroTemperature": true})), false).await;
    let mut hot = openai("heiß", false);
    hot["temperature"] = json!(0.7);
    assert_eq!(
        post(s.addr, "/v1/chat/completions", &[], hot.clone())
            .await
            .cache(),
        Some("miss")
    );
    assert_eq!(
        post(s.addr, "/v1/chat/completions", &[], hot).await.cache(),
        Some("hit-exact")
    );
    // Andere Temperatur = anderer Schlüssel.
    let mut hotter = openai("heiß", false);
    hotter["temperature"] = json!(0.9);
    assert_eq!(
        post(s.addr, "/v1/chat/completions", &[], hotter)
            .await
            .cache(),
        Some("miss")
    );
}

#[tokio::test]
async fn errors_and_tool_calls_are_not_stored() {
    let s = setup(Some(json!({})), false).await;
    let mut err = openai("x", false);
    err["model"] = json!("mock-error");
    for _ in 0..2 {
        let r = post(s.addr, "/v1/chat/completions", &[], err.clone()).await;
        assert!(r.status.is_server_error(), "{}", r.status);
        assert_eq!(r.cache(), Some("miss"));
    }
    // Stream mit Fehler mittendrin.
    let mut serr = openai("abbrechen bitte", true);
    serr["model"] = json!("mock-stream-error");
    for _ in 0..2 {
        let r = post(s.addr, "/v1/chat/completions", &[], serr.clone()).await;
        assert_eq!(r.cache(), Some("miss"));
        settle().await;
    }
    // Tool-Aufrufe.
    let tools = json!({"model": "mock", "max_tokens": 100, "temperature": 0,
        "tools": [{"type": "function", "function": {"name": "wetter", "parameters": {"type": "object"}}}],
        "messages": [{"role": "user", "content": "nutze das tool"}]});
    for _ in 0..2 {
        let r = post(s.addr, "/v1/chat/completions", &[], tools.clone()).await;
        assert_eq!(r.cache(), Some("miss"));
        assert_eq!(r.json()["choices"][0]["finish_reason"], "tool_calls");
    }
    assert_eq!(s.calls(), 6);
    assert!(
        s.cache
            .metrics()
            .skipped_count(aigw_cache::SkipReason::ToolCalls)
            >= 2
    );
    assert!(
        s.cache
            .metrics()
            .skipped_count(aigw_cache::SkipReason::Incomplete)
            >= 2
    );
}

#[tokio::test]
async fn without_binding_there_is_no_cache() {
    let s = setup(None, false).await;
    for _ in 0..2 {
        let r = post(s.addr, "/v1/chat/completions", &[], openai("x", false)).await;
        assert_eq!(r.cache(), None);
    }
    assert_eq!(s.calls(), 2);
    assert_eq!(s.records.last().cache, None);
}

#[tokio::test]
async fn semantic_hit_through_the_pipeline() {
    let s = setup(
        Some(
            json!({"mode": "semantic", "semantic": {"threshold": 0.9, "embeddingModel": "embed"}}),
        ),
        true,
    )
    .await;
    let a = post(
        s.addr,
        "/v1/chat/completions",
        &[],
        openai("Wie spät ist es in Berlin", false),
    )
    .await;
    assert_eq!(a.cache(), Some("miss"));
    for _ in 0..100 {
        if s.cache.metrics().vector_stores() == 1 {
            break;
        }
        settle().await;
    }
    let b = post(
        s.addr,
        "/v1/chat/completions",
        &[],
        openai("wie spät ist es in berlin?", false),
    )
    .await;
    assert_eq!(b.cache(), Some("hit-semantic"));
    assert_eq!(openai_text(&b.json()), openai_text(&a.json()));
    assert_eq!(s.records.last().cache, Some(CacheOutcome::HitSemantic));
    assert_eq!(s.records.last().usage.total_tokens(), 0);
    let c = post(
        s.addr,
        "/v1/chat/completions",
        &[],
        openai("Erkläre Photosynthese", false),
    )
    .await;
    assert_eq!(c.cache(), Some("miss"));
    assert_eq!(s.calls(), 2);
}

#[tokio::test]
async fn routed_embedder_uses_internal_routing() {
    let st = ProxyState::mock();
    let e = aigw_proxy::cache::RoutedEmbedder::for_state(&st);
    let a = e.embed("mock", "Hallo").await.unwrap();
    let b = e.embed("mock", "Hallo").await.unwrap();
    assert!(!a.is_empty());
    assert_eq!(a, b, "deterministisch");
    // Fehler des Upstreams (nach Retries) kommen als Fehler zurück (im Cache: Miss).
    assert!(e.embed("mock-error", "x").await.is_err());
}
