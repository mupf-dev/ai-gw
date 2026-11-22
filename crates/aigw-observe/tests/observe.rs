// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Request-Log Ende-zu-Ende: Batch-Verhalten, Überlauf, Shutdown-Flush, Rollups und
//! Aufbewahrung gegen SQLite und – mit `AIGW_TEST_POSTGRES_URL` – Postgres.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr,
    missing_docs
)]

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aigw_core::config::{ObserveConfig, RetentionConfig};
use aigw_core::ir::{ErrorKind, Usage};
use aigw_core::observe::{CacheOutcome, PayloadMode, RawPayload, RequestObserver, RequestRecord};
use aigw_core::resource::{Id, new_id};
use aigw_core::types::ProviderKind;
use aigw_core::wire::WireFormat;
use aigw_observe::retention::sweep;
use aigw_observe::{ObserveBuilder, ObserveMetrics, RequestLogger};
use aigw_store::Store;
use sqlx::AssertSqlSafe;
use tokio::sync::mpsc;

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

fn at(ms: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(u64::try_from(ms).unwrap())
}

fn rec() -> RequestRecord {
    let mut r = RequestRecord::new(new_id().to_string(), WireFormat::OpenAiChat, 200);
    r.duration = Duration::from_millis(12);
    r.usage = Usage {
        input_tokens: 10,
        output_tokens: 5,
        ..Usage::default()
    };
    r
}

fn cfg(batch_size: usize, flush_ms: u64) -> ObserveConfig {
    ObserveConfig {
        batch_size,
        flush_interval_ms: flush_ms,
        ..ObserveConfig::default()
    }
}

async fn sqlite() -> Store {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    store.migrate().await.unwrap();
    store
}

async fn wait_written(m: &ObserveMetrics, n: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while m.snapshot().written < n {
        assert!(
            tokio::time::Instant::now() < deadline,
            "Zeitüberschreitung: {:?}",
            m.snapshot()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn flushes_at_batch_size() {
    let store = sqlite().await;
    // Zeitgesteuertes Schreiben praktisch aus: nur die Batchgröße zählt.
    let h = ObserveBuilder::new(cfg(10, 600_000))
        .store(store.clone())
        .start();
    let log = h.logger();
    for _ in 0..25 {
        log.record(rec());
    }
    wait_written(h.metrics(), 20).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let s = h.metrics().snapshot();
    assert_eq!((s.written, s.batches), (20, 2), "{s:?}");
    assert_eq!(store.count_request_logs().await.unwrap().0, 20);
    assert!(h.shutdown().await);
    assert_eq!(store.count_request_logs().await.unwrap().0, 25);
}

#[tokio::test]
async fn flushes_after_interval() {
    let store = sqlite().await;
    let h = ObserveBuilder::new(cfg(1000, 50))
        .store(store.clone())
        .start();
    for _ in 0..3 {
        h.logger().record(rec());
    }
    wait_written(h.metrics(), 3).await;
    let s = h.metrics().snapshot();
    assert_eq!((s.written, s.batches, s.dropped), (3, 1, 0));
    assert_eq!(store.count_request_logs().await.unwrap().0, 3);
    assert!(h.shutdown().await);
}

#[tokio::test]
async fn overflow_drops_and_counts() {
    // Kanal ohne lesenden Writer: Kapazität 4, danach wird verworfen – ohne zu warten.
    let metrics = Arc::new(ObserveMetrics::default());
    let (tx, rx) = mpsc::channel(4);
    let log = RequestLogger::new(tx, Arc::clone(&metrics), PayloadMode::Off);
    let start = std::time::Instant::now();
    for _ in 0..10 {
        log.record(rec());
    }
    assert!(start.elapsed() < Duration::from_millis(50));
    assert_eq!(metrics.snapshot().dropped, 6);
    // Geschlossener Kanal (Writer beendet) zählt ebenfalls als verworfen.
    drop(rx);
    log.record(rec());
    assert_eq!(metrics.snapshot().dropped, 7);
    let mut text = String::new();
    metrics.render_prometheus(&mut text);
    assert!(text.contains("aigw_log_dropped_total 7"));
}

#[tokio::test]
async fn overflow_with_running_writer_counts_exactly() {
    let store = sqlite().await;
    let mut c = cfg(1000, 600_000);
    c.queue_capacity = 8;
    let h = ObserveBuilder::new(c).store(store.clone()).start();
    let log = h.logger();
    // Ohne `.await` dazwischen kann der Writer auf diesem (current-thread) Runtime nicht
    // lesen: 8 passen, 92 werden verworfen.
    for _ in 0..100 {
        log.record(rec());
    }
    assert_eq!(h.metrics().snapshot().dropped, 92);
    let metrics = Arc::clone(h.metrics());
    assert!(h.shutdown().await);
    let s = metrics.snapshot();
    assert_eq!((s.written, s.dropped), (8, 92));
    assert_eq!(store.count_request_logs().await.unwrap().0, 8);
    // Nach dem Shutdown wird nichts mehr angenommen.
    log.record(rec());
    assert_eq!(metrics.snapshot().dropped, 93);
}

#[tokio::test]
async fn shutdown_flushes_buffer() {
    let store = sqlite().await;
    let h = ObserveBuilder::new(cfg(1000, 600_000))
        .store(store.clone())
        .start();
    for _ in 0..1500 {
        h.logger().record(rec());
    }
    tokio::task::yield_now().await;
    let metrics = Arc::clone(h.metrics());
    assert!(h.shutdown().await);
    assert_eq!(metrics.snapshot().written, 1500);
    assert_eq!(metrics.snapshot().dropped, 0);
    assert_eq!(store.count_request_logs().await.unwrap().0, 1500);
}

#[tokio::test]
async fn jsonl_export_without_store() {
    let dir = std::env::temp_dir().join(format!("aigw-observe-{}", new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("requests.jsonl");
    let mut c = cfg(1000, 600_000);
    c.payload = PayloadMode::Masked;
    c.jsonl.enabled = true;
    c.jsonl.path = path.clone();
    c.jsonl.include_payload = true;
    let h = ObserveBuilder::new(c).start();
    let mut r = rec();
    r.payload = Some(Box::new(RawPayload {
        request_headers: vec![("authorization".into(), "Bearer sk-geheim".into())],
        // Log-Maskierung über die PII-Engine (AIGW-37): alle Kategorien, nicht nur E-Mail.
        request_body:
            "an carol@example.net, Tel 0171 1234567, Key sk-proj-AbCdEf0123456789GhIjKl0123456789"
                .into(),
        response_body: "ok".into(),
        mode: PayloadMode::Masked,
    }));
    let id = r.request_id.clone();
    h.logger().record(r);
    h.logger().record(rec());
    assert!(h.shutdown().await);
    let text = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["id"], serde_json::json!(id));
    assert_eq!(lines[0]["endpoint"], "openai-chat");
    assert_eq!(lines[0]["inputTokens"], 10);
    assert_eq!(
        lines[0]["payload"]["request"]["body"],
        "an [EMAIL], Tel [PHONE], Key [API_KEY]"
    );
    assert!(lines[1].get("payload").is_none());
    assert!(!text.contains("geheim") && !text.contains("carol") && !text.contains("1234567"));
    std::fs::remove_dir_all(&dir).ok();
}

fn rich(at_ms: i64, latency_ms: u64, key: Id, team: Id, org: Id, deployment: Id) -> RequestRecord {
    let mut r = rec();
    r.started_at = at(at_ms);
    r.duration = Duration::from_millis(latency_ms);
    r.api_key_id = Some(key);
    r.team_id = Some(team);
    r.organization_id = Some(org);
    r.deployment_id = Some(deployment);
    r.model_alias = Some("gpt-4o-latest".into());
    r.model_group = Some("gpt-4o".into());
    r.provider = Some(ProviderKind::OpenAi);
    r.provider_model = Some("gpt-4o-2024-08-06".into());
    r.usage = Usage {
        input_tokens: 100,
        output_tokens: 20,
        cache_read_tokens: Some(40),
        cache_write_tokens: Some(7),
        reasoning_tokens: Some(3),
        ..Usage::default()
    };
    r.cost = Some(aigw_core::resource::Amount::from_nanos(1_500));
    r.stream = true;
    r
}

/// Rollups (inkl. UPSERT über Batches hinweg), Payload-Ablage und Aufbewahrung.
async fn rollup_scenario(store: Store) {
    store.migrate().await.unwrap();
    let (key, team, org, dep) = (new_id(), new_id(), new_id(), new_id());
    let base = now_ms() / 60_000 * 60_000 - 10 * 60_000;
    let mut c = cfg(1000, 20);
    c.payload = PayloadMode::Masked;
    let h = ObserveBuilder::new(c).store(store.clone()).start();
    let log = h.logger();

    let mut r1 = rich(base + 1_000, 20, key, team, org, dep);
    r1.ttft = Some(Duration::from_millis(8));
    r1.payload = Some(Box::new(RawPayload {
        request_headers: vec![
            ("Authorization".into(), "Bearer sk-aigw-geheim".into()),
            ("x-api-key".into(), "geheim2".into()),
            ("content-type".into(), "application/json".into()),
        ],
        request_body: r#"{"messages":[{"role":"user","content":"IBAN DE89370400440532013000"}]}"#
            .into(),
        response_body: "Danke, dave@example.com".into(),
        mode: PayloadMode::Masked,
    }));
    let mut r2 = rich(base + 2_000, 600, key, team, org, dep);
    r2.status = 500;
    r2.error_kind = Some(ErrorKind::Internal);
    r2.cache = Some(CacheOutcome::HitExact);
    r2.usage_estimated = true;
    r2.client_aborted = true;
    let r3 = rich(base + 61_000, 30, key, team, org, dep);
    let (id1, id2) = (r1.request_id.clone(), r2.request_id.clone());
    log.record(r1);
    log.record(r2);
    log.record(r3);
    wait_written(h.metrics(), 3).await;
    // Zweiter Batch, gleiche Minute/gleicher Schlüssel → additiver UPSERT.
    log.record(rich(base + 3_000, 20, key, team, org, dep));
    wait_written(h.metrics(), 4).await;
    assert!(h.metrics().snapshot().batches >= 2);

    let rollups = store.list_rollups(base, base + 120_000).await.unwrap();
    assert_eq!(rollups.len(), 2, "{rollups:?}");
    let (k0, m0) = &rollups[0];
    assert_eq!(k0.bucket_start, base);
    assert_eq!(k0.api_key_id, key.to_string());
    assert_eq!(k0.team_id, team.to_string());
    assert_eq!(k0.organization_id, org.to_string());
    assert_eq!(k0.model_group, "gpt-4o");
    assert_eq!(k0.deployment_id, dep.to_string());
    assert_eq!(m0.requests, 3);
    assert_eq!(m0.errors, 1);
    assert_eq!(m0.cache_hits, 1);
    assert_eq!(m0.input_tokens, 300);
    assert_eq!(m0.output_tokens, 60);
    assert_eq!(m0.cached_tokens, 120);
    assert_eq!(m0.cache_write_tokens, 21);
    assert_eq!(m0.reasoning_tokens, 9);
    assert_eq!(m0.cost, 4_500);
    assert_eq!(m0.latency_ms_sum, 640);
    assert_eq!(m0.estimated, 1);
    assert_eq!(m0.aborted, 1);
    assert_eq!((m0.ttft_ms_sum, m0.ttft_count), (8, 1));
    let mut hist = [0; 12];
    hist[0] = 2; // 20 ms, 20 ms
    hist[5] = 1; // 600 ms
    assert_eq!(m0.latency_hist, hist);
    let (k1, m1) = &rollups[1];
    assert_eq!(k1.bucket_start, base + 60_000);
    assert_eq!((m1.requests, m1.errors, m1.latency_hist[1]), (1, 0, 1));

    // Rohlog-Zeile
    let row = store.get_request_log(&id2).await.unwrap().unwrap();
    assert_eq!(row.status, 500);
    assert_eq!(row.error_code.as_deref(), Some("internal"));
    assert_eq!(row.cache.as_deref(), Some("hit-exact"));
    assert_eq!(row.provider.as_deref(), Some("openai"));
    assert_eq!(row.model_requested.as_deref(), Some("gpt-4o-latest"));
    assert!(row.usage_estimated && row.client_aborted && row.stream);
    assert_eq!(row.ttft_ms, None);
    assert_eq!(
        store.get_request_log(&id1).await.unwrap().unwrap().ttft_ms,
        Some(8)
    );

    // Payload: maskiert, sensible Header fehlen.
    let p = store.get_request_payload(&id1).await.unwrap().unwrap();
    assert_eq!(p.mode, "masked");
    let text = p.payload.to_string();
    assert!(
        text.contains("[IBAN]") && text.contains("[EMAIL]"),
        "{text}"
    );
    assert!(!text.contains("geheim") && !text.to_lowercase().contains("authorization"));
    assert!(store.get_request_payload(&id2).await.unwrap().is_none());
    assert_eq!(store.count_request_logs().await.unwrap(), (4, 1));

    assert!(h.shutdown().await);

    // Aufbewahrung: Payload nach 1 Tag, Rohlogs nach 30 Tagen, Rollups nach 400 Tagen.
    let ret = RetentionConfig {
        request_log_days: 30,
        payload_days: 1,
        rollup_days: 400,
        sweep_interval_secs: 3600,
    };
    let day = 86_400_000;
    let r = sweep(&store, &ret, base + 2 * day).await.unwrap();
    assert_eq!((r.payloads, r.request_logs, r.rollups), (1, 0, 0));
    assert_eq!(store.count_request_logs().await.unwrap(), (4, 0));
    let r = sweep(&store, &ret, base + 31 * day).await.unwrap();
    assert_eq!((r.request_logs, r.rollups), (4, 0));
    let r = sweep(&store, &ret, base + 401 * day).await.unwrap();
    assert_eq!(r.rollups, 2);
    // 0 = unbegrenzt
    let keep = RetentionConfig {
        request_log_days: 0,
        payload_days: 0,
        rollup_days: 0,
        sweep_interval_secs: 1,
    };
    assert_eq!(
        sweep(&store, &keep, base + 10_000 * day).await.unwrap(),
        Default::default()
    );
    store.close().await;
}

#[tokio::test]
async fn rollups_sqlite_memory() {
    rollup_scenario(Store::connect("sqlite::memory:").await.unwrap()).await;
}

#[tokio::test]
async fn rollups_sqlite_file() {
    let dir = std::env::temp_dir().join(format!("aigw-observe-test-{}", new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let url = format!("sqlite://{}/aigw.db?mode=rwc", dir.display());
    rollup_scenario(Store::connect(&url).await.unwrap()).await;
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn rollups_postgres() {
    let Ok(base) = std::env::var("AIGW_TEST_POSTGRES_URL") else {
        eprintln!("AIGW_TEST_POSTGRES_URL nicht gesetzt – Postgres-Test übersprungen");
        return;
    };
    let db = format!("aigw_observe_{}", new_id().simple());
    let admin = sqlx::PgPool::connect(&base).await.unwrap();
    sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {db}")))
        .execute(&admin)
        .await
        .unwrap();
    let (prefix, _) = base.rsplit_once('/').expect("URL mit Datenbankpfad");
    let store = Store::connect(&format!("{prefix}/{db}")).await.unwrap();
    assert_eq!(store.backend(), aigw_store::Backend::Postgres);
    rollup_scenario(store).await;
    sqlx::query(AssertSqlSafe(format!("DROP DATABASE {db} WITH (FORCE)")))
        .execute(&admin)
        .await
        .unwrap();
}
