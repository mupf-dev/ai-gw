// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Budgetstände (`budget_usage`) als `BudgetLedger` gegen SQLite und – falls
//! `AIGW_TEST_POSTGRES_URL` gesetzt ist – gegen Postgres; zwei `Budgets`-Instanzen teilen
//! sich die Datenbank wie zwei Gateway-Instanzen.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::sync::Arc;

use aigw_billing::{BudgetKey, BudgetLedger, BudgetLevel, BudgetTarget, Budgets, Calendar};
use aigw_core::resource::{Amount, Budget, BudgetPeriod, new_id};
use aigw_store::Store;
use sqlx::AssertSqlSafe;

fn a(s: &str) -> Amount {
    s.parse().unwrap()
}

async fn run(store: Store) {
    store.migrate().await.unwrap();
    let k1 = BudgetKey {
        level: BudgetLevel::Team,
        target: new_id(),
        period: BudgetPeriod::Month,
        period_start_ms: 1_000,
    };
    let k2 = BudgetKey {
        level: BudgetLevel::ApiKey,
        ..k1
    };
    let k3 = BudgetKey {
        period_start_ms: 2_000,
        ..k1
    };
    // Additiv, Summe zurück; unbekannte Schlüssel = 0.
    let v = store
        .sync_budget_usage(&[(k1, 5), (k2, 7)], &[k1, k2, k3], 10)
        .await
        .unwrap();
    assert_eq!(v, [5, 7, 0]);
    let v = store
        .sync_budget_usage(&[(k1, -2), (k3, 1)], &[k3, k1], 11)
        .await
        .unwrap();
    assert_eq!(v, [1, 3]);
    assert_eq!(store.budget_usage(&[k2]).await.unwrap(), [7]);
    // Viele Schlüssel (mehrere Statements).
    let many: Vec<(BudgetKey, i64)> = (0..250)
        .map(|i| {
            (
                BudgetKey {
                    period_start_ms: 10_000 + i,
                    ..k1
                },
                i,
            )
        })
        .collect();
    let keys: Vec<BudgetKey> = many.iter().map(|(k, _)| *k).collect();
    let v = BudgetLedger::sync(&store, &many, &keys, 12).await.unwrap();
    assert_eq!(v, (0..250).collect::<Vec<i64>>());

    // Zwei Instanzen mit gemeinsamer Datenbank.
    let ledger: Arc<dyn BudgetLedger> = Arc::new(store.clone());
    let i1 = Budgets::new(Calendar::utc(), Some(Arc::clone(&ledger)));
    let i2 = Budgets::new(
        Calendar::new("Europe/Berlin").unwrap(),
        Some(Arc::clone(&ledger)),
    );
    let spec = Budget {
        amount: a("1"),
        period: BudgetPeriod::Total,
        warn_at_percent: vec![],
        hard_limit: true,
        enforcement: Default::default(),
    };
    let t = [BudgetTarget {
        level: BudgetLevel::Organization,
        target: new_id(),
        budget: &spec,
    }];
    let now = 1_800_000_000_000;
    i1.check(&t, now).await.unwrap();
    i2.check(&t, now).await.unwrap();
    i1.charge(&t, a("0.75"), now);
    i2.charge(&t, a("0.25"), now);
    i1.flush().await.unwrap();
    i2.flush().await.unwrap();
    i1.flush().await.unwrap();
    let (key, _) = i1.key(&t[0], now);
    assert_eq!(store.budget_usage(&[key]).await.unwrap(), [a("1").nanos()]);
    assert!(i1.check(&t, now).await.is_err());
    assert!(i2.check(&t, now).await.is_err());

    // Bereinigung: alte Zeiträume weg, `total` bleibt.
    let removed = store.prune_budget_usage(5_000).await.unwrap();
    assert_eq!(removed, 3); // k1, k2, k3 (Beginn < 5000)
    assert_eq!(store.budget_usage(&[key]).await.unwrap(), [a("1").nanos()]);
}

#[tokio::test]
async fn sqlite() {
    let dir = std::env::temp_dir().join(format!("aigw-budget-test-{}", new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let url = format!("sqlite://{}/aigw.db?mode=rwc", dir.display());
    run(Store::connect(&url).await.unwrap()).await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn postgres() {
    let Ok(base) = std::env::var("AIGW_TEST_POSTGRES_URL") else {
        eprintln!("AIGW_TEST_POSTGRES_URL nicht gesetzt – Postgres-Test übersprungen");
        return;
    };
    let db = format!("aigw_budget_{}", new_id().simple());
    let admin = sqlx::PgPool::connect(&base).await.unwrap();
    sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {db}")))
        .execute(&admin)
        .await
        .unwrap();
    let (prefix, _) = base.rsplit_once('/').expect("URL mit Datenbankpfad");
    let store = Store::connect(&format!("{prefix}/{db}")).await.unwrap();
    run(store).await;
    sqlx::query(AssertSqlSafe(format!("DROP DATABASE {db} WITH (FORCE)")))
        .execute(&admin)
        .await
        .unwrap();
}
