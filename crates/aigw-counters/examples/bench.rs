// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Mikro-Benchmark: Kosten von `reserve` + `settle` je Backend (über `dyn CounterBackend`).
//!
//! ```sh
//! cargo run --release -p aigw-counters --example bench
//! # optional zusätzlich:
//! AIGW_TEST_POSTGRES_URL=postgres://… AIGW_TEST_REDIS_URL=redis://… cargo run --release …
//! ```

#![allow(clippy::print_stdout, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use aigw_counters::{
    CounterBackend, LimitCheck, LocalCounters, PgSyncCounters, PgSyncOptions, RedisCounters,
    RedisOptions, Window,
};

const KEYS: u64 = 1_000;

/// Eine Anfrage: Key + Team + Org + Modell (Tokens) und Anfragezähler – 5 Limits.
async fn one(c: &dyn CounterBackend, i: u64, multi: bool) {
    let key = format!("key:{}", i % KEYS);
    let team = format!("team:{}", i % 16);
    let checks_multi = [
        LimitCheck::new(&key, Window::Minute, u64::MAX / 4, 1_000),
        LimitCheck::new(&team, Window::Minute, u64::MAX / 4, 1_000),
        LimitCheck::new("org:1", Window::Day, u64::MAX / 4, 1_000),
        LimitCheck::new("model:gpt-4o", Window::Minute, u64::MAX / 4, 1_000),
        LimitCheck::fixed(&key, Window::Second, u64::MAX / 4, 1),
    ];
    let checks: &[LimitCheck<'_>] = if multi {
        &checks_multi
    } else {
        &checks_multi[..1]
    };
    let r = c.reserve(checks).await.unwrap();
    c.settle(r, 700).await;
}

async fn sequential(c: &Arc<dyn CounterBackend>, n: u64, multi: bool) -> Duration {
    let t = Instant::now();
    for i in 0..n {
        one(c.as_ref(), i, multi).await;
    }
    t.elapsed() / u32::try_from(n).unwrap()
}

/// Durchsatz mit `tasks` nebenläufigen Tasks; liefert Operationen/s.
async fn parallel(c: &Arc<dyn CounterBackend>, tasks: u64, per_task: u64, multi: bool) -> f64 {
    let t = Instant::now();
    let handles: Vec<_> = (0..tasks)
        .map(|k| {
            let c = c.clone();
            tokio::spawn(async move {
                for i in 0..per_task {
                    one(c.as_ref(), k * per_task + i, multi).await;
                }
            })
        })
        .collect();
    for h in handles {
        h.await.unwrap();
    }
    (tasks * per_task) as f64 / t.elapsed().as_secs_f64()
}

async fn report(name: &str, c: Arc<dyn CounterBackend>, n_seq: u64, per_task: u64) {
    // Aufwärmen (Zähler anlegen, Skripte laden).
    sequential(&c, KEYS, true).await;
    let s1 = sequential(&c, n_seq, false).await;
    let s5 = sequential(&c, n_seq, true).await;
    let p5 = parallel(&c, 64, per_task, true).await;
    println!(
        "| {name} | {:.2} µs | {:.2} µs | {:.0} |",
        s1.as_nanos() as f64 / 1_000.0,
        s5.as_nanos() as f64 / 1_000.0,
        p5
    );
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    println!("Kerne: {threads}");
    println!(
        "| Backend | reserve+settle, 1 Limit | reserve+settle, 5 Limits | 64 Tasks, 5 Limits [Anfragen/s] |"
    );
    println!("|---|---|---|---|");

    report("local", Arc::new(LocalCounters::new()), 200_000, 20_000).await;

    if let Ok(base) = std::env::var("AIGW_TEST_POSTGRES_URL") {
        let pool = sqlx::PgPool::connect(&base).await.unwrap();
        aigw_store::migrator().run(&pool).await.unwrap();
        let pg = Arc::new(PgSyncCounters::new_manual(pool, PgSyncOptions::default()));
        report("postgres-sync", pg.clone(), 200_000, 20_000).await;
        // Abgleichkosten bei vielen aktiven Zählern (Zähler aus dem Lauf oben).
        let t = Instant::now();
        let rep = pg.sync_now().await.unwrap();
        let first = t.elapsed();
        let t = Instant::now();
        let rep2 = pg.sync_now().await.unwrap();
        println!(
            "\npostgres-sync Abgleich: {} Deltas + {} Buckets in {:.1} ms; ohne Deltas {} Buckets in {:.1} ms",
            rep.written,
            rep.read,
            first.as_secs_f64() * 1e3,
            rep2.read,
            t.elapsed().as_secs_f64() * 1e3
        );
    } else {
        println!("| postgres-sync | (AIGW_TEST_POSTGRES_URL fehlt) | | |");
    }

    if let Ok(url) = std::env::var("AIGW_TEST_REDIS_URL") {
        let prefix = format!("aigwbench:{}:", uuid::Uuid::now_v7().simple());
        let r = RedisCounters::connect(
            &url,
            RedisOptions {
                key_prefix: prefix,
                timeout: Duration::from_secs(2),
                ..RedisOptions::default()
            },
        )
        .await
        .unwrap();
        assert!(!r.is_degraded());
        report("redis", r, 10_000, 500).await;
    } else {
        println!("| redis | (AIGW_TEST_REDIS_URL fehlt) | | |");
    }
}
