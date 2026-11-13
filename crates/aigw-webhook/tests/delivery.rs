// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Webhook-Client und Ereignis-Zustellung gegen lokale Fake-Server: Signatur, SSRF-Schutz bei
//! der Namensauflösung, Größenlimit, Wiederholung mit Backoff, Dead Letter, Deduplizierung.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aigw_core::config::AdminConfig;
use aigw_core::resource::SecretString;
use aigw_core::upstream::UpstreamPolicy;
use aigw_webhook::{
    ClientOptions, DispatcherOptions, Event, EventDedup, EventDispatcher, EventMetrics,
    EventTarget, SecretResolver, WebhookClient, WebhookError, sign,
};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use bytes::Bytes;
use http::Uri;
use serde_json::{Value, json};

#[derive(Default)]
struct Server {
    /// Antworten der Reihe nach (danach 200).
    script: Mutex<Vec<StatusCode>>,
    hits: AtomicUsize,
    bodies: Mutex<Vec<(HeaderMap, Bytes)>>,
}

async fn handler(State(s): State<Arc<Server>>, h: HeaderMap, body: Bytes) -> (StatusCode, String) {
    s.hits.fetch_add(1, Ordering::SeqCst);
    s.bodies.lock().unwrap().push((h, body));
    let next = {
        let mut sc = s.script.lock().unwrap();
        if sc.is_empty() {
            StatusCode::OK
        } else {
            sc.remove(0)
        }
    };
    (next, "x".repeat(10))
}

async fn big() -> String {
    "y".repeat(10_000)
}

async fn server(script: Vec<StatusCode>) -> (SocketAddr, Arc<Server>) {
    let s = Arc::new(Server {
        script: Mutex::new(script),
        ..Server::default()
    });
    let app = axum::Router::new()
        .route("/e", post(handler))
        .route("/big", post(big))
        .with_state(Arc::clone(&s));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, s)
}

fn policy(names: &[&str]) -> UpstreamPolicy {
    let v: Vec<String> = names.iter().map(|s| (*s).to_owned()).collect();
    UpstreamPolicy::from_config(&AdminConfig {
        allow_insecure_upstreams: v.clone(),
        allow_private_upstreams: v,
        ..AdminConfig::default()
    })
    .unwrap()
}

fn local_client() -> WebhookClient {
    WebhookClient::new(
        policy(&["127.0.0.1", "localhost"]),
        ClientOptions::default(),
    )
    .unwrap()
}

#[tokio::test]
async fn signed_post_and_response_limit() {
    let (addr, s) = server(vec![]).await;
    let c = local_client();
    let uri: Uri = c.check_url(&format!("http://{addr}/e")).unwrap();
    let secret = SecretString::new("k");
    let r = c
        .post_json(
            &uri,
            HeaderMap::new(),
            Bytes::from_static(b"{\"a\":1}"),
            Some(&secret),
            Duration::from_secs(2),
            1024,
        )
        .await
        .unwrap();
    assert_eq!(r.status, StatusCode::OK);
    let (h, body) = s.bodies.lock().unwrap()[0].clone();
    let sig = h.get(sign::SIGNATURE_HEADER).unwrap().to_str().unwrap();
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    sign::verify(b"k", sig, &body, now, sign::DEFAULT_TOLERANCE).unwrap();
    assert_eq!(h.get("content-type").unwrap(), "application/json");
    assert!(
        h.get("user-agent")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("ai-gw/")
    );

    let uri: Uri = format!("http://{addr}/big").parse().unwrap();
    let e = c
        .post_json(
            &uri,
            HeaderMap::new(),
            Bytes::new(),
            None,
            Duration::from_secs(2),
            100,
        )
        .await
        .unwrap_err();
    assert_eq!(e, WebhookError::ResponseTooLarge(100));
}

#[tokio::test]
async fn ssrf_guard_at_resolution() {
    let (addr, s) = server(vec![]).await;
    // `localhost` löst auf 127.0.0.1 auf – ohne Freischaltung lehnt der Resolver ab, auch
    // wenn die URL-Prüfung umgangen wird.
    let strict = WebhookClient::new(policy(&[]), ClientOptions::default()).unwrap();
    assert!(
        strict
            .check_url(&format!("http://localhost:{}/e", addr.port()))
            .is_err()
    );
    let uri: Uri = format!("http://localhost:{}/e", addr.port())
        .parse()
        .unwrap();
    let e = strict
        .post_json(
            &uri,
            HeaderMap::new(),
            Bytes::new(),
            None,
            Duration::from_secs(2),
            100,
        )
        .await
        .unwrap_err();
    match e {
        WebhookError::Blocked(m) => assert!(m.contains("SSRF"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(s.hits.load(Ordering::SeqCst), 0);
    // Mit Freischaltung des Namens geht es.
    let ok = WebhookClient::new(policy(&["localhost"]), ClientOptions::default()).unwrap();
    let uri = ok
        .check_url(&format!("http://localhost:{}/e", addr.port()))
        .unwrap();
    let r = ok
        .post_json(
            &uri,
            HeaderMap::new(),
            Bytes::new(),
            None,
            Duration::from_secs(2),
            100,
        )
        .await
        .unwrap();
    assert_eq!(r.status, StatusCode::OK);
}

fn target(addr: SocketAddr, attempts: u32) -> EventTarget {
    EventTarget {
        name: "test".into(),
        uri: format!("http://{addr}/e").parse().unwrap(),
        events: vec!["budget.*".into()],
        secret: SecretResolver::Fixed(SecretString::new("k")),
        timeout: Duration::from_secs(2),
        max_attempts: attempts,
    }
}

fn opts() -> DispatcherOptions {
    DispatcherOptions {
        retry_initial: Duration::from_millis(20),
        retry_max: Duration::from_millis(100),
        ..DispatcherOptions::default()
    }
}

async fn wait_for(m: &EventMetrics, f: impl Fn(&EventMetrics) -> bool) {
    for _ in 0..200 {
        if f(m) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("Bedingung nicht erreicht: {m:?}");
}

#[tokio::test]
async fn retry_with_backoff_then_delivered() {
    let (addr, s) = server(vec![
        StatusCode::SERVICE_UNAVAILABLE,
        StatusCode::TOO_MANY_REQUESTS,
    ])
    .await;
    let (d, task) = EventDispatcher::start(local_client(), vec![target(addr, 5)], None, opts());
    assert!(d.emit(Event::new("budget.warning", json!({"percent": 80}))));
    // Kein Empfänger für diesen Typ.
    assert!(d.emit(Event::new("other.thing", json!({}))));
    let m = Arc::clone(d.metrics());
    wait_for(&m, |m| EventMetrics::get(&m.delivered) == 1).await;
    assert_eq!(EventMetrics::get(&m.retries), 2);
    assert_eq!(EventMetrics::get(&m.dead_letter), 0);
    assert_eq!(s.hits.load(Ordering::SeqCst), 3);
    // Gleiche Zustell-ID über alle Versuche, Versuchszähler, gültige Signatur.
    let bodies = s.bodies.lock().unwrap().clone();
    let ids: HashSet<_> = bodies
        .iter()
        .map(|(h, _)| h.get("x-aigw-delivery").unwrap().clone())
        .collect();
    assert_eq!(ids.len(), 1);
    assert_eq!(bodies[2].0.get("x-aigw-attempt").unwrap(), "3");
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    for (h, b) in &bodies {
        let sig = h.get(sign::SIGNATURE_HEADER).unwrap().to_str().unwrap();
        sign::verify(b"k", sig, b, now, sign::DEFAULT_TOLERANCE).unwrap();
        let v: Value = serde_json::from_slice(b).unwrap();
        assert_eq!(v["type"], "budget.warning");
        assert_eq!(v["data"]["percent"], 80);
    }
    task.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn dead_letter_after_max_attempts_and_on_client_error() {
    let (addr, s) = server(vec![StatusCode::INTERNAL_SERVER_ERROR; 10]).await;
    let (d, task) = EventDispatcher::start(local_client(), vec![target(addr, 3)], None, opts());
    d.emit(Event::new("budget.exhausted", json!({})));
    let m = Arc::clone(d.metrics());
    wait_for(&m, |m| EventMetrics::get(&m.dead_letter) == 1).await;
    assert_eq!(s.hits.load(Ordering::SeqCst), 3);
    assert_eq!(EventMetrics::get(&m.delivered), 0);
    task.shutdown(Duration::from_secs(1)).await;

    // 400 wird nicht wiederholt.
    let (addr, s) = server(vec![StatusCode::BAD_REQUEST]).await;
    let (d, task) = EventDispatcher::start(local_client(), vec![target(addr, 5)], None, opts());
    d.emit(Event::new("budget.exhausted", json!({})));
    let m = Arc::clone(d.metrics());
    wait_for(&m, |m| EventMetrics::get(&m.dead_letter) == 1).await;
    assert_eq!(s.hits.load(Ordering::SeqCst), 1);
    task.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn full_queue_drops_without_blocking() {
    let (addr, _) = server(vec![]).await;
    let o = DispatcherOptions {
        queue_capacity: 1,
        ..opts()
    };
    let (d, task) = EventDispatcher::start(local_client(), vec![target(addr, 1)], None, o);
    let mut accepted = 0;
    for _ in 0..50 {
        accepted += usize::from(d.emit(Event::new("budget.warning", json!({}))));
    }
    assert!(accepted < 50);
    assert!(EventMetrics::get(&d.metrics().dropped) > 0);
    task.shutdown(Duration::from_secs(2)).await;
}

/// Dedup im RAM (die DB-Variante testet `aigw-store`).
#[derive(Debug, Default)]
struct MemDedup(Mutex<HashSet<String>>);

#[async_trait::async_trait]
impl EventDedup for MemDedup {
    async fn claim(&self, key: &str, _: &str, _: i64, _: i64) -> Result<bool, String> {
        Ok(self.0.lock().unwrap().insert(key.to_owned()))
    }
}

#[tokio::test]
async fn dedup_across_dispatchers() {
    let (addr, s) = server(vec![]).await;
    let dedup: Arc<dyn EventDedup> = Arc::new(MemDedup::default());
    let (a, ta) = EventDispatcher::start(
        local_client(),
        vec![target(addr, 1)],
        Some(Arc::clone(&dedup)),
        opts(),
    );
    let (b, tb) =
        EventDispatcher::start(local_client(), vec![target(addr, 1)], Some(dedup), opts());
    let ev = || Event::new("budget.warning", json!({})).with_dedup("k1", i64::MAX);
    a.emit(ev());
    b.emit(ev());
    a.emit(Event::new("budget.warning", json!({})).with_dedup("k2", i64::MAX));
    wait_for(a.metrics(), |_| {
        EventMetrics::get(&a.metrics().delivered) + EventMetrics::get(&b.metrics().delivered) == 2
            && EventMetrics::get(&a.metrics().deduplicated)
                + EventMetrics::get(&b.metrics().deduplicated)
                == 1
    })
    .await;
    assert_eq!(s.hits.load(Ordering::SeqCst), 2);
    ta.shutdown(Duration::from_secs(1)).await;
    tb.shutdown(Duration::from_secs(1)).await;
}
