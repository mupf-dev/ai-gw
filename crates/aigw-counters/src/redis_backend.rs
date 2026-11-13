// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Backend `redis`: exakt im Cluster über Lua-Skripte (Redis ≥ 7 oder Valkey).
//!
//! Je Zähler und Fenster ein Hash `<prefix><Fensterkürzel>:<Schlüssel>` mit einem Feld je
//! Bucket-Nummer; der Schlüssel läuft nach zwei Fensterlängen ab. Das Skript `reserve` prüft
//! alle übergebenen Limits und bucht sie nur, wenn alle passen – atomar, weil Redis Skripte
//! am Stück ausführt. Als Uhr dient `TIME` des Redis-Servers (keine Uhrenabweichung zwischen
//! Gateway-Instanzen).
//!
//! Client: `redis` (redis-rs) mit `ConnectionManager` (multiplexte Verbindung mit
//! automatischem Pipelining nebenläufiger Befehle und Wiederverbindung), mehrere Manager im
//! Round-Robin als kleiner Pool. Skripte laufen per `EVALSHA` (bei `NOSCRIPT` lädt der Client
//! sie automatisch nach).
//!
//! Ausfall: Fehler oder Zeitüberschreitung → Warnung, [`Health::Degraded`], ab dann zählt die
//! lokale Engine (exakt je Instanz). Ein Hintergrund-Task prüft im Sekundentakt die
//! Verbindung und schaltet zurück.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock, Weak};
use std::time::Duration;

use aigw_core::config::CounterBackendKind;
use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use redis::{RedisResult, Script};

use crate::engine::Engine;
use crate::local::spawn_janitor;
use crate::window::{estimate, evaluate, reset_after_ms, retry_after_ms};
use crate::{
    Booked, Clock, CounterBackend, CounterError, DEFAULT_JANITOR, Health, LimitCheck, Origin,
    Quota, Rejection, Reservation, SettleMode, SystemClock, Window,
};

/// Einstellungen für `redis`.
#[derive(Debug, Clone)]
pub struct RedisOptions {
    /// Anzahl multiplexter Verbindungen (Standard 4).
    pub pool_size: usize,
    /// Zeitlimit je Operation, danach lokaler Rückfall (Standard 250 ms).
    pub timeout: Duration,
    /// Zeitlimit für den Verbindungsaufbau (Standard 2 s).
    pub connect_timeout: Duration,
    /// Prüfintervall im Rückfallbetrieb (Standard 1 s).
    pub reconnect_interval: Duration,
    /// Präfix aller Schlüssel (Standard `aigw:c:`).
    pub key_prefix: String,
    /// Zeitquelle der lokalen Rückfall-Engine (Standard: Systemuhr).
    pub clock: Option<Arc<dyn Clock>>,
}

impl Default for RedisOptions {
    fn default() -> Self {
        Self {
            pool_size: 4,
            timeout: Duration::from_millis(250),
            connect_timeout: Duration::from_secs(2),
            reconnect_interval: Duration::from_secs(1),
            key_prefix: "aigw:c:".to_owned(),
            clock: None,
        }
    }
}

/// Lua: alle Limits prüfen, nur wenn alle passen buchen.
/// KEYS[i]; ARGV je i: Fensterlänge (ms), Limit, Menge.
/// Antwort: {ok, now_ms, (bucket, cur, prev) je i}; `cur` enthält bereits Mengen früherer
/// Einträge mit gleichem Schlüssel in diesem Aufruf.
const RESERVE_LUA: &str = r"
local t = redis.call('TIME')
local now = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)
local out = {1, now}
local pend = {}
local ok = 1
for i = 1, #KEYS do
  local b = (i - 1) * 3
  local len = tonumber(ARGV[b + 1])
  local limit = tonumber(ARGV[b + 2])
  local amount = tonumber(ARGV[b + 3])
  local idx = math.floor(now / len)
  local v = redis.call('HMGET', KEYS[i], string.format('%d', idx), string.format('%d', idx - 1))
  local cur = (tonumber(v[1]) or 0) + (pend[KEYS[i]] or 0)
  local prev = tonumber(v[2]) or 0
  local elapsed = now - idx * len
  local est = cur + math.floor(prev * ((len - elapsed) / len))
  if est + amount > limit then ok = 0 end
  pend[KEYS[i]] = (pend[KEYS[i]] or 0) + amount
  out[#out + 1] = idx
  out[#out + 1] = cur
  out[#out + 1] = prev
end
if ok == 1 then
  for i = 1, #KEYS do
    local b = (i - 1) * 3
    local len = tonumber(ARGV[b + 1])
    local amount = tonumber(ARGV[b + 3])
    local idx = math.floor(now / len)
    if amount > 0 then
      redis.call('HINCRBY', KEYS[i], string.format('%d', idx), amount)
    end
    redis.call('HDEL', KEYS[i], string.format('%d', idx - 2))
    redis.call('PEXPIRE', KEYS[i], len * 2 + 1000)
  end
end
out[1] = ok
return out
";

/// Lua: Korrekturen verbuchen. ARGV je i: Fensterlänge, Bucket, Delta (±).
const SETTLE_LUA: &str = r"
local t = redis.call('TIME')
local now = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)
for i = 1, #KEYS do
  local b = (i - 1) * 3
  local len = tonumber(ARGV[b + 1])
  local idx = tonumber(ARGV[b + 2])
  local delta = tonumber(ARGV[b + 3])
  if idx + 1 >= math.floor(now / len) then
    local f = string.format('%d', idx)
    if delta > 0 then
      redis.call('HINCRBY', KEYS[i], f, delta)
      redis.call('PEXPIRE', KEYS[i], len * 2 + 1000)
    else
      local v = tonumber(redis.call('HGET', KEYS[i], f))
      if v then
        local nv = v + delta
        if nv > 0 then
          redis.call('HSET', KEYS[i], f, string.format('%d', nv))
        else
          redis.call('HDEL', KEYS[i], f)
        end
      end
    end
  end
end
return 1
";

/// Lua: Stand lesen. ARGV[1]: Fensterlänge. Antwort {now_ms, bucket, cur, prev}.
const GET_LUA: &str = r"
local t = redis.call('TIME')
local now = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)
local len = tonumber(ARGV[1])
local idx = math.floor(now / len)
local v = redis.call('HMGET', KEYS[1], string.format('%d', idx), string.format('%d', idx - 1))
return {now, idx, tonumber(v[1]) or 0, tonumber(v[2]) or 0}
";

#[derive(Debug)]
struct Inner {
    client: redis::Client,
    conns: RwLock<Vec<ConnectionManager>>,
    next: AtomicUsize,
    reserve: Script,
    settle: Script,
    get: Script,
    local: Arc<Engine>,
    degraded: AtomicBool,
    last_error: Mutex<Option<String>>,
    lost_settles: AtomicU64,
    opts: RedisOptions,
}

/// Exakte, verteilte Zähler über Redis mit lokalem Rückfall.
#[derive(Debug, Clone)]
pub struct RedisCounters {
    inner: Arc<Inner>,
}

fn to_u64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

impl RedisCounters {
    /// Verbindet sich mit `url` (`redis://…` bzw. `rediss://…` für TLS). Ist Redis nicht
    /// erreichbar, startet das Backend im lokalen Rückfall und verbindet sich später.
    pub async fn connect(url: &str, opts: RedisOptions) -> Result<Arc<Self>, CounterError> {
        let client = redis::Client::open(url).map_err(|e| CounterError::Redis(e.to_string()))?;
        let clock = opts.clock.clone().unwrap_or_else(|| Arc::new(SystemClock));
        let this = Arc::new(Self {
            inner: Arc::new(Inner {
                client,
                conns: RwLock::new(Vec::new()),
                next: AtomicUsize::new(0),
                reserve: Script::new(RESERVE_LUA),
                settle: Script::new(SETTLE_LUA),
                get: Script::new(GET_LUA),
                local: Arc::new(Engine::new(clock, false)),
                degraded: AtomicBool::new(true),
                last_error: Mutex::new(Some("noch nicht verbunden".to_owned())),
                lost_settles: AtomicU64::new(0),
                opts,
            }),
        });
        if let Err(e) = this.inner.try_connect().await {
            tracing::warn!(error = %e, "Redis nicht erreichbar – Zähler laufen lokal weiter");
            this.inner.set_error(e);
        }
        spawn_janitor(Arc::downgrade(&this.inner.local), DEFAULT_JANITOR);
        spawn_monitor(Arc::downgrade(&this.inner));
        Ok(this)
    }

    /// `true`, solange der lokale Rückfall aktiv ist.
    pub fn is_degraded(&self) -> bool {
        self.inner.degraded.load(Ordering::Acquire)
    }

    /// Anzahl Abgleiche, die wegen eines Redis-Ausfalls verloren gingen.
    pub fn lost_settles(&self) -> u64 {
        self.inner.lost_settles.load(Ordering::Relaxed)
    }

    fn redis_key(&self, window: Window, key: &str) -> String {
        let p = &self.inner.opts.key_prefix;
        let mut s = String::with_capacity(p.len() + key.len() + 4);
        s.push_str(p);
        s.push_str(window.code());
        s.push(':');
        s.push_str(key);
        s
    }

    async fn reserve_redis(
        &self,
        con: &mut ConnectionManager,
        checks: &[LimitCheck<'_>],
    ) -> RedisResult<Result<Reservation, Rejection>> {
        let mut inv = self.inner.reserve.prepare_invoke();
        for c in checks {
            inv.key(self.redis_key(c.window, c.key));
            inv.arg(c.window.length_ms()).arg(c.limit).arg(c.amount);
        }
        let out: Vec<i64> = inv.invoke_async(con).await?;
        if out.len() != 2 + 3 * checks.len() {
            return Err(redis::RedisError::from((
                redis::ErrorKind::UnexpectedReturnType,
                "reserve: unerwartete Antwortlänge",
            )));
        }
        Ok(interpret_reserve(&out, checks))
    }

    async fn settle_redis(
        &self,
        con: &mut ConnectionManager,
        booked: &[Booked],
        actual: u64,
    ) -> RedisResult<()> {
        let mut inv = self.inner.settle.prepare_invoke();
        let mut any = false;
        for b in booked {
            if b.settle == SettleMode::Keep {
                continue;
            }
            let delta = i128::from(actual) - i128::from(b.amount);
            if delta == 0 {
                continue;
            }
            let delta = i64::try_from(delta).unwrap_or(if delta > 0 { i64::MAX } else { i64::MIN });
            inv.key(self.redis_key(b.window, &b.key));
            inv.arg(b.window.length_ms()).arg(b.bucket).arg(delta);
            any = true;
        }
        if any {
            let _: i64 = inv.invoke_async(con).await?;
        }
        Ok(())
    }

    async fn get_redis(
        &self,
        con: &mut ConnectionManager,
        key: &str,
        window: Window,
    ) -> RedisResult<u64> {
        let out: Vec<i64> = self
            .inner
            .get
            .key(self.redis_key(window, key))
            .arg(window.length_ms())
            .invoke_async(con)
            .await?;
        let [now, idx, cur, prev] = out[..] else {
            return Err(redis::RedisError::from((
                redis::ErrorKind::UnexpectedReturnType,
                "get: unerwartete Antwortlänge",
            )));
        };
        let len = window.length_ms();
        let elapsed = to_u64(now).saturating_sub(to_u64(idx) * len);
        Ok(estimate(to_u64(cur), to_u64(prev), elapsed, len))
    }
}

/// Wertet die Antwort des Reserve-Skripts aus (gleiche Rechenregeln wie die lokale Engine).
fn interpret_reserve(out: &[i64], checks: &[LimitCheck<'_>]) -> Result<Reservation, Rejection> {
    let ok = out[0] == 1;
    let now = to_u64(out[1]);
    let mut booked = Vec::with_capacity(checks.len());
    let mut quotas = Vec::with_capacity(checks.len());
    let mut worst: Option<(usize, Option<u64>, u64)> = None;
    for (i, c) in checks.iter().enumerate() {
        let b = 2 + 3 * i;
        let (idx, cur, prev) = (to_u64(out[b]), to_u64(out[b + 1]), to_u64(out[b + 2]));
        let len = c.window.length_ms();
        let elapsed = now.saturating_sub(idx * len);
        let ev = evaluate(cur, prev, elapsed, len, c.limit, c.amount);
        if ok {
            booked.push(Booked {
                key: c.key.into(),
                window: c.window,
                bucket: idx,
                amount: c.amount,
                settle: c.settle,
            });
            quotas.push(Quota {
                limit: c.limit,
                remaining: c.limit.saturating_sub(ev.estimate.saturating_add(c.amount)),
                reset_after: Duration::from_millis(reset_after_ms(
                    cur.saturating_add(c.amount),
                    prev,
                    elapsed,
                    len,
                )),
            });
        } else if !ev.admit {
            let ra = retry_after_ms(cur, prev, elapsed, len, c.limit, c.amount);
            let replace = match worst {
                None => true,
                Some((_, None, _)) => false,
                Some((_, Some(w), _)) => ra.is_none_or(|r| r > w),
            };
            if replace {
                worst = Some((i, ra, ev.estimate));
            }
        }
    }
    if ok {
        return Ok(Reservation {
            booked,
            quotas,
            origin: Origin::Redis,
        });
    }
    // Lua rechnet in Gleitkomma; bei Rundungsgrenzfällen ohne eigenen Befund das erste Limit
    // mit minimaler Wartezeit melden.
    let (i, ra, used) = worst.unwrap_or((0, Some(1), 0));
    let c = &checks[i];
    Err(Rejection {
        index: i,
        key: c.key.to_owned(),
        window: c.window,
        limit: c.limit,
        used,
        retry_after: ra.map(Duration::from_millis),
    })
}

impl Inner {
    async fn try_connect(&self) -> Result<(), String> {
        let cfg = ConnectionManagerConfig::new()
            .set_connection_timeout(Some(self.opts.connect_timeout))
            .set_response_timeout(Some(self.opts.timeout))
            .set_number_of_retries(1);
        let mut conns = Vec::with_capacity(self.opts.pool_size.max(1));
        for _ in 0..self.opts.pool_size.max(1) {
            let fut = ConnectionManager::new_with_config(self.client.clone(), cfg.clone());
            let mut con = tokio::time::timeout(self.opts.connect_timeout, fut)
                .await
                .map_err(|_| "Zeitüberschreitung beim Verbinden".to_owned())?
                .map_err(|e| e.to_string())?;
            // Skripte vorab laden (EVALSHA trifft dann sofort).
            for s in [&self.reserve, &self.settle, &self.get] {
                let _: String = s
                    .prepare_invoke()
                    .load_async(&mut con)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            conns.push(con);
        }
        *self.conns.write().unwrap_or_else(PoisonError::into_inner) = conns;
        self.degraded.store(false, Ordering::Release);
        *self
            .last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        Ok(())
    }

    async fn ping(&self) -> Result<(), String> {
        let Some(mut con) = self.conn() else {
            return self.try_connect().await;
        };
        let res = tokio::time::timeout(
            self.opts.timeout.max(Duration::from_millis(500)),
            redis::cmd("PING").query_async::<String>(&mut con),
        )
        .await;
        match res {
            Ok(Ok(_)) => {
                self.degraded.store(false, Ordering::Release);
                *self
                    .last_error
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = None;
                Ok(())
            }
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err("Zeitüberschreitung".to_owned()),
        }
    }

    fn conn(&self) -> Option<ConnectionManager> {
        let conns = self.conns.read().unwrap_or_else(PoisonError::into_inner);
        if conns.is_empty() {
            return None;
        }
        let i = self.next.fetch_add(1, Ordering::Relaxed) % conns.len();
        Some(conns[i].clone())
    }

    /// Verbindung für den Normalbetrieb (`None` im Rückfall).
    fn active_conn(&self) -> Option<ConnectionManager> {
        if self.degraded.load(Ordering::Acquire) {
            None
        } else {
            self.conn()
        }
    }

    fn set_error(&self, err: String) {
        *self
            .last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(err);
    }

    fn mark_down(&self, err: String) {
        if !self.degraded.swap(true, Ordering::AcqRel) {
            tracing::warn!(
                error = %err,
                "Redis-Zähler nicht erreichbar – Rückfall auf lokale Zähler (nur je Instanz exakt)"
            );
        }
        self.set_error(err);
    }
}

fn spawn_monitor(inner: Weak<Inner>) {
    tokio::spawn(async move {
        loop {
            let interval = match inner.upgrade() {
                Some(i) => i.opts.reconnect_interval,
                None => break,
            };
            tokio::time::sleep(interval).await;
            let Some(inner) = inner.upgrade() else {
                break;
            };
            if !inner.degraded.load(Ordering::Acquire) {
                continue;
            }
            match inner.ping().await {
                Ok(()) => tracing::info!("Redis-Zähler wieder erreichbar – Rückfall beendet"),
                Err(e) => {
                    tracing::debug!(error = %e, "Redis weiterhin nicht erreichbar");
                    inner.set_error(e);
                }
            }
        }
    });
}

fn timeout_err<T>(r: Result<RedisResult<T>, tokio::time::error::Elapsed>) -> Result<T, String> {
    match r {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("Zeitüberschreitung".to_owned()),
    }
}

#[async_trait::async_trait]
impl CounterBackend for RedisCounters {
    async fn reserve(&self, checks: &[LimitCheck<'_>]) -> Result<Reservation, Rejection> {
        if checks.is_empty() {
            return Ok(Reservation::empty(Origin::Redis));
        }
        if let Some(mut con) = self.inner.active_conn() {
            let r = tokio::time::timeout(
                self.inner.opts.timeout,
                self.reserve_redis(&mut con, checks),
            )
            .await;
            match timeout_err(r) {
                Ok(res) => return res,
                Err(e) => self.inner.mark_down(e),
            }
        }
        self.inner.local.reserve(checks)
    }

    async fn settle(&self, reservation: Reservation, actual: u64) {
        if reservation.origin == Origin::Local {
            self.inner.local.settle(&reservation.booked, actual);
            return;
        }
        let Some(mut con) = self.inner.active_conn() else {
            self.inner.lost_settles.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let r = tokio::time::timeout(
            self.inner.opts.timeout,
            self.settle_redis(&mut con, &reservation.booked, actual),
        )
        .await;
        if let Err(e) = timeout_err(r) {
            self.inner.lost_settles.fetch_add(1, Ordering::Relaxed);
            self.inner.mark_down(e);
        }
    }

    async fn get(&self, key: &str, window: Window) -> u64 {
        if let Some(mut con) = self.inner.active_conn() {
            let r = tokio::time::timeout(
                self.inner.opts.timeout,
                self.get_redis(&mut con, key, window),
            )
            .await;
            match timeout_err(r) {
                Ok(v) => return v,
                Err(e) => self.inner.mark_down(e),
            }
        }
        self.inner.local.get(key, window)
    }

    fn kind(&self) -> CounterBackendKind {
        CounterBackendKind::Redis
    }

    fn health(&self) -> Health {
        if self.is_degraded() {
            let err = self
                .inner
                .last_error
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
                .unwrap_or_default();
            Health::Degraded(format!("Redis nicht erreichbar, lokaler Rückfall: {err}"))
        } else {
            Health::Ok
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn interpret_ok_and_rejection() {
        let checks = [
            LimitCheck::new("a", Window::Minute, 100, 10),
            LimitCheck::new("b", Window::Minute, 50, 10),
        ];
        let now = 1_000 * 60_000;
        let r = interpret_reserve(&[1, now, 1_000, 20, 0, 1_000, 30, 0], &checks).unwrap();
        assert_eq!(r.quotas()[0].remaining, 70);
        assert_eq!(r.quotas()[1].remaining, 10);
        assert_eq!(r.booked[1].bucket, 1_000);

        let rej = interpret_reserve(&[0, now, 1_000, 20, 0, 1_000, 45, 0], &checks).unwrap_err();
        assert_eq!(rej.index, 1);
        assert_eq!(rej.used, 45);
        assert!(rej.retry_after.is_some());
    }
}
