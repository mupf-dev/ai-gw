// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Latenz-Benchmark des Gateways (AIGW-14/15, Ziel Q1/Q4): zusätzliche Latenz gegenüber dem
//! direkten Aufruf eines lokalen Fake-Upstreams.
//!
//! Drei Rollen, als getrennte Prozesse (z. B. mit `taskset` auf getrennte Kerne gelegt):
//!
//! ```text
//! latency upstream 127.0.0.1:9100                    # Fake-OpenAI (JSON + SSE, sofort)
//! latency gateway  127.0.0.1:9200 127.0.0.1:9100     # Gateway, Modell "bench" → Upstream
//! latency load     127.0.0.1:9100 200 20000 json     # direkt
//! latency load     127.0.0.1:9200 200 20000 json     # über das Gateway
//! latency load     127.0.0.1:9200 200 20000 stream   # TTFT gestreamt
//! ```
//!
//! `load` hält `conns` parallele Keep-Alive-Verbindungen und misst je Anfrage die Zeit bis
//! zum ersten Body-Frame (TTFT) und bis zum Ende; Ausgabe p50/p90/p99/p99.9/max in µs.
//!
//! Der End-to-End-Benchmark (`scripts/bench-e2e.sh`, AIGW-57) nutzt `upstream` und `load`
//! gegen das echte `aigw`-Binary (Key-Prüfung, Limits/Budgets, Request-Log).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::panic
)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use aigw_core::resource::{Deployment, DeploymentSpec, ObjectMeta, new_id};
use aigw_core::snapshot::DeploymentCtx;
use aigw_core::types::ProviderKind;
use aigw_providers::{Registry, Upstream, UpstreamClient};
use aigw_proxy::{ProxyState, StaticRoutes};
use axum::Router;
use axum::body::Bytes;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::serve::ListenerExt as _;
use http_body_util::{BodyExt as _, Full};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

const JSON_RESPONSE: &str = r#"{"id":"chatcmpl-b","object":"chat.completion","created":1,"model":"bench","choices":[{"index":0,"message":{"role":"assistant","content":"Hallo Welt, das ist eine kurze Benchmark-Antwort."},"finish_reason":"stop"}],"usage":{"prompt_tokens":20,"completion_tokens":10,"total_tokens":30}}"#;

/// SSE-Antwort mit `chunks` Inhalts-Chunks (Wörter rotierend), Endgrund, Usage und `[DONE]`.
fn sse_events(chunks: usize) -> Vec<Bytes> {
    const WORDS: [&str; 6] = ["Hallo", " Welt,", " das", " ist", " ein", " Stream."];
    let mut out = Vec::with_capacity(chunks + 3);
    for i in 0..chunks {
        let role = if i == 0 { r#""role":"assistant","# } else { "" };
        let w = WORDS[i % WORDS.len()];
        out.push(Bytes::from(format!(
            "data: {{\"id\":\"chatcmpl-b\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"bench\",\"choices\":[{{\"index\":0,\"delta\":{{{role}\"content\":\"{w}\"}}}}]}}\n\n"
        )));
    }
    out.push(Bytes::from_static(b"data: {\"id\":\"chatcmpl-b\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"bench\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"));
    out.push(Bytes::from(format!("data: {{\"id\":\"chatcmpl-b\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"bench\",\"choices\":[],\"usage\":{{\"prompt_tokens\":20,\"completion_tokens\":{chunks},\"total_tokens\":{}}}}}\n\n", chunks + 20)));
    out.push(Bytes::from_static(b"data: [DONE]\n\n"));
    out
}

fn env_num(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Fake-OpenAI-Upstream. `CHUNKS` (Standard 6) Inhalts-Chunks je Stream; mit
/// `CHUNK_DELAY_MS` > 0 kommen sie einzeln im Abstand von so vielen Millisekunden (lange
/// Streams), sonst alle in einem Body.
async fn upstream(addr: SocketAddr) {
    let chunks = usize::try_from(env_num("CHUNKS", 6)).unwrap();
    let delay = std::time::Duration::from_millis(env_num("CHUNK_DELAY_MS", 0));
    let events: &'static [Bytes] = Box::leak(sse_events(chunks).into_boxed_slice());
    let sse: &'static [u8] = Box::leak(events.concat().into_boxed_slice());
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |body: Bytes| async move {
            if !memchr(&body, b"\"stream\":true") {
                return ([("content-type", "application/json")], JSON_RESPONSE).into_response();
            }
            if delay.is_zero() {
                return ([("content-type", "text/event-stream")], sse).into_response();
            }
            let s = futures_util::stream::unfold(0usize, move |i| async move {
                if i >= events.len() {
                    return None;
                }
                if i > 0 {
                    tokio::time::sleep(delay).await;
                }
                Some((Ok::<_, std::convert::Infallible>(events[i].clone()), i + 1))
            });
            (
                [("content-type", "text/event-stream")],
                axum::body::Body::from_stream(s),
            )
                .into_response()
        }),
    );
    let l = nodelay(addr).await;
    axum::serve(l, app).await.unwrap();
}

async fn nodelay(addr: SocketAddr) -> impl axum::serve::Listener<Addr = SocketAddr> {
    // Accept-Warteschlange wie im Binary (4096 statt Tokios 1024) für Tests mit tausenden
    // gleichzeitig geöffneten Verbindungen.
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_reuseaddr(true).unwrap();
    socket.bind(addr).unwrap();
    socket.listen(4096).unwrap().tap_io(|tcp| {
        let _ = tcp.set_nodelay(true);
    })
}

fn memchr(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

async fn gateway(addr: SocketAddr, up: SocketAddr) {
    let dep = DeploymentCtx {
        deployment: Deployment {
            id: new_id(),
            metadata: ObjectMeta::named("bench"),
            spec: DeploymentSpec {
                model_group: "bench".into(),
                provider: ProviderKind::OpenAiCompatible,
                endpoint: Some(format!("http://{up}/v1")),
                provider_model: "bench".into(),
                credential: None,
                weight: 1,
                priority: 0,
                capacity: Default::default(),
                enabled: true,
                settings: Default::default(),
                budget: None,
            },
        },
        credential: None,
    };
    let client = UpstreamClient::unguarded().unwrap();
    let mut routes = StaticRoutes::new();
    routes.insert(
        "bench",
        Arc::new(Upstream::from_deployment(&dep, &Registry::with_builtin(), &client).unwrap()),
    );
    let state = ProxyState::with_routes(Arc::new(routes));
    let l = nodelay(addr).await;
    axum::serve(l, aigw_proxy::router(state)).await.unwrap();
}

fn pct(sorted: &[u64], p: f64) -> u64 {
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i]
}

/// Ergebnis einer Anfrage: Zeit bis zum ersten Body-Frame, Gesamtzeit, oder Fehler
/// (HTTP-Status, 0 = Verbindungsfehler).
type Sample = Result<(std::time::Duration, std::time::Duration), u16>;

/// Lastgenerator. Umgebung: `RATE` (req/s gesamt, sonst Sättigung), `FORMAT=anthropic`
/// (`/v1/messages` statt `/v1/chat/completions`), `AUTH` (API-Key; OpenAI: `Authorization:
/// Bearer`, Anthropic: `x-api-key`), `WARMUP` (Anfragen je Verbindung vorab, Standard 20).
async fn load(addr: SocketAddr, conns: usize, total: usize, stream: bool) {
    let client = Client::builder(TokioExecutor::new())
        .pool_max_idle_per_host(conns)
        .build_http::<Full<Bytes>>();
    let anthropic = std::env::var("FORMAT").is_ok_and(|f| f == "anthropic");
    let auth = std::env::var("AUTH").ok();
    let warmup = usize::try_from(env_num("WARMUP", 20)).unwrap();
    let (path, body) = if anthropic {
        (
            "/v1/messages",
            format!(
                r#"{{"model":"bench","stream":{stream},"max_tokens":64,"system":"Du bist ein hilfreicher Assistent.","messages":[{{"role":"user","content":"Erzähl mir in einem Satz etwas über Rust und warum es für Gateways geeignet ist."}}]}}"#
            ),
        )
    } else {
        (
            "/v1/chat/completions",
            format!(
                r#"{{"model":"bench","stream":{stream},"max_tokens":64,"messages":[{{"role":"system","content":"Du bist ein hilfreicher Assistent."}},{{"role":"user","content":"Erzähl mir in einem Satz etwas über Rust und warum es für Gateways geeignet ist."}}]}}"#
            ),
        )
    };
    let body = Bytes::from(body);
    let uri = format!("http://{addr}{path}");
    let rate: Option<f64> = std::env::var("RATE").ok().and_then(|v| v.parse().ok());
    let run = |n: usize| {
        let mut tasks = Vec::new();
        for task in 0..conns {
            let client = client.clone();
            let body = body.clone();
            let uri = uri.clone();
            let auth = auth.clone();
            tasks.push(tokio::spawn(async move {
                let mut out: Vec<Sample> = Vec::with_capacity(n);
                // RATE (req/s gesamt): jede Verbindung sendet im festen Takt statt so schnell
                // wie möglich – misst Latenz unterhalb der Sättigung. Die Takte der
                // Verbindungen sind gleichmäßig versetzt (sonst kämen Bursts von `conns`
                // gleichzeitigen Anfragen).
                let mut tick = rate.map(|r| {
                    let period = std::time::Duration::from_secs_f64(conns as f64 / r);
                    let offset = period.mul_f64(task as f64 / conns as f64);
                    let mut i =
                        tokio::time::interval_at(tokio::time::Instant::now() + offset, period);
                    i.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    i
                });
                for _ in 0..n {
                    if let Some(t) = tick.as_mut() {
                        t.tick().await;
                    }
                    let mut req =
                        hyper::Request::post(&uri).header("content-type", "application/json");
                    if let Some(k) = &auth {
                        req = if anthropic {
                            req.header("x-api-key", k)
                                .header("anthropic-version", "2023-06-01")
                        } else {
                            req.header("authorization", format!("Bearer {k}"))
                        };
                    }
                    let req = req.body(Full::new(body.clone())).unwrap();
                    let t0 = Instant::now();
                    let res = match client.request(req).await {
                        Ok(r) => r,
                        Err(_) => {
                            out.push(Err(0));
                            continue;
                        }
                    };
                    let status = res.status();
                    let mut b = res.into_body();
                    let mut first = None;
                    let mut broken = false;
                    while let Some(f) = b.frame().await {
                        let Ok(f) = f else {
                            broken = true;
                            break;
                        };
                        if first.is_none() && f.data_ref().is_some_and(|d| !d.is_empty()) {
                            first = Some(t0.elapsed());
                        }
                    }
                    let done = t0.elapsed();
                    out.push(if broken {
                        Err(0)
                    } else if status.is_success() {
                        Ok((first.unwrap_or(done), done))
                    } else {
                        Err(status.as_u16())
                    });
                }
                out
            }));
        }
        tasks
    };
    // Aufwärmen (Verbindungen, Pools, Caches).
    if warmup > 0 {
        for t in run(warmup) {
            t.await.unwrap();
        }
    }
    let per = total.div_ceil(conns);
    let t0 = Instant::now();
    let mut ttft = Vec::new();
    let mut lat = Vec::new();
    let mut errors = std::collections::BTreeMap::<u16, usize>::new();
    for t in run(per) {
        for s in t.await.unwrap() {
            match s {
                Ok((f, d)) => {
                    ttft.push(u64::try_from(f.as_micros()).unwrap());
                    lat.push(u64::try_from(d.as_micros()).unwrap());
                }
                Err(code) => *errors.entry(code).or_default() += 1,
            }
        }
    }
    let secs = t0.elapsed().as_secs_f64();
    ttft.sort_unstable();
    lat.sort_unstable();
    println!(
        "{} Anfragen, {conns} Verbindungen, {:.0} req/s (Ziel {rate:?}), stream={stream}, format={}, Fehler={errors:?}",
        lat.len(),
        lat.len() as f64 / secs,
        if anthropic { "anthropic" } else { "openai" },
    );
    if lat.is_empty() {
        return;
    }
    let show = |name: &str, v: &[u64]| {
        println!(
            "{name:<8} p50={:>6}µs p90={:>6}µs p99={:>6}µs p99.9={:>7}µs max={:>8}µs",
            pct(v, 0.5),
            pct(v, 0.9),
            pct(v, 0.99),
            pct(v, 0.999),
            v[v.len() - 1]
        );
    };
    show("ttfb", &ttft);
    show("gesamt", &lat);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let threads: usize = std::env::var("THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .enable_all()
        .build()
        .unwrap();
    let addr = |i: usize| -> SocketAddr { args[i].parse().unwrap() };
    rt.block_on(async {
        match args.get(1).map(String::as_str) {
            Some("upstream") => upstream(addr(2)).await,
            Some("gateway") => gateway(addr(2), addr(3)).await,
            Some("load") => {
                load(
                    addr(2),
                    args[3].parse().unwrap(),
                    args[4].parse().unwrap(),
                    args[5] == "stream",
                )
                .await;
            }
            _ => panic!("Aufruf: latency upstream|gateway|load …"),
        }
    });
}
