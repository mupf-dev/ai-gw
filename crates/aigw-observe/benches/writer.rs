// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Durchsatz des Batch-Writers (Einträge/s) gegen SQLite (Datei, WAL-Standard von sqlx) und –
//! mit `AIGW_TEST_POSTGRES_URL` – Postgres.
//!
//! `cargo bench -p aigw-observe --bench writer` (optional `AIGW_BENCH_N=200000`)
//!
//! Gemessen: Zeit vom ersten `record()` bis alle Einträge geschrieben sind (Graceful
//! Shutdown). Der Puffer ist so groß, dass nichts verworfen wird; 50 verschiedene
//! Key/Team/Deployment-Kombinationen über wenige Minuten verteilt (realistische Rollups).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    missing_docs
)]

use std::time::{Duration, Instant, SystemTime};

use aigw_core::config::ObserveConfig;
use aigw_core::ir::Usage;
use aigw_core::observe::{PayloadMode, RawPayload, RequestObserver, RequestRecord};
use aigw_core::resource::{Id, new_id};
use aigw_core::types::ProviderKind;
use aigw_core::wire::WireFormat;
use aigw_observe::ObserveBuilder;
use aigw_store::Store;
use sqlx::AssertSqlSafe;

fn records(n: usize, payload: bool) -> Vec<RequestRecord> {
    let ids: Vec<(Id, Id, Id)> = (0..50).map(|_| (new_id(), new_id(), new_id())).collect();
    let now = SystemTime::now();
    (0..n)
        .map(|i| {
            let (key, team, dep) = ids[i % ids.len()];
            let mut r = RequestRecord::new(new_id().to_string(), WireFormat::OpenAiChat, 200);
            r.started_at = now - Duration::from_millis((i % 180_000) as u64);
            r.api_key_id = Some(key);
            r.team_id = Some(team);
            r.deployment_id = Some(dep);
            r.model_alias = Some("gpt-4o".into());
            r.model_group = Some("gpt-4o".into());
            r.provider = Some(ProviderKind::OpenAi);
            r.provider_model = Some("gpt-4o-2024-08-06".into());
            r.usage = Usage {
                input_tokens: 1200,
                output_tokens: 300,
                ..Usage::default()
            };
            r.duration = Duration::from_millis((i % 3000) as u64);
            r.ttft = Some(Duration::from_millis(100));
            r.stream = true;
            if payload {
                r.payload = Some(Box::new(RawPayload {
                    request_headers: vec![
                        ("authorization".into(), "Bearer sk-aigw-x".into()),
                        ("content-type".into(), "application/json".into()),
                    ],
                    request_body: format!(
                        r#"{{"model":"gpt-4o","messages":[{{"role":"user","content":"Hallo, ich bin user{i}@example.com, IBAN DE89370400440532013000. {}"}}]}}"#,
                        "Lorem ipsum dolor sit amet. ".repeat(30)
                    )
                    .into(),
                    response_body: "Gern geschehen. ".repeat(40).into(),
                    mode: PayloadMode::Masked,
                }));
            }
            r
        })
        .collect()
}

async fn measure(label: &str, store: Store, n: usize, mode: PayloadMode) {
    store.migrate().await.unwrap();
    let recs = records(n, mode.captures());
    let h = ObserveBuilder::new(ObserveConfig {
        queue_capacity: n + 1,
        payload: mode,
        shutdown_timeout_ms: 600_000,
        ..ObserveConfig::default()
    })
    .store(store.clone())
    .start();
    let log = h.logger();
    let metrics = h.metrics().clone();
    let start = Instant::now();
    for r in recs {
        log.record(r);
    }
    let enqueue = start.elapsed();
    assert!(h.shutdown().await);
    let total = start.elapsed();
    let s = metrics.snapshot();
    assert_eq!(s.written as usize, n, "{s:?}");
    assert_eq!(s.dropped, 0);
    println!(
        "{label:<28} payload={:<8} n={n:>7}  {:>9.0} Einträge/s  (gesamt {:>7.1} ms, Batches {}, Einreihen {:.0} ns/Eintrag)",
        mode.as_str(),
        n as f64 / total.as_secs_f64(),
        total.as_secs_f64() * 1e3,
        s.batches,
        enqueue.as_nanos() as f64 / n as f64,
    );
    store.close().await;
}

fn main() {
    let n: usize = std::env::var("AIGW_BENCH_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        for mode in [PayloadMode::Off, PayloadMode::Masked] {
            let dir = std::env::temp_dir().join(format!("aigw-bench-{}", new_id()));
            std::fs::create_dir_all(&dir).unwrap();
            let url = format!("sqlite://{}/aigw.db?mode=rwc", dir.display());
            measure(
                "SQLite (Datei)",
                Store::connect(&url).await.unwrap(),
                n,
                mode,
            )
            .await;
            std::fs::remove_dir_all(&dir).ok();
        }

        match std::env::var("AIGW_TEST_POSTGRES_URL") {
            Ok(base) => {
                for mode in [PayloadMode::Off, PayloadMode::Masked] {
                    let db = format!("aigw_bench_{}", new_id().simple());
                    let admin = sqlx::PgPool::connect(&base).await.unwrap();
                    sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {db}")))
                        .execute(&admin)
                        .await
                        .unwrap();
                    let (prefix, _) = base.rsplit_once('/').unwrap();
                    let store = Store::connect(&format!("{prefix}/{db}")).await.unwrap();
                    measure("Postgres", store, n, mode).await;
                    sqlx::query(AssertSqlSafe(format!("DROP DATABASE {db} WITH (FORCE)")))
                        .execute(&admin)
                        .await
                        .unwrap();
                }
            }
            Err(_) => eprintln!("AIGW_TEST_POSTGRES_URL nicht gesetzt – Postgres übersprungen"),
        }
    });
}
