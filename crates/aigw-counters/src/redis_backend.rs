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
//! Verbindung und schaltet zurück. Was während des Ausfalls lokal gebucht wurde und noch zum
//! laufenden Fenster gehört, trägt er danach in Redis nach (F28); ebenso Abgleiche
//! (`settle`) von Redis-Reservierungen, die während des Ausfalls anfielen. Abgelaufene
//! Buckets werden verworfen ([`CounterStats`]).
//!
//! Nur Einzelinstanz bzw. Sentinel-Primary: Ein Server im Cluster-Modus (`INFO cluster` →
//! `cluster_enabled:1`) bricht den Start ab (F27) – ein `reserve` über mehrere Schlüssel
//! bräuchte dort denselben Hash-Slot.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock, Weak};
use std::time::Duration;

use aigw_core::config::CounterBackendKind;
use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use redis::{RedisResult, Script};

use crate::engine::{Engine, SyncDelta};
use crate::local::spawn_janitor;
use crate::{
    Booked, Clock, CounterBackend, CounterError, CounterStats, DEFAULT_JANITOR, Health, LimitCheck,
    Origin, Quota, Rejection, Reservation, SettleMode, SystemClock, Window,
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
    /// Höchstzahl nachzutragender Deltas (Rückstau nach einem Ausfall, Standard 100 000);
    /// darüber werden die ältesten verworfen.
    pub max_backlog: usize,
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
            max_backlog: 100_000,
        }
    }
}

/// Deltas je Bucket in einem Aufruf des Nachtrag-Skripts.
const REPLAY_CHUNK: usize = 256;

/// Lua: alle Limits prüfen, nur wenn alle passen buchen.
/// KEYS[i]; ARGV je i: Fensterlänge (ms), Limit, Menge, Beginn eines festen Zeitraums (0 =
/// gleitend). Feste Zeiträume (F25): Bucket = Beginn, kein voriger Bucket, Ablauf nach dem Ende.
/// Antwort: {ok, now_ms, (bucket, cur, prev) je i}; `cur` enthält bereits Mengen früherer
/// Einträge mit gleichem Schlüssel in diesem Aufruf.
const RESERVE_LUA: &str = r"
local t = redis.call('TIME')
local now = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)
local out = {1, now}
local pend = {}
local ok = 1
for i = 1, #KEYS do
  local b = (i - 1) * 4
  local len = tonumber(ARGV[b + 1])
  local limit = tonumber(ARGV[b + 2])
  local amount = tonumber(ARGV[b + 3])
  local start = tonumber(ARGV[b + 4])
  local idx, cur, prev, est
  if start > 0 then
    idx = start
    cur = (tonumber(redis.call('HGET', KEYS[i], string.format('%d', idx))) or 0) + (pend[KEYS[i]] or 0)
    prev = 0
    est = cur
  else
    idx = math.floor(now / len)
    local v = redis.call('HMGET', KEYS[i], string.format('%d', idx), string.format('%d', idx - 1))
    cur = (tonumber(v[1]) or 0) + (pend[KEYS[i]] or 0)
    prev = tonumber(v[2]) or 0
    local elapsed = now - idx * len
    est = cur + math.floor(prev * ((len - elapsed) / len))
  end
  if est + amount > limit then ok = 0 end
  pend[KEYS[i]] = (pend[KEYS[i]] or 0) + amount
  out[#out + 1] = idx
  out[#out + 1] = cur
  out[#out + 1] = prev
end
if ok == 1 then
  for i = 1, #KEYS do
    local b = (i - 1) * 4
    local len = tonumber(ARGV[b + 1])
    local amount = tonumber(ARGV[b + 3])
    local start = tonumber(ARGV[b + 4])
    if start > 0 then
      if amount > 0 then
        redis.call('HINCRBY', KEYS[i], string.format('%d', start), amount)
      end
      local ttl = start + len - now + 60000
      if ttl < 60000 then ttl = 60000 end
      redis.call('PEXPIRE', KEYS[i], ttl)
    else
      local idx = math.floor(now / len)
      if amount > 0 then
        redis.call('HINCRBY', KEYS[i], string.format('%d', idx), amount)
      end
      redis.call('HDEL', KEYS[i], string.format('%d', idx - 2))
      redis.call('PEXPIRE', KEYS[i], len * 2 + 1000)
    end
  end
end
out[1] = ok
return out
";

/// Lua: Korrekturen verbuchen (auch das Nachtragen nach einem Ausfall, F28). ARGV je i:
/// Fensterlänge, Bucket, Delta (±), fester Zeitraum (1/0). Nur Buckets, die laut Serverzeit
/// noch zum Fenster zählen; negative Korrekturen sättigen bei 0.
const SETTLE_LUA: &str = r"
local t = redis.call('TIME')
local now = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)
local applied = 0
for i = 1, #KEYS do
  local b = (i - 1) * 4
  local len = tonumber(ARGV[b + 1])
  local idx = tonumber(ARGV[b + 2])
  local delta = tonumber(ARGV[b + 3])
  local fixed = tonumber(ARGV[b + 4]) == 1
  local live
  if fixed then live = now < idx + len else live = idx + 1 >= math.floor(now / len) end
  if live then
    applied = applied + 1
    local f = string.format('%d', idx)
    if delta > 0 then
      redis.call('HINCRBY', KEYS[i], f, delta)
      if fixed then
        local ttl = idx + len - now + 60000
        if ttl < 60000 then ttl = 60000 end
        redis.call('PEXPIRE', KEYS[i], ttl)
      else
        redis.call('PEXPIRE', KEYS[i], len * 2 + 1000)
      end
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
return applied
";

/// Lua: Stand lesen. ARGV[1]: Fensterlänge, ARGV[2]: Beginn eines festen Zeitraums (0 =
/// gleitend). Antwort {now_ms, bucket, cur, prev}.
const GET_LUA: &str = r"
local t = redis.call('TIME')
local now = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)
local len = tonumber(ARGV[1])
local start = tonumber(ARGV[2])
if start > 0 then
  local v = redis.call('HGET', KEYS[1], string.format('%d', start))
  return {now, start, tonumber(v) or 0, 0}
end
local idx = math.floor(now / len)
local v = redis.call('HMGET', KEYS[1], string.format('%d', idx), string.format('%d', idx - 1))
return {now, idx, tonumber(v[1]) or 0, tonumber(v[2]) or 0}
";

/// Rückstau-Schlüssel: (Zählerschlüssel, Fenster, Bucket).
type BacklogKey = (Box<str>, Window, u64);

/// Beginn eines festen Zeitraums für die Skripte (0 = gleitend).
fn period_start(w: Window) -> u64 {
    match w {
        Window::Period { start_ms, .. } => start_ms.max(1),
        _ => 0,
    }
}

/// Fehler beim Verbinden.
#[derive(Debug)]
enum ConnectError {
    /// Server im Cluster-Modus (F27) – nicht unterstützt.
    Cluster,
    /// Sonstiger Fehler (nicht erreichbar, Zeitüberschreitung …).
    Other(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cluster => f.write_str(CLUSTER_MSG),
            Self::Other(e) => f.write_str(e),
        }
    }
}

const CLUSTER_MSG: &str = "Redis läuft im Cluster-Modus (cluster_enabled:1) – nicht unterstützt; \
     counters.redis_url muss auf eine Einzelinstanz oder den Sentinel-Primary zeigen";

/// `true`, wenn die Antwort von `INFO cluster` den Cluster-Modus meldet.
fn is_cluster_info(info: &str) -> bool {
    info.lines()
        .filter_map(|l| l.trim().split_once(':'))
        .any(|(k, v)| k == "cluster_enabled" && v.trim() == "1")
}

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
    /// Nachzutragende Deltas (während des Ausfalls gebucht bzw. abgeglichen, F28).
    backlog: Mutex<HashMap<BacklogKey, i64>>,
    /// Serialisiert das Nachtragen.
    replay_lock: tokio::sync::Mutex<()>,
    replayed: AtomicU64,
    dropped: AtomicU64,
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
                // Änderungen mitschreiben: Nachtragen nach dem Ausfall (F28).
                local: Arc::new(Engine::new(clock, true)),
                degraded: AtomicBool::new(true),
                last_error: Mutex::new(Some("noch nicht verbunden".to_owned())),
                lost_settles: AtomicU64::new(0),
                backlog: Mutex::new(HashMap::new()),
                replay_lock: tokio::sync::Mutex::new(()),
                replayed: AtomicU64::new(0),
                dropped: AtomicU64::new(0),
                opts,
            }),
        });
        match this.inner.try_connect().await {
            Ok(()) => {}
            Err(ConnectError::Cluster) => return Err(CounterError::RedisCluster),
            Err(ConnectError::Other(e)) => {
                tracing::warn!(error = %e, "Redis nicht erreichbar – Zähler laufen lokal weiter");
                this.inner.set_error(e);
            }
        }
        spawn_janitor(Arc::downgrade(&this.inner.local), DEFAULT_JANITOR);
        spawn_monitor(Arc::downgrade(&this.inner));
        Ok(this)
    }

    /// Stellt den Abgleich einer Redis-Reservierung während eines Ausfalls zurück (F28).
    fn defer_settle(&self, booked: &[Booked], actual: u64, positive_only: bool) {
        self.inner.lost_settles.fetch_add(1, Ordering::Relaxed);
        let deltas = booked.iter().filter_map(|b| {
            if b.settle == SettleMode::Keep {
                return None;
            }
            let delta = i128::from(actual) - i128::from(b.amount);
            (delta > 0 || (delta < 0 && !positive_only)).then(|| SyncDelta {
                key: b.key.clone(),
                window: b.window,
                bucket: b.bucket,
                delta: i64::try_from(delta).unwrap_or(if delta > 0 { i64::MAX } else { i64::MIN }),
            })
        });
        self.inner.push_backlog(deltas.collect::<Vec<_>>(), 0);
    }

    /// `true`, solange der lokale Rückfall aktiv ist.
    pub fn is_degraded(&self) -> bool {
        self.inner.degraded.load(Ordering::Acquire)
    }

    /// Anzahl Abgleiche von Redis-Reservierungen, die während eines Ausfalls anfielen (sie
    /// werden zurückgestellt und nach der Rückkehr nachgetragen, F28).
    pub fn lost_settles(&self) -> u64 {
        self.inner.lost_settles.load(Ordering::Relaxed)
    }

    /// Trägt zurückgestellte Deltas sofort nach (sonst im Takt von `reconnect_interval`).
    pub async fn replay_now(&self) -> Result<usize, String> {
        self.inner.replay().await
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
            inv.arg(c.window.length_ms())
                .arg(c.limit)
                .arg(c.amount)
                .arg(period_start(c.window));
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
            inv.arg(b.window.length_ms())
                .arg(b.bucket)
                .arg(delta)
                .arg(u8::from(!b.window.is_sliding()));
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
            .arg(period_start(window))
            .invoke_async(con)
            .await?;
        let [now, idx, cur, prev] = out[..] else {
            return Err(redis::RedisError::from((
                redis::ErrorKind::UnexpectedReturnType,
                "get: unerwartete Antwortlänge",
            )));
        };
        let (now, idx) = (to_u64(now), to_u64(idx));
        if !window.counts(idx, now) {
            return Ok(0);
        }
        let elapsed = now.saturating_sub(window.bucket_start(idx));
        Ok(window.estimate(to_u64(cur), to_u64(prev), elapsed))
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
        let elapsed = now.saturating_sub(c.window.bucket_start(idx));
        let ev = c.window.evaluate(cur, prev, elapsed, c.limit, c.amount);
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
                reset_after: Duration::from_millis(c.window.reset_after_ms(
                    cur.saturating_add(c.amount),
                    prev,
                    elapsed,
                )),
            });
        } else if !ev.admit {
            let ra = c
                .window
                .retry_after_ms(cur, prev, elapsed, c.limit, c.amount);
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
    async fn try_connect(&self) -> Result<(), ConnectError> {
        let cfg = ConnectionManagerConfig::new()
            .set_connection_timeout(Some(self.opts.connect_timeout))
            .set_response_timeout(Some(self.opts.timeout))
            .set_number_of_retries(1);
        let other = |e: String| ConnectError::Other(e);
        let mut conns = Vec::with_capacity(self.opts.pool_size.max(1));
        for i in 0..self.opts.pool_size.max(1) {
            let fut = ConnectionManager::new_with_config(self.client.clone(), cfg.clone());
            let mut con = tokio::time::timeout(self.opts.connect_timeout, fut)
                .await
                .map_err(|_| other("Zeitüberschreitung beim Verbinden".to_owned()))?
                .map_err(|e| other(e.to_string()))?;
            if i == 0 {
                // F27: Cluster-Modus erkennen (ein `reserve` über mehrere Schlüssel bräuchte
                // denselben Hash-Slot). Ältere Server ohne Abschnitt `cluster` melden nichts.
                let info = tokio::time::timeout(
                    self.opts.connect_timeout,
                    redis::cmd("INFO")
                        .arg("cluster")
                        .query_async::<String>(&mut con),
                )
                .await
                .map_err(|_| other("Zeitüberschreitung bei INFO cluster".to_owned()))?
                .map_err(|e| other(e.to_string()))?;
                if is_cluster_info(&info) {
                    return Err(ConnectError::Cluster);
                }
            }
            // Skripte vorab laden (EVALSHA trifft dann sofort).
            for s in [&self.reserve, &self.settle, &self.get] {
                let _: String = s
                    .prepare_invoke()
                    .load_async(&mut con)
                    .await
                    .map_err(|e| other(e.to_string()))?;
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
            return self.try_connect().await.map_err(|e| {
                if matches!(e, ConnectError::Cluster) {
                    tracing::error!("{CLUSTER_MSG}");
                }
                e.to_string()
            });
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

    /// Fügt Deltas dem Rückstau hinzu; abgelaufene Buckets und Überlauf werden verworfen.
    fn push_backlog(&self, deltas: impl IntoIterator<Item = SyncDelta>, dropped: usize) {
        let now = self.local.now_ms();
        let mut b = self.backlog.lock().unwrap_or_else(PoisonError::into_inner);
        let mut lost = dropped;
        for d in deltas {
            *b.entry((d.key, d.window, d.bucket)).or_default() += d.delta;
        }
        let before = b.len();
        b.retain(|(_, w, bucket), delta| *delta != 0 && w.counts(*bucket, now));
        lost += before - b.len();
        if b.len() > self.opts.max_backlog {
            let mut keys: Vec<BacklogKey> = b.keys().cloned().collect();
            keys.sort_by_key(|(_, w, bucket)| w.bucket_start(*bucket));
            let excess = b.len() - self.opts.max_backlog;
            for k in keys.into_iter().take(excess) {
                b.remove(&k);
            }
            lost += excess;
            tracing::warn!(
                verworfen = excess,
                "Rückstau der Redis-Zähler voll – älteste Deltas verworfen"
            );
        }
        if lost > 0 {
            self.dropped.fetch_add(lost as u64, Ordering::Relaxed);
        }
    }

    /// Sammelt lokale Deltas in den Rückstau (während des Ausfalls; begrenzt den Speicher).
    fn collect_local(&self) {
        let (deltas, dropped) = self.local.take_live_deltas();
        self.push_backlog(deltas, dropped);
    }

    /// Trägt den Rückstau und lokale Deltas in Redis nach (F28). Liefert die Anzahl
    /// angewendeter Deltas; bei einem Fehler bleibt der Rest im Rückstau.
    async fn replay(&self) -> Result<usize, String> {
        let _serial = self.replay_lock.lock().await;
        self.collect_local();
        let pending: Vec<(BacklogKey, i64)> =
            std::mem::take(&mut *self.backlog.lock().unwrap_or_else(PoisonError::into_inner))
                .into_iter()
                .collect();
        if pending.is_empty() {
            return Ok(0);
        }
        let Some(mut con) = self.conn() else {
            self.push_backlog(
                pending
                    .into_iter()
                    .map(|((key, window, bucket), delta)| SyncDelta {
                        key,
                        window,
                        bucket,
                        delta,
                    }),
                0,
            );
            return Err("keine Verbindung".to_owned());
        };
        let total = pending.len();
        let mut sent = 0usize;
        let mut applied = 0usize;
        let mut failure = None;
        for chunk in pending.chunks(REPLAY_CHUNK) {
            let mut inv = self.settle.prepare_invoke();
            for ((key, window, bucket), delta) in chunk {
                let mut k = String::with_capacity(self.opts.key_prefix.len() + key.len() + 4);
                k.push_str(&self.opts.key_prefix);
                k.push_str(window.code());
                k.push(':');
                k.push_str(key);
                inv.key(k);
                inv.arg(window.length_ms())
                    .arg(*bucket)
                    .arg(*delta)
                    .arg(u8::from(!window.is_sliding()));
            }
            let r =
                tokio::time::timeout(self.opts.timeout, inv.invoke_async::<i64>(&mut con)).await;
            match timeout_err(r) {
                Ok(n) => {
                    sent += chunk.len();
                    applied += usize::try_from(n).unwrap_or(0);
                }
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
        }
        // Vom Server nicht angewendet (Bucket laut Serverzeit abgelaufen) = verworfen.
        self.dropped
            .fetch_add((sent - applied.min(sent)) as u64, Ordering::Relaxed);
        self.replayed.fetch_add(applied as u64, Ordering::Relaxed);
        if let Some(e) = failure {
            self.push_backlog(
                pending
                    .into_iter()
                    .skip(sent)
                    .map(|((key, window, bucket), delta)| SyncDelta {
                        key,
                        window,
                        bucket,
                        delta,
                    }),
                0,
            );
            self.mark_down(e.clone());
            return Err(e);
        }
        tracing::info!(
            nachgetragen = applied,
            verworfen = sent - applied.min(sent),
            gesamt = total,
            "Redis-Zähler: lokal gebuchten Verbrauch nachgetragen"
        );
        Ok(applied)
    }

    fn stats(&self) -> CounterStats {
        CounterStats {
            replayed: self.replayed.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            pending: self
                .backlog
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .len() as u64,
        }
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
                // Lokal gebuchte Reservierungen werden auch nach der Rückkehr noch lokal
                // abgeglichen – solche Änderungen nachtragen (F28).
                let pending = !inner
                    .backlog
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .is_empty();
                if (inner.local.take_dirty() || pending)
                    && let Err(e) = inner.replay().await
                {
                    tracing::debug!(error = %e, "Nachtragen der Redis-Zähler fehlgeschlagen");
                }
                continue;
            }
            match inner.ping().await {
                Ok(()) => {
                    tracing::info!("Redis-Zähler wieder erreichbar – Rückfall beendet");
                    let _ = inner.local.take_dirty();
                    if let Err(e) = inner.replay().await {
                        tracing::debug!(error = %e, "Nachtragen der Redis-Zähler fehlgeschlagen");
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, "Redis weiterhin nicht erreichbar");
                    inner.set_error(e);
                    // Speicher begrenzen: abgelaufene Buckets laufend verwerfen.
                    inner.collect_local();
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
            self.defer_settle(&reservation.booked, actual, false);
            return;
        };
        let r = tokio::time::timeout(
            self.inner.opts.timeout,
            self.settle_redis(&mut con, &reservation.booked, actual),
        )
        .await;
        if let Err(e) = timeout_err(r) {
            // Unklar, ob das Skript noch lief: nur positive Korrekturen zurückstellen
            // (doppelt gebucht = konservativ); Freigaben würden sonst doppelt abziehen.
            self.defer_settle(&reservation.booked, actual, true);
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

    fn stats(&self) -> CounterStats {
        self.inner.stats()
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
    fn cluster_info_detection() {
        assert!(is_cluster_info("# Cluster\r\ncluster_enabled:1\r\n"));
        assert!(!is_cluster_info("# Cluster\r\ncluster_enabled:0\r\n"));
        assert!(!is_cluster_info(""));
    }

    #[test]
    fn interpret_period() {
        let p = Window::period(1_000_000, 1_060_000);
        let checks = [LimitCheck::new("a", p, 100, 10)];
        let r = interpret_reserve(&[1, 1_030_000, 1_000_000, 50, 0], &checks).unwrap();
        assert_eq!(r.quotas()[0].remaining, 40);
        assert_eq!(r.quotas()[0].reset_after, Duration::from_millis(30_000));
        assert_eq!(r.booked[0].bucket, 1_000_000);
        let rej = interpret_reserve(&[0, 1_030_000, 1_000_000, 95, 0], &checks).unwrap_err();
        assert_eq!(rej.retry_after, Some(Duration::from_millis(30_000)));
    }

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
