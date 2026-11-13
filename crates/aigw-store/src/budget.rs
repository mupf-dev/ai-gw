// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Budgetstände in `budget_usage` (AIGW-31): [`BudgetLedger`] für SQLite und Postgres.
//!
//! Deltas werden **additiv** geschrieben (`amount = budget_usage.amount + excluded.amount`),
//! damit mehrere Instanzen ohne Read-Modify-Write gleichzeitig buchen können; danach werden
//! die Summen in derselben Transaktion zurückgelesen. Primärschlüssel (level, target_id,
//! period, period_start), `amount` in Nano-Einheiten (Postgres: `target_id UUID`,
//! `period_start TIMESTAMPTZ`; SQLite: Text bzw. Unix-ms).

use std::collections::HashMap;
use std::fmt::Write as _;

use aigw_billing::{BudgetKey, BudgetLedger, LedgerError, period_code};
use aigw_core::resource::Id;

use crate::db::{Q, Val};
use crate::{Store, StoreError};

/// Zeilen je Statement (4–6 Parameter je Zeile).
const ROWS_PER_STATEMENT: usize = 100;

type RowKey = (String, Id, String, i64);

fn row_key(k: &BudgetKey) -> RowKey {
    (
        k.level.as_str().to_owned(),
        k.target,
        period_code(k.period).to_owned(),
        k.period_start_ms,
    )
}

fn bind_key(q: Q, k: &BudgetKey) -> Q {
    q.bind(Val::text(k.level.as_str()))
        .bind(Val::uuid(k.target))
        .bind(Val::text(period_code(k.period)))
        .bind(Val::ms(k.period_start_ms))
}

impl Store {
    /// Addiert `deltas` (je Schlüssel höchstens einmal) und liest die Summen für `read` in
    /// einer Transaktion (Reihenfolge wie `read`, unbekannt = 0).
    pub async fn sync_budget_usage(
        &self,
        deltas: &[(BudgetKey, i64)],
        read: &[BudgetKey],
        now_ms: i64,
    ) -> Result<Vec<i64>, StoreError> {
        let mut tx = self.pool.begin().await?;
        for chunk in deltas.chunks(ROWS_PER_STATEMENT) {
            let mut sql = String::from(
                "INSERT INTO budget_usage (level, target_id, period, period_start, amount, \
                 updated_at) VALUES ",
            );
            for i in 0..chunk.len() {
                let n = i * 6;
                if i > 0 {
                    sql.push_str(", ");
                }
                let _ = write!(
                    sql,
                    "(${}, ${}, ${}, ${}, ${}, ${})",
                    n + 1,
                    n + 2,
                    n + 3,
                    n + 4,
                    n + 5,
                    n + 6
                );
            }
            sql.push_str(
                " ON CONFLICT (level, target_id, period, period_start) DO UPDATE SET \
                 amount = budget_usage.amount + excluded.amount, updated_at = excluded.updated_at",
            );
            let mut q = Q::new(sql);
            for (k, d) in chunk {
                q = bind_key(q, k).bind(Val::int(*d)).bind(Val::ms(now_ms));
            }
            q.execute(&mut tx).await?;
        }

        let mut found: HashMap<RowKey, i64> = HashMap::with_capacity(read.len());
        for chunk in read.chunks(ROWS_PER_STATEMENT) {
            let mut sql = String::from(
                "SELECT level, target_id, period, period_start, amount FROM budget_usage WHERE ",
            );
            for i in 0..chunk.len() {
                let n = i * 4;
                if i > 0 {
                    sql.push_str(" OR ");
                }
                let _ = write!(
                    sql,
                    "(level = ${} AND target_id = ${} AND period = ${} AND period_start = ${})",
                    n + 1,
                    n + 2,
                    n + 3,
                    n + 4
                );
            }
            let mut q = Q::new(sql);
            for k in chunk {
                q = bind_key(q, k);
            }
            for row in q.fetch_all(&mut tx).await? {
                let key: RowKey = (
                    row.text("level")?,
                    row.uuid("target_id")?,
                    row.text("period")?,
                    row.ms("period_start")?,
                );
                found.insert(key, row.i64("amount")?);
            }
        }
        tx.commit().await?;
        Ok(read
            .iter()
            .map(|k| found.get(&row_key(k)).copied().unwrap_or(0))
            .collect())
    }

    /// Liest Budgetstände (Admin-API, ohne Schreiben).
    pub async fn budget_usage(&self, keys: &[BudgetKey]) -> Result<Vec<i64>, StoreError> {
        self.sync_budget_usage(&[], keys, 0).await
    }

    /// Löscht Budgetstände, deren Zeitraum vor `before_ms` begann (außer `total`).
    pub async fn prune_budget_usage(&self, before_ms: i64) -> Result<u64, StoreError> {
        Ok(
            Q::new("DELETE FROM budget_usage WHERE period <> 'total' AND period_start < $1")
                .bind(Val::ms(before_ms))
                .execute(&self.pool)
                .await?,
        )
    }
}

#[async_trait::async_trait]
impl BudgetLedger for Store {
    async fn sync(
        &self,
        deltas: &[(BudgetKey, i64)],
        read: &[BudgetKey],
        now_ms: i64,
    ) -> Result<Vec<i64>, LedgerError> {
        self.sync_budget_usage(deltas, read, now_ms)
            .await
            .map_err(|e| LedgerError(e.to_string()))
    }
}
