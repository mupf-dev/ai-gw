// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! `postgres-sync` mit zwei Instanzen gegen eine echte Postgres-Datenbank.
//!
//! Läuft nur, wenn `AIGW_TEST_POSTGRES_URL` gesetzt ist (`scripts/check.sh` mit
//! `AIGW_TEST_POSTGRES=docker`); legt je Lauf eine Wegwerf-Datenbank an.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stderr,
    unused_must_use
)]

use std::sync::Arc;
use std::time::Duration;

use aigw_counters::{
    CounterBackend, Health, LimitCheck, ManualClock, PgSyncCounters, PgSyncOptions, Window,
};
use sqlx::{AssertSqlSafe, PgPool};

const HOUR: u64 = 3_600_000;

async fn fresh_db() -> Option<(PgPool, PgPool, String)> {
    let Ok(base) = std::env::var("AIGW_TEST_POSTGRES_URL") else {
        eprintln!("AIGW_TEST_POSTGRES_URL nicht gesetzt – Postgres-Test übersprungen");
        return None;
    };
    let db = format!("aigw_cnt_{}", uuid::Uuid::now_v7().simple());
    let admin = PgPool::connect(&base).await.unwrap();
    sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {db}")))
        .execute(&admin)
        .await
        .unwrap();
    let (prefix, _) = base.rsplit_once('/').expect("URL mit Datenbankpfad");
    let pool = PgPool::connect(&format!("{prefix}/{db}")).await.unwrap();
    aigw_store::migrator(aigw_store::Backend::Postgres)
        .run(&pool)
        .await
        .unwrap();
    Some((admin, pool, db))
}

fn instance(pool: &PgPool, name: &str, clock: &Arc<ManualClock>) -> PgSyncCounters {
    PgSyncCounters::new_manual(
        pool.clone(),
        PgSyncOptions {
            instance_id: Some(name.to_owned()),
            clock: Some(clock.clone()),
            cleanup_interval: Duration::ZERO,
            ..PgSyncOptions::default()
        },
    )
}

#[tokio::test]
async fn two_instances_share_counts() {
    let Some((admin, pool, db)) = fresh_db().await else {
        return;
    };
    // Mitten in einer Stunde, damit kein Bucket-Wechsel stört.
    let clock = Arc::new(ManualClock::new(500_000 * HOUR + HOUR / 2));
    let a = instance(&pool, "a", &clock);
    let b = instance(&pool, "b", &clock);
    let lim = |amount| [LimitCheck::new("team:x", Window::Hour, 100, amount)];

    let ra = a.reserve(&lim(60)).await.unwrap();
    // Vor dem Abgleich weiß b nichts davon (unscharf im Cluster).
    assert_eq!(b.get("team:x", Window::Hour).await, 0);
    b.reserve(&lim(0)).await.unwrap(); // b kennt den Zähler, damit er gelesen wird
    let rep = a.sync_now().await.unwrap();
    assert_eq!(rep.written, 1);
    b.sync_now().await.unwrap();
    assert_eq!(b.get("team:x", Window::Hour).await, 60);
    assert!(b.reserve(&lim(41)).await.is_err());
    b.reserve(&lim(40)).await.unwrap();
    b.sync_now().await.unwrap();
    a.sync_now().await.unwrap();
    assert_eq!(a.get("team:x", Window::Hour).await, 100);
    assert!(a.reserve(&lim(1)).await.is_err());

    // Abgleich nach unten: a hat nur 20 statt 60 verbraucht.
    a.settle(ra, 20).await;
    a.sync_now().await.unwrap();
    b.sync_now().await.unwrap();
    assert_eq!(b.get("team:x", Window::Hour).await, 60);

    // Datenbankausfall (Tabelle weg): a zählt lokal weiter und holt später nach.
    sqlx::query("ALTER TABLE counter_state RENAME TO counter_state_off")
        .execute(&pool)
        .await
        .unwrap();
    a.reserve(&lim(10)).await.unwrap();
    assert!(a.sync_now().await.is_err());
    assert!(matches!(a.health(), Health::Degraded(_)));
    assert_eq!(a.backlog_len(), 1);
    a.reserve(&lim(5)).await.unwrap();
    assert!(a.sync_now().await.is_err());
    assert_eq!(
        a.backlog_len(),
        1,
        "Deltas desselben Buckets zusammengefasst"
    );
    assert_eq!(a.get("team:x", Window::Hour).await, 75);
    sqlx::query("ALTER TABLE counter_state_off RENAME TO counter_state")
        .execute(&pool)
        .await
        .unwrap();
    let rep = a.sync_now().await.unwrap();
    assert_eq!(rep.written, 1);
    assert_eq!(a.health(), Health::Ok);
    assert_eq!(a.backlog_len(), 0);
    b.sync_now().await.unwrap();
    assert_eq!(b.get("team:x", Window::Hour).await, 75);

    // Bereinigung: nach drei Stunden zählt der Bucket nicht mehr, die Zeilen verschwinden.
    clock.advance(3 * HOUR);
    a.sync_now().await.unwrap();
    let (rows,): (i64,) = sqlx::query_as("SELECT count(*) FROM counter_state")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
    assert_eq!(b.get("team:x", Window::Hour).await, 0);

    pool.close().await;
    sqlx::query(AssertSqlSafe(format!("DROP DATABASE {db} WITH (FORCE)")))
        .execute(&admin)
        .await
        .unwrap();
}

#[tokio::test]
async fn sliding_window_across_instances_and_multi_key() {
    let Some((admin, pool, db)) = fresh_db().await else {
        return;
    };
    let clock = Arc::new(ManualClock::new(500_000 * HOUR));
    let a = instance(&pool, "a", &clock);
    let b = instance(&pool, "b", &clock);

    let checks = |amount| {
        [
            LimitCheck::new("key:1", Window::Minute, 1_000, amount),
            LimitCheck::new("org:1", Window::Minute, 100, amount),
        ]
    };
    a.reserve(&checks(80)).await.unwrap();
    b.reserve(&checks(0)).await.unwrap();
    a.sync_now().await.unwrap();
    b.sync_now().await.unwrap();
    // org voll bis auf 20 → b darf 30 nicht, auch key bleibt unberührt.
    assert!(b.reserve(&checks(30)).await.is_err());
    assert_eq!(b.get("key:1", Window::Minute).await, 80);

    // Eine halbe Minute später im nächsten Bucket zählt a's Buchung zur Hälfte.
    clock.advance(60_000 + 30_000);
    b.sync_now().await.unwrap();
    assert_eq!(b.get("org:1", Window::Minute).await, 40);
    b.reserve(&checks(60)).await.unwrap();

    pool.close().await;
    sqlx::query(AssertSqlSafe(format!("DROP DATABASE {db} WITH (FORCE)")))
        .execute(&admin)
        .await
        .unwrap();
}

#[tokio::test]
async fn period_windows_shared_and_cleaned_up() {
    let Some((admin, pool, db)) = fresh_db().await else {
        return;
    };
    let day = 86_400_000;
    let start = 20_000 * day;
    let clock = Arc::new(ManualClock::new(start + HOUR));
    let a = instance(&pool, "a", &clock);
    let b = instance(&pool, "b", &clock);
    // F25: Kalendertag als fester Zeitraum, Schlüssel mit Periodenbeginn (wie der Proxy).
    let p = Window::period(start, start + day);
    let key = format!("key:1:requests@calendar-day:{start}");
    let lim = |amount| [LimitCheck::new(&key, p, 10, amount)];
    a.reserve(&lim(7)).await.unwrap();
    b.reserve(&lim(0)).await.unwrap();
    a.sync_now().await.unwrap();
    b.sync_now().await.unwrap();
    assert_eq!(b.get(&key, p).await, 7);
    let rej = b.reserve(&lim(4)).await.unwrap_err();
    // Wartezeit bis zum Ende des Tages, kein Gleiten
    assert_eq!(rej.retry_after, Some(Duration::from_millis(day - HOUR)));
    let (k,): (String,) = sqlx::query_as("SELECT counter_key FROM counter_state LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(k, format!("p{}|{key}", start + day));

    // Nach dem Ende zählt der Zeitraum nicht mehr; nach 32 Tagen werden die Zeilen gelöscht.
    clock.set(start + day + 1);
    assert_eq!(b.get(&key, p).await, 0);
    clock.set(start + 33 * day);
    a.sync_now().await.unwrap();
    let (rows,): (i64,) = sqlx::query_as("SELECT count(*) FROM counter_state")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);

    pool.close().await;
    sqlx::query(AssertSqlSafe(format!("DROP DATABASE {db} WITH (FORCE)")))
        .execute(&admin)
        .await
        .unwrap();
}

#[tokio::test]
async fn unreachable_database_counts_locally() {
    // Braucht keine Datenbank: Port 1 ist nicht erreichbar.
    let c = PgSyncCounters::connect(
        "postgres://u:p@127.0.0.1:1/x",
        PgSyncOptions {
            interval: Duration::from_secs(3_600),
            timeout: Duration::from_millis(300),
            ..PgSyncOptions::default()
        },
    )
    .await
    .unwrap();
    c.reserve(&[LimitCheck::new("k", Window::Minute, 10, 7)])
        .await
        .unwrap();
    assert!(c.sync_now().await.is_err());
    assert!(matches!(c.health(), Health::Degraded(_)));
    assert!(
        c.reserve(&[LimitCheck::new("k", Window::Minute, 10, 4)])
            .await
            .is_err()
    );
    assert_eq!(c.backlog_len(), 1);
}
