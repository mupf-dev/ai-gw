// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Geteilter Zähler für strikte Budgets über Redis (F106: „Redis exakt“).
//!
//! Je Budgetstand ein Schlüssel `<präfix>{aigw-budget}:<ebene>:<ziel>:<zeitraum>:<beginn>` mit
//! Verbrauch + laufenden Reservierungen aller Instanzen in Nano-Einheiten. Ein Lua-Skript prüft
//! und bucht alle Ebenen einer Anfrage atomar; fehlt ein Schlüssel, wird er mit dem bekannten
//! Verbrauch der Instanz (Ledger) angelegt und läuft einen Tag nach Ende des Zeitraums ab. Das
//! Hash-Tag `{aigw-budget}` hält alle Schlüssel in einem Slot (Redis Cluster).
//!
//! Persistenz und Admin-Übersicht bleiben beim Ledger (`budget_usage`); der Redis-Zähler dient
//! nur der exakten Zulassung. Ist Redis nicht erreichbar, entscheidet die Instanz lokal
//! (Abgleich-Takt, siehe [`aigw_billing::Budgets`]). Grenze: Beträge bis ≈ 9 Mio. Einheiten je
//! Budget (2^53 Nano-Einheiten, Lua-Zahlen).

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use aigw_billing::{BudgetKey, LedgerError, SharedBudgets, SharedItem, SharedReserve};
use redis::Script;
use redis::aio::{ConnectionManager, ConnectionManagerConfig};

/// Nach einem gescheiterten Verbindungsversuch so lange sofort lokal entscheiden.
const RETRY: Duration = Duration::from_secs(5);

const RESERVE_LUA: &str = r"
local n = #KEYS
for i = 1, n do
  local b = (i - 1) * 4
  local v = redis.call('GET', KEYS[i])
  if not v then
    redis.call('SET', KEYS[i], ARGV[b + 1])
    local exp = tonumber(ARGV[b + 4])
    if exp > 0 then redis.call('PEXPIREAT', KEYS[i], exp) end
    v = ARGV[b + 1]
  end
  local cur = tonumber(v)
  local limit = tonumber(ARGV[b + 2])
  local amt = tonumber(ARGV[b + 3])
  if cur >= limit or cur + amt > limit then
    return {i, v}
  end
end
for i = 1, n do
  local amt = ARGV[(i - 1) * 4 + 3]
  if amt ~= '0' then redis.call('INCRBY', KEYS[i], amt) end
end
return {0, '0'}
";

const ADJUST_LUA: &str = r"
for i = 1, #KEYS do
  if redis.call('EXISTS', KEYS[i]) == 1 then
    redis.call('INCRBY', KEYS[i], ARGV[i])
  end
end
return 0
";

/// Strikte Budgets über Redis.
pub struct RedisBudgets {
    client: redis::Client,
    conn: Mutex<Option<ConnectionManager>>,
    /// Letzter gescheiterter Verbindungsaufbau.
    last_failure: Mutex<Option<Instant>>,
    /// Serialisiert den Verbindungsaufbau (gleichzeitige Anfragen warten auf denselben).
    connecting: tokio::sync::Mutex<()>,
    prefix: String,
    timeout: Duration,
    reserve: Script,
    adjust: Script,
}

impl std::fmt::Debug for RedisBudgets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisBudgets")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

fn err(e: impl std::fmt::Display) -> LedgerError {
    LedgerError(format!("Redis: {e}"))
}

impl RedisBudgets {
    /// Client für `url` (`redis://…`, `rediss://…`); verbunden wird beim ersten Zugriff.
    /// `prefix` wird jedem Schlüssel vorangestellt (z. B. `aigw:`).
    pub fn new(url: &str, prefix: &str) -> Result<Self, LedgerError> {
        Ok(Self {
            client: redis::Client::open(url).map_err(err)?,
            conn: Mutex::new(None),
            last_failure: Mutex::new(None),
            connecting: tokio::sync::Mutex::new(()),
            prefix: prefix.to_owned(),
            timeout: Duration::from_millis(250),
            reserve: Script::new(RESERVE_LUA),
            adjust: Script::new(ADJUST_LUA),
        })
    }

    /// Verbindung (der `ConnectionManager` verbindet sich nach Abbrüchen selbst neu). Der
    /// erste Aufbau wird nach einem Fehlschlag höchstens alle 5 s erneut versucht, damit
    /// Anfragen bei Ausfall nicht warten.
    async fn conn(&self) -> Result<ConnectionManager, LedgerError> {
        if let Some(c) = self
            .conn
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            return Ok(c.clone());
        }
        let recently_failed = || {
            self.last_failure
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_some_and(|t| t.elapsed() < RETRY)
        };
        if recently_failed() {
            return Err(err("nicht verbunden (nächster Versuch in Kürze)"));
        }
        let _guard = self.connecting.lock().await;
        // Ein anderer Aufrufer hat inzwischen verbunden bzw. ist gescheitert.
        if let Some(c) = self
            .conn
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            return Ok(c.clone());
        }
        if recently_failed() {
            return Err(err("nicht verbunden (nächster Versuch in Kürze)"));
        }
        let cfg = ConnectionManagerConfig::new()
            .set_connection_timeout(Some(self.timeout * 4))
            .set_response_timeout(Some(self.timeout))
            .set_number_of_retries(1);
        let res = tokio::time::timeout(
            self.timeout * 8,
            ConnectionManager::new_with_config(self.client.clone(), cfg),
        )
        .await
        .map_err(|_| err("Zeitüberschreitung beim Verbinden"))
        .and_then(|r| r.map_err(err));
        match res {
            Ok(c) => {
                *self.conn.lock().unwrap_or_else(PoisonError::into_inner) = Some(c.clone());
                Ok(c)
            }
            Err(e) => {
                *self
                    .last_failure
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
                Err(e)
            }
        }
    }

    fn key(&self, k: &BudgetKey) -> String {
        format!(
            "{}{{aigw-budget}}:{}:{}:{}:{}",
            self.prefix,
            k.level.as_str(),
            k.target,
            aigw_billing::period_code(k.period),
            k.period_start_ms
        )
    }
}

#[async_trait::async_trait]
impl SharedBudgets for RedisBudgets {
    async fn reserve(&self, items: &[SharedItem]) -> Result<SharedReserve, LedgerError> {
        let mut con = self.conn().await?;
        let mut inv = self.reserve.prepare_invoke();
        for it in items {
            inv.key(self.key(&it.key));
            inv.arg(it.base)
                .arg(it.limit)
                .arg(it.amount)
                .arg(it.expire_at_ms.unwrap_or(0));
        }
        let (index, current): (i64, String) =
            tokio::time::timeout(self.timeout, inv.invoke_async(&mut con))
                .await
                .map_err(|_| err("Zeitüberschreitung"))?
                .map_err(err)?;
        if index == 0 {
            return Ok(SharedReserve::Reserved);
        }
        Ok(SharedReserve::Rejected {
            index: usize::try_from(index - 1).unwrap_or(0),
            current: current.parse().unwrap_or(i64::MAX),
        })
    }

    async fn adjust(&self, deltas: &[(BudgetKey, i64)]) -> Result<(), LedgerError> {
        if deltas.is_empty() {
            return Ok(());
        }
        let mut con = self.conn().await?;
        let mut inv = self.adjust.prepare_invoke();
        for (k, d) in deltas {
            inv.key(self.key(k));
            inv.arg(*d);
        }
        let _: i64 = tokio::time::timeout(self.timeout * 4, inv.invoke_async(&mut con))
            .await
            .map_err(|_| err("Zeitüberschreitung"))?
            .map_err(err)?;
        Ok(())
    }
}
