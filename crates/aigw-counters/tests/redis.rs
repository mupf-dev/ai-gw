// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Backend `redis` gegen einen echten Redis/Valkey-Server.
//!
//! Läuft nur, wenn `AIGW_TEST_REDIS_URL` gesetzt ist (`scripts/check.sh` mit
//! `AIGW_TEST_REDIS=docker`). Jeder Test verwendet ein eigenes Schlüsselpräfix.
//! Der Rückfalltest ohne Server läuft immer.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stderr,
    unused_must_use
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use aigw_counters::{CounterBackend, Health, LimitCheck, RedisCounters, RedisOptions, Window};

fn url() -> Option<String> {
    let url = std::env::var("AIGW_TEST_REDIS_URL").ok();
    if url.is_none() {
        eprintln!("AIGW_TEST_REDIS_URL nicht gesetzt – Redis-Test übersprungen");
    }
    url
}

fn opts(prefix: &str) -> RedisOptions {
    RedisOptions {
        key_prefix: prefix.to_owned(),
        timeout: Duration::from_secs(2),
        ..RedisOptions::default()
    }
}

async fn pair(url: &str) -> (Arc<RedisCounters>, Arc<RedisCounters>) {
    let prefix = format!("aigwtest:{}:", uuid::Uuid::now_v7().simple());
    let a = RedisCounters::connect(url, opts(&prefix)).await.unwrap();
    let b = RedisCounters::connect(url, opts(&prefix)).await.unwrap();
    assert!(!a.is_degraded(), "{:?}", a.health());
    (a, b)
}

#[tokio::test]
async fn shared_exact_counts_and_settle() {
    let Some(url) = url() else { return };
    let (a, b) = pair(&url).await;
    let lim = |amount| [LimitCheck::new("team:x", Window::Hour, 100, amount)];

    let ra = a.reserve(&lim(60)).await.unwrap();
    assert!(!ra.is_local_fallback());
    assert_eq!(ra.quotas()[0].remaining, 40);
    let rej = b.reserve(&lim(41)).await.unwrap_err();
    assert_eq!(rej.used, 60);
    assert!(rej.retry_after.is_some());
    b.reserve(&lim(40)).await.unwrap();
    assert_eq!(a.get("team:x", Window::Hour).await, 100);

    a.settle(ra, 20).await;
    assert_eq!(b.get("team:x", Window::Hour).await, 60);
    let rb = b.reserve(&lim(10)).await.unwrap();
    b.settle(rb, 25).await; // mehr als reserviert
    assert_eq!(a.get("team:x", Window::Hour).await, 85); // 20 + 40 + 25

    let never = a.reserve(&lim(101)).await.unwrap_err();
    assert_eq!(never.retry_after, None);
}

#[tokio::test]
async fn multi_key_all_or_nothing() {
    let Some(url) = url() else { return };
    let (a, b) = pair(&url).await;
    a.reserve(&[LimitCheck::new("team", Window::Hour, 100, 95)])
        .await
        .unwrap();
    let checks = |amount| {
        [
            LimitCheck::new("key", Window::Hour, 1_000, amount),
            LimitCheck::new("team", Window::Hour, 100, amount),
            LimitCheck::fixed("org:req", Window::Day, 1_000, 1),
        ]
    };
    let rej = b.reserve(&checks(10)).await.unwrap_err();
    assert_eq!(rej.index, 1);
    assert_eq!(b.get("key", Window::Hour).await, 0);
    assert_eq!(b.get("org:req", Window::Day).await, 0);

    let r = b.reserve(&checks(5)).await.unwrap();
    assert_eq!(r.tightest().unwrap().0, 1);
    b.settle(r, 2).await; // org:req ist fixed und bleibt bei 1
    assert_eq!(a.get("key", Window::Hour).await, 2);
    assert_eq!(a.get("team", Window::Hour).await, 97);
    assert_eq!(a.get("org:req", Window::Day).await, 1);

    // Doppelter Schlüssel in einem Aufruf zählt kumulativ.
    let dup = [
        LimitCheck::new("dup", Window::Hour, 10, 6),
        LimitCheck::new("dup", Window::Hour, 10, 6),
    ];
    assert!(a.reserve(&dup).await.is_err());
    assert_eq!(a.get("dup", Window::Hour).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_tasks_two_instances_never_exceed_limit() {
    let Some(url) = url() else { return };
    let (a, b) = pair(&url).await;
    let granted = Arc::new(AtomicU64::new(0));
    let mut tasks = Vec::new();
    for t in 0..64u64 {
        let c = if t % 2 == 0 { a.clone() } else { b.clone() };
        let granted = granted.clone();
        tasks.push(tokio::spawn(async move {
            for i in 0..100u64 {
                let key = format!("key{}", t % 8);
                let amount = 1 + (t + i) % 5;
                let checks = [
                    LimitCheck::new(&key, Window::Hour, 1_000, amount),
                    LimitCheck::new("team", Window::Hour, 3_000, amount),
                ];
                if let Ok(r) = c.reserve(&checks).await {
                    granted.fetch_add(amount, Ordering::Relaxed);
                    assert!(!r.is_local_fallback());
                }
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let team = a.get("team", Window::Hour).await;
    assert_eq!(team, granted.load(Ordering::Relaxed));
    assert!(team <= 3_000, "Limit überschritten: {team}");
    assert!(team > 3_000 - 5, "Limit nicht ausgeschöpft: {team}");
}

#[tokio::test]
async fn falls_back_and_recovers() {
    let Some(url) = url() else { return };
    let prefix = format!("aigwtest:{}:", uuid::Uuid::now_v7().simple());
    let c = RedisCounters::connect(
        &url,
        RedisOptions {
            key_prefix: prefix,
            timeout: Duration::from_millis(100),
            reconnect_interval: Duration::from_millis(100),
            ..RedisOptions::default()
        },
    )
    .await
    .unwrap();
    assert!(!c.is_degraded());

    // Alle Clients für 600 ms anhalten → Zeitüberschreitung → lokaler Rückfall.
    let admin = redis::Client::open(url.as_str()).unwrap();
    let mut con = admin.get_multiplexed_async_connection().await.unwrap();
    let _: () = redis::cmd("CLIENT")
        .arg("PAUSE")
        .arg(600)
        .arg("ALL")
        .query_async(&mut con)
        .await
        .unwrap();
    let r = c
        .reserve(&[LimitCheck::new("k", Window::Minute, 10, 3)])
        .await
        .unwrap();
    assert!(r.is_local_fallback());
    assert!(matches!(c.health(), Health::Degraded(_)));
    c.settle(r, 1).await; // lokal gebucht → lokal abgeglichen

    // Nach der Pause verbindet der Monitor wieder.
    for _ in 0..50 {
        if !c.is_degraded() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!c.is_degraded(), "{:?}", c.health());
    let r = c
        .reserve(&[LimitCheck::new("k", Window::Minute, 10, 3)])
        .await
        .unwrap();
    assert!(!r.is_local_fallback());
}

#[tokio::test]
async fn unreachable_redis_counts_locally() {
    // Braucht keinen Server: Port 1 ist nicht erreichbar.
    let c = RedisCounters::connect(
        "redis://127.0.0.1:1/0",
        RedisOptions {
            connect_timeout: Duration::from_millis(300),
            ..RedisOptions::default()
        },
    )
    .await
    .unwrap();
    assert!(c.is_degraded());
    assert!(matches!(c.health(), Health::Degraded(_)));
    let r = c
        .reserve(&[LimitCheck::new("k", Window::Minute, 10, 7)])
        .await
        .unwrap();
    assert!(r.is_local_fallback());
    assert!(
        c.reserve(&[LimitCheck::new("k", Window::Minute, 10, 4)])
            .await
            .is_err()
    );
    c.settle(r, 2).await;
    assert_eq!(c.get("k", Window::Minute).await, 2);
    assert!(
        RedisCounters::connect("kein-url", RedisOptions::default())
            .await
            .is_err()
    );
}
