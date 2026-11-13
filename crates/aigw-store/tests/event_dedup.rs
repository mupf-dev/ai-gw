// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Ereignis-Webhooks im Cluster (AIGW-61): zwei Gateway-Instanzen (zwei `Store`-Verbindungen
//! auf dieselbe SQLite-Datei bzw. – mit `AIGW_TEST_POSTGRES_URL` – dieselbe Postgres-DB)
//! melden dieselbe Budget-Warnung; der Fake-Webhook erhält sie genau einmal, auch wenn der
//! erste Zustellversuch scheitert (Wiederholung).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aigw_billing::{
    BudgetEvent, BudgetEventKind, BudgetEventSink, BudgetKey, BudgetLevel, Calendar, WebhookEvents,
};
use aigw_core::config::AdminConfig;
use aigw_core::resource::{Amount, BudgetPeriod, new_id};
use aigw_core::upstream::UpstreamPolicy;
use aigw_store::Store;
use aigw_webhook::{
    ClientOptions, DispatcherOptions, EventDedup, EventDispatcher, EventMetrics, EventTarget,
    EventTask, SecretResolver, WebhookClient,
};
use axum::extract::State;
use axum::http::StatusCode;
use bytes::Bytes;
use serde_json::Value;
use sqlx::AssertSqlSafe;

#[derive(Default)]
struct Hook {
    fail_first: Mutex<usize>,
    hits: AtomicUsize,
    bodies: Mutex<Vec<Value>>,
}

async fn handler(State(h): State<Arc<Hook>>, body: Bytes) -> StatusCode {
    h.hits.fetch_add(1, Ordering::SeqCst);
    {
        let mut left = h.fail_first.lock().unwrap();
        if *left > 0 {
            *left -= 1;
            return StatusCode::SERVICE_UNAVAILABLE;
        }
    }
    h.bodies
        .lock()
        .unwrap()
        .push(serde_json::from_slice(&body).unwrap());
    StatusCode::NO_CONTENT
}

async fn hook(fail_first: usize) -> (SocketAddr, Arc<Hook>) {
    let h = Arc::new(Hook {
        fail_first: Mutex::new(fail_first),
        ..Hook::default()
    });
    let app = axum::Router::new()
        .route("/budget", axum::routing::post(handler))
        .with_state(Arc::clone(&h));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, h)
}

fn instance(addr: SocketAddr, store: Store) -> (WebhookEvents, EventDispatcher, EventTask) {
    let policy = UpstreamPolicy::from_config(&AdminConfig {
        allow_insecure_upstreams: vec!["127.0.0.1".into()],
        allow_private_upstreams: vec!["127.0.0.1".into()],
        ..AdminConfig::default()
    })
    .unwrap();
    let client = WebhookClient::new(policy, ClientOptions::default()).unwrap();
    let target = EventTarget {
        name: "finance".into(),
        uri: client.check_url(&format!("http://{addr}/budget")).unwrap(),
        events: vec!["budget.warning".into(), "budget.exhausted".into()],
        secret: SecretResolver::None,
        timeout: Duration::from_secs(2),
        max_attempts: 4,
    };
    let dedup: Arc<dyn EventDedup> = Arc::new(store);
    let (d, task) = EventDispatcher::start(
        client,
        vec![target],
        Some(dedup),
        DispatcherOptions {
            retry_initial: Duration::from_millis(20),
            retry_max: Duration::from_millis(100),
            ..DispatcherOptions::default()
        },
    );
    (WebhookEvents::new(d.clone(), Calendar::utc()), d, task)
}

async fn settled(ds: &[&EventDispatcher], delivered: u64, deduplicated: u64) {
    for _ in 0..250 {
        let d: u64 = ds
            .iter()
            .map(|d| EventMetrics::get(&d.metrics().delivered))
            .sum();
        let x: u64 = ds
            .iter()
            .map(|d| EventMetrics::get(&d.metrics().deduplicated))
            .sum();
        if d == delivered && x == deduplicated {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "nicht erreicht: {:?}",
        ds.iter().map(|d| d.metrics()).collect::<Vec<_>>()
    );
}

async fn run(a: Store, b: Store) {
    a.migrate().await.unwrap();
    b.migrate().await.unwrap();
    let (addr, h) = hook(1).await;
    let (sink_a, da, ta) = instance(addr, a.clone());
    let (sink_b, db, tb) = instance(addr, b.clone());

    // Monatsbudget eines Teams, Zeitraum ab 2026-11-01 (UTC).
    let start = 1_793_491_200_000; // 2026-11-01T00:00:00Z
    let key = BudgetKey {
        level: BudgetLevel::Team,
        target: new_id(),
        period: BudgetPeriod::Month,
        period_start_ms: start,
    };
    let warn = BudgetEvent {
        kind: BudgetEventKind::Warning { percent: 80 },
        key,
        amount: "100".parse::<Amount>().unwrap(),
        used: "80.5".parse::<Amount>().unwrap(),
        hard_limit: true,
    };
    // Beide Instanzen erreichen die Schwelle (und eine meldet sie nach einem Neustart erneut).
    sink_a.on_event(&warn);
    sink_b.on_event(&warn);
    sink_b.on_event(&warn);
    settled(&[&da, &db], 1, 2).await;
    // Erster Versuch 503, danach zugestellt: 2 Aufrufe, 1 Ereignis.
    assert_eq!(h.hits.load(Ordering::SeqCst), 2);
    {
        let bodies = h.bodies.lock().unwrap();
        assert_eq!(bodies.len(), 1);
        let v = &bodies[0];
        assert_eq!(v["type"], "budget.warning");
        assert_eq!(v["data"]["level"], "team");
        assert_eq!(v["data"]["period"], "month");
        assert_eq!(v["data"]["percent"], 80);
        assert_eq!(v["data"]["periodEndMs"], 1_796_083_200_000_i64); // 2026-12-01
        assert_eq!(v["data"]["hardLimit"], true);
    }

    // Andere Schwelle bzw. Erschöpfung sind eigene Ereignisse – je einmal im Cluster.
    let exhausted = BudgetEvent {
        kind: BudgetEventKind::Exhausted,
        ..warn.clone()
    };
    sink_a.on_event(&exhausted);
    sink_b.on_event(&exhausted);
    settled(&[&da, &db], 2, 3).await;
    // Nächster Zeitraum: wieder einmal.
    let next = BudgetEvent {
        key: BudgetKey {
            period_start_ms: 1_796_083_200_000,
            ..key
        },
        ..warn.clone()
    };
    sink_b.on_event(&next);
    sink_a.on_event(&next);
    settled(&[&da, &db], 3, 4).await;
    assert_eq!(h.bodies.lock().unwrap().len(), 3);

    // Ablauf: abgelaufene Schlüssel dürfen neu beansprucht werden.
    assert!(a.claim_event("x", "t", 1_000, 2_000).await.unwrap());
    assert!(!b.claim_event("x", "t", 1_500, 3_000).await.unwrap());
    assert!(b.claim_event("x", "t", 2_000, 3_000).await.unwrap());

    ta.shutdown(Duration::from_secs(1)).await;
    tb.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn sqlite_two_instances() {
    let dir = std::env::temp_dir().join(format!("aigw-event-dedup-{}", new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let url = format!("sqlite://{}/aigw.db?mode=rwc", dir.display());
    let a = Store::connect(&url).await.unwrap();
    a.migrate().await.unwrap();
    let b = Store::connect(&url).await.unwrap();
    run(a, b).await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn postgres_two_instances() {
    let Ok(base) = std::env::var("AIGW_TEST_POSTGRES_URL") else {
        eprintln!("AIGW_TEST_POSTGRES_URL nicht gesetzt – Postgres-Test übersprungen");
        return;
    };
    let db = format!("aigw_events_{}", new_id().simple());
    let admin = sqlx::PgPool::connect(&base).await.unwrap();
    sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {db}")))
        .execute(&admin)
        .await
        .unwrap();
    let (prefix, _) = base.rsplit_once('/').expect("URL mit Datenbankpfad");
    let url = format!("{prefix}/{db}");
    let a = Store::connect(&url).await.unwrap();
    a.migrate().await.unwrap();
    let b = Store::connect(&url).await.unwrap();
    run(a, b).await;
    sqlx::query(AssertSqlSafe(format!("DROP DATABASE {db} WITH (FORCE)")))
        .execute(&admin)
        .await
        .unwrap();
}
