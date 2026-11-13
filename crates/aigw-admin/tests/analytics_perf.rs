// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Lastmessung Dashboards/Log-Explorer (AIGW-46): 1 Mio. Minuten-Rollups über 7 Tage und
//! 500 000 Rohlogs (synthetisch), danach Antwortzeiten der Admin-Routen über HTTP.
//!
//! Läuft nur mit `AIGW_ANALYTICS_PERF=1` (sonst übersprungen), gegen SQLite (Datei) und – mit
//! `AIGW_TEST_POSTGRES_URL` – Postgres. Größen über `AIGW_PERF_ROLLUPS` / `AIGW_PERF_LOGS`.
//!
//! ```sh
//! AIGW_ANALYTICS_PERF=1 cargo test --release -p aigw-admin --test analytics_perf -- --nocapture --test-threads 1
//! ```

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stderr,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aigw_admin::AdminState;
use aigw_admin::analytics::{AnalyticsState, router as analytics_router};
use aigw_core::config::AdminAuthConfig;
use aigw_core::resource::new_id;
use aigw_store::Store;
use aigw_store::observe::{RequestLogRow, RollupCounts, RollupKey};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};
use sqlx::AssertSqlSafe;

const MIN: i64 = 60_000;
const DAY: i64 = 86_400_000;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn ts(ms: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .unwrap()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}

async fn call(
    addr: SocketAddr,
    token: &str,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Bytes) {
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let mut req = Request::builder()
        .method(method)
        .uri(format!("http://{addr}/admin/api/v1{path}"))
        .header("authorization", format!("Bearer {token}"));
    let body = match body {
        Some(b) => {
            req = req.header("content-type", "application/json");
            Bytes::from(serde_json::to_vec(&b).unwrap())
        }
        None => Bytes::new(),
    };
    let res = client
        .request(req.body(Full::new(body)).unwrap())
        .await
        .unwrap();
    let status = res.status();
    (status, res.into_body().collect().await.unwrap().to_bytes())
}

/// Einfacher deterministischer Zufall (xorshift).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

async fn run(label: &str, store: Store) {
    let n_rollups = env_usize("AIGW_PERF_ROLLUPS", 1_000_000);
    let n_logs = env_usize("AIGW_PERF_LOGS", 500_000);
    store.migrate().await.unwrap();

    let (token, secret) = aigw_admin::BootstrapToken::generate().unwrap();
    let state = AdminState::from_config(
        store.clone(),
        &AdminAuthConfig::default(),
        Some(token),
        None,
    )
    .unwrap();
    let app = aigw_admin::router_with(state.clone()).merge(analytics_router(AnalyticsState {
        store: store.clone(),
        auth: Arc::clone(&state.auth),
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    // Mandanten: 3 Organisationen, 12 Teams; Keys nur als IDs (100), 6 Modellgruppen, 12 Deployments.
    let mut teams: Vec<(String, String)> = Vec::new(); // (id, org-id)
    for o in 0..3 {
        let (s, b) = call(
            addr,
            &secret,
            Method::POST,
            "/organizations",
            Some(json!({"metadata": {"name": format!("org{o}")}, "spec": {}})),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED);
        let org: Value = serde_json::from_slice(&b).unwrap();
        for t in 0..4 {
            let (s, b) = call(addr, &secret, Method::POST, "/teams",
                Some(json!({"metadata": {"name": format!("team{o}-{t}")}, "spec": {"organization": format!("org{o}")}}))).await;
            assert_eq!(s, StatusCode::CREATED);
            let team: Value = serde_json::from_slice(&b).unwrap();
            teams.push((
                team["id"].as_str().unwrap().to_owned(),
                org["id"].as_str().unwrap().to_owned(),
            ));
        }
    }
    let (s, b) = call(addr, &secret, Method::POST, "/admin-api-keys",
        Some(json!({"name": "perf-viewer", "roles": [{"role": "viewer", "organization": "org0", "team": "team0-0"}]}))).await;
    assert_eq!(s, StatusCode::CREATED);
    let scoped = serde_json::from_slice::<Value>(&b).unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_owned();

    let keys: Vec<String> = (0..100).map(|_| new_id().to_string()).collect();
    let deployments: Vec<String> = (0..12).map(|_| new_id().to_string()).collect();
    let groups = [
        "gpt-4o",
        "gpt-4o-mini",
        "claude",
        "llama",
        "mistral",
        "gemini",
    ];
    let now = (time::OffsetDateTime::now_utc().unix_timestamp() * 1000).div_euclid(MIN) * MIN;
    let start = now - 7 * DAY;
    let minutes = 7 * 24 * 60;
    let per_minute = n_rollups.div_ceil(minutes).max(1);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);

    // --- Rollups ---------------------------------------------------------------------------
    let t = Instant::now();
    let mut written = 0usize;
    let mut batch: Vec<(RollupKey, RollupCounts)> = Vec::with_capacity(20_000);
    for m in 0..minutes as i64 {
        for i in 0..per_minute {
            if written >= n_rollups {
                break;
            }
            let k = (i * 7 + m as usize) % keys.len();
            let (team, org) = &teams[k % teams.len()];
            let d = k % deployments.len();
            let req = 1 + rng.below(20) as i64;
            let mut c = RollupCounts {
                requests: req,
                errors: i64::from(rng.below(50) == 0),
                cache_hits: rng.below(3) as i64,
                input_tokens: req * 500,
                output_tokens: req * 120,
                cost: req * 1_234_567,
                latency_ms_sum: req * 800,
                ..RollupCounts::default()
            };
            c.latency_hist[2 + rng.below(6) as usize] = req;
            c.ttft_hist[1 + rng.below(4) as usize] = req / 2;
            batch.push((
                RollupKey {
                    bucket_start: start + m * MIN,
                    api_key_id: keys[k].clone(),
                    team_id: team.clone(),
                    organization_id: org.clone(),
                    model_group: groups[d % groups.len()].to_owned(),
                    deployment_id: deployments[d].clone(),
                },
                c,
            ));
            written += 1;
        }
        if batch.len() >= 10_000 {
            batch.sort_by(|a, b| a.0.cmp(&b.0));
            batch.dedup_by(|a, b| a.0 == b.0);
            store.write_request_batch(&[], &[], &batch).await.unwrap();
            batch.clear();
        }
    }
    if !batch.is_empty() {
        batch.sort_by(|a, b| a.0.cmp(&b.0));
        batch.dedup_by(|a, b| a.0 == b.0);
        store.write_request_batch(&[], &[], &batch).await.unwrap();
    }
    eprintln!(
        "[{label}] {written} Rollups geschrieben in {:.1?}",
        t.elapsed()
    );

    // --- Rohlogs ---------------------------------------------------------------------------
    let t = Instant::now();
    let mut rows: Vec<RequestLogRow> = Vec::with_capacity(5000);
    for i in 0..n_logs {
        let k = rng.below(keys.len() as u64) as usize;
        let (team, org) = &teams[k % teams.len()];
        let d = k % deployments.len();
        let at = start + (rng.next() % (7 * DAY as u64)) as i64;
        let err = rng.below(30) == 0;
        rows.push(RequestLogRow {
            // UUID (Postgres: Spaltentyp UUID), zeitlich sortiert wie bisher.
            id: aigw_core::resource::Id::from_u64_pair(at as u64, i as u64).to_string(),
            at,
            endpoint: "openai-chat".into(),
            api_key_id: Some(keys[k].clone()),
            team_id: Some(team.clone()),
            organization_id: Some(org.clone()),
            model_requested: Some(groups[d % groups.len()].into()),
            model_group: Some(groups[d % groups.len()].into()),
            deployment_id: Some(deployments[d].clone()),
            provider: Some("openai".into()),
            provider_model: Some("gpt-4o-2024-08-06".into()),
            status: if err { 429 } else { 200 },
            error_code: err.then(|| "rate_limited".into()),
            input_tokens: 500,
            output_tokens: 120,
            cost: 1_234_567,
            latency_ms: 300 + rng.below(2000) as i64,
            stream: rng.below(2) == 0,
            client_request_id: Some(format!("c-{i}")),
            ..RequestLogRow::default()
        });
        if rows.len() == 5000 {
            let refs: Vec<&RequestLogRow> = rows.iter().collect();
            store.write_request_batch(&refs, &[], &[]).await.unwrap();
            rows.clear();
        }
    }
    if !rows.is_empty() {
        let refs: Vec<&RequestLogRow> = rows.iter().collect();
        store.write_request_batch(&refs, &[], &[]).await.unwrap();
    }
    eprintln!(
        "[{label}] {n_logs} Rohlogs geschrieben in {:.1?}",
        t.elapsed()
    );
    store.analyze().await.unwrap();

    // --- Messung ---------------------------------------------------------------------------
    let r7 = format!("from={}&to={}", ts(start), ts(now));
    let r24 = format!("from={}&to={}", ts(now - DAY), ts(now));
    let (s, b) = call(addr, &secret, Method::GET, "/request-logs?limit=50", None).await;
    assert_eq!(s, StatusCode::OK);
    let first: Value = serde_json::from_slice(&b).unwrap();
    let cursor = first["nextCursor"].as_str().unwrap().to_owned();
    let cases: Vec<(&str, String, &str)> = vec![
        ("summary 7d", format!("/analytics/summary?{r7}"), &secret),
        (
            "timeseries 7d requests (1h)",
            format!("/analytics/timeseries?metric=requests&{r7}"),
            &secret,
        ),
        (
            "timeseries 7d cost groupBy model_group",
            format!("/analytics/timeseries?metric=cost&groupBy=model_group&{r7}"),
            &secret,
        ),
        (
            "timeseries 7d latency_p95 groupBy key",
            format!("/analytics/timeseries?metric=latency_p95&groupBy=key&{r7}"),
            &secret,
        ),
        (
            "timeseries 7d ttft_p95 groupBy provider",
            format!("/analytics/timeseries?metric=ttft_p95&groupBy=provider&{r7}"),
            &secret,
        ),
        (
            "top 7d key by cost",
            format!("/analytics/top?dimension=key&metric=cost&{r7}"),
            &secret,
        ),
        (
            "timeseries 24h requests (5m)",
            format!("/analytics/timeseries?metric=requests&{r24}"),
            &secret,
        ),
        (
            "summary 7d (team-scoped viewer)",
            format!("/analytics/summary?{r7}"),
            &scoped,
        ),
        (
            "timeseries 7d groupBy team (team-scoped)",
            format!("/analytics/timeseries?metric=requests&groupBy=team&{r7}"),
            &scoped,
        ),
        (
            "request-logs page 1",
            "/request-logs?limit=50".to_owned(),
            &secret,
        ),
        (
            "request-logs page 2 (cursor)",
            format!("/request-logs?limit=50&cursor={cursor}"),
            &secret,
        ),
        (
            "request-logs status=error",
            "/request-logs?status=error".to_owned(),
            &secret,
        ),
        (
            "request-logs modelGroup+24h",
            format!("/request-logs?modelGroup=claude&{r24}"),
            &secret,
        ),
        (
            "request-logs clientRequestId",
            "/request-logs?clientRequestId=c-4242".to_owned(),
            &secret,
        ),
        (
            "request-logs team-scoped",
            "/request-logs".to_owned(),
            &scoped,
        ),
    ];
    for (name, path, tok) in cases {
        let mut times = Vec::new();
        for _ in 0..5 {
            let t = Instant::now();
            let (s, b) = call(addr, tok, Method::GET, &path, None).await;
            times.push(t.elapsed());
            assert_eq!(s, StatusCode::OK, "{name}: {}", String::from_utf8_lossy(&b));
        }
        times.sort();
        let med = times[times.len() / 2];
        let max = times[times.len() - 1];
        eprintln!(
            "PERF [{label}] {name:<45} median {:>7.1} ms  max {:>7.1} ms",
            ms(med),
            ms(max)
        );
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn perf_sqlite() {
    if std::env::var("AIGW_ANALYTICS_PERF").is_err() {
        eprintln!("AIGW_ANALYTICS_PERF nicht gesetzt – Lastmessung übersprungen");
        return;
    }
    let dir = std::env::temp_dir().join(format!("aigw-perf-{}", new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let url = format!("sqlite://{}/aigw.db?mode=rwc", dir.display());
    run("sqlite", Store::connect(&url).await.unwrap()).await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn perf_postgres() {
    if std::env::var("AIGW_ANALYTICS_PERF").is_err() {
        return;
    }
    let Ok(base) = std::env::var("AIGW_TEST_POSTGRES_URL") else {
        eprintln!("AIGW_TEST_POSTGRES_URL nicht gesetzt – Postgres-Lastmessung übersprungen");
        return;
    };
    let db = format!("aigw_perf_{}", new_id().simple());
    let admin = sqlx::PgPool::connect(&base).await.unwrap();
    sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {db}")))
        .execute(&admin)
        .await
        .unwrap();
    let (prefix, _) = base.rsplit_once('/').unwrap();
    let store = Store::connect(&format!("{prefix}/{db}")).await.unwrap();
    run("postgres", store.clone()).await;
    store.close().await;
    sqlx::query(AssertSqlSafe(format!("DROP DATABASE {db} WITH (FORCE)")))
        .execute(&admin)
        .await
        .unwrap();
}
