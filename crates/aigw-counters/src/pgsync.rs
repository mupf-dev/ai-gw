// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Backend `postgres-sync`: lokal zählen, periodisch mit Postgres abgleichen.
//!
//! Jede Instanz entscheidet allein aus ihrer lokalen Engine. Darin steht je Bucket der eigene
//! Verbrauch (`local`) und die Summe der anderen Instanzen (`remote`) laut letztem Abgleich.
//! Alle `interval` (Standard 1 s) schreibt eine Instanz ihre Deltas per `INSERT … ON CONFLICT
//! DO UPDATE` in `counter_state` (eine Zeile je Schlüssel, Bucket und Instanz) und liest **in
//! derselben Anweisung** die Summe der anderen Instanzen für alle aktiven Buckets zurück.
//!
//! Ausfall der Datenbank: Es wird lokal weitergezählt (Warnung, [`Health::Degraded`]); die
//! Deltas sammeln sich im Rückstau und werden beim nächsten erfolgreichen Abgleich
//! nachgeholt, soweit ihr Bucket noch zum Fenster zählt.
//!
//! Für den Zugriff wird direkt ein `sqlx::PgPool` verwendet (nicht `aigw_store::Store`): Der
//! Abgleich braucht Postgres-Arrays (`UNNEST($1::text[], …)`), die der `Any`-Treiber des
//! Stores nicht kennt, und einen eigenen kleinen Pool, damit der Abgleich nicht mit der
//! Admin-API um Verbindungen konkurriert. Das Schema (Tabelle `counter_state`) gehört
//! weiterhin der Store-Migration.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use aigw_core::config::CounterBackendKind;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use crate::engine::Engine;
use crate::local::spawn_janitor;
use crate::{
    Clock, CounterBackend, CounterError, DEFAULT_JANITOR, Health, LimitCheck, Rejection,
    Reservation, SystemClock, Window,
};

/// Einstellungen für `postgres-sync`.
#[derive(Debug, Clone)]
pub struct PgSyncOptions {
    /// Abgleichintervall (Standard 1 s; empfohlen 1–2 s).
    pub interval: Duration,
    /// Kennung dieser Instanz in `counter_state.instance_id` (Standard: neue UUIDv7 je Start).
    pub instance_id: Option<String>,
    /// Wie oft abgelaufene Zeilen gelöscht werden (Standard 60 s).
    pub cleanup_interval: Duration,
    /// Höchstzahl zurückgestauter Deltas bei Datenbankausfall (Standard 100 000).
    pub max_backlog: usize,
    /// Zeitlimit je Abgleich (Standard 5 s).
    pub timeout: Duration,
    /// Zeitquelle (Standard: Systemuhr).
    pub clock: Option<Arc<dyn Clock>>,
}

impl Default for PgSyncOptions {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(1),
            instance_id: None,
            cleanup_interval: Duration::from_secs(60),
            max_backlog: 100_000,
            timeout: Duration::from_secs(5),
            clock: None,
        }
    }
}

/// Ergebnis eines Abgleichs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyncReport {
    /// Geschriebene Delta-Zeilen.
    pub written: usize,
    /// Gelesene Buckets.
    pub read: usize,
    /// Verworfene Deltas (Bucket bereits außerhalb des Fensters).
    pub dropped: usize,
}

type BacklogKey = (Box<str>, Window, u64);

#[derive(Debug, Default)]
struct SyncState {
    failing: Option<String>,
    last_cleanup_ms: u64,
}

#[derive(Debug)]
struct Inner {
    engine: Arc<Engine>,
    pool: PgPool,
    instance: String,
    opts: PgSyncOptions,
    backlog: Mutex<HashMap<BacklogKey, i64>>,
    sync_lock: tokio::sync::Mutex<()>,
    state: Mutex<SyncState>,
}

/// Zähler mit periodischem Abgleich über Postgres (unscharf im Cluster: ±Intervall).
#[derive(Debug, Clone)]
pub struct PgSyncCounters {
    inner: Arc<Inner>,
}

const SYNC_SQL: &str = r#"
WITH delta AS (
    SELECT * FROM UNNEST($1::text[], $2::bigint[], $3::bigint[]) AS d(counter_key, window_start, value)
), up AS (
    INSERT INTO counter_state AS c (counter_key, window_start, instance_id, value, updated_at)
    SELECT counter_key, window_start, $4, value, $5 FROM delta
    ON CONFLICT (counter_key, window_start, instance_id)
    DO UPDATE SET value = c.value + EXCLUDED.value, updated_at = EXCLUDED.updated_at
    RETURNING c.counter_key
), wanted AS (
    SELECT * FROM UNNEST($6::text[], $7::bigint[]) AS w(counter_key, window_start)
)
SELECT w.counter_key, w.window_start, COALESCE(SUM(s.value), 0)::bigint AS others,
       (SELECT count(*) FROM up)::bigint AS written
FROM wanted w
LEFT JOIN counter_state s
       ON s.counter_key = w.counter_key
      AND s.window_start = w.window_start
      AND s.instance_id <> $4
GROUP BY w.counter_key, w.window_start
"#;

const CLEANUP_SQL: &str =
    "DELETE FROM counter_state WHERE counter_key LIKE $1 AND window_start < $2";

/// Zeilenschlüssel in `counter_state`: `<Fensterkürzel>|<Schlüssel>`.
fn db_key(window: Window, key: &str) -> String {
    let mut s = String::with_capacity(key.len() + 3);
    s.push_str(window.code());
    s.push('|');
    s.push_str(key);
    s
}

fn parse_db_key(s: &str) -> Option<(Window, &str)> {
    let (code, key) = s.split_once('|')?;
    Some((Window::from_code(code)?, key))
}

fn window_start(window: Window, bucket: u64) -> Option<i64> {
    i64::try_from(bucket.checked_mul(window.length_ms())?).ok()
}

impl PgSyncCounters {
    /// Verbindet sich (verzögert: eine nicht erreichbare Datenbank verhindert den Start
    /// nicht) und startet Abgleich und Speicherbereinigung im Hintergrund.
    pub async fn connect(url: &str, opts: PgSyncOptions) -> Result<Arc<Self>, CounterError> {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(opts.timeout)
            .connect_lazy(url)?;
        Ok(Self::new(pool, opts))
    }

    /// Wie [`PgSyncCounters::connect`] mit vorhandenem Pool.
    pub fn new(pool: PgPool, opts: PgSyncOptions) -> Arc<Self> {
        let this = Arc::new(Self::new_manual(pool, opts));
        spawn_janitor(Arc::downgrade(&this.inner.engine), DEFAULT_JANITOR);
        spawn_sync(Arc::downgrade(&this.inner), this.inner.opts.interval);
        this
    }

    /// Ohne Hintergrund-Tasks; abgeglichen wird nur über [`PgSyncCounters::sync_now`]
    /// (Tests, Benchmarks).
    pub fn new_manual(pool: PgPool, opts: PgSyncOptions) -> Self {
        let clock = opts.clock.clone().unwrap_or_else(|| Arc::new(SystemClock));
        let instance = opts
            .instance_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
        Self {
            inner: Arc::new(Inner {
                engine: Arc::new(Engine::new(clock, true)),
                pool,
                instance,
                opts,
                backlog: Mutex::new(HashMap::new()),
                sync_lock: tokio::sync::Mutex::new(()),
                state: Mutex::new(SyncState::default()),
            }),
        }
    }

    /// Kennung dieser Instanz.
    pub fn instance_id(&self) -> &str {
        &self.inner.instance
    }

    /// Anzahl zurückgestauter Deltas (nach fehlgeschlagenem Abgleich).
    pub fn backlog_len(&self) -> usize {
        self.inner
            .backlog
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Gleicht sofort ab (sonst periodisch im Hintergrund).
    pub async fn sync_now(&self) -> Result<SyncReport, sqlx::Error> {
        self.inner.sync().await
    }
}

fn spawn_sync(inner: Weak<Inner>, interval: Duration) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let Some(inner) = inner.upgrade() else {
                break;
            };
            // Fehler werden in `sync` protokolliert; es wird lokal weitergezählt.
            let _ = inner.sync().await;
        }
    });
}

impl Inner {
    async fn sync(&self) -> Result<SyncReport, sqlx::Error> {
        let _serial = self.sync_lock.lock().await;
        let now = self.engine.now_ms();
        let (deltas, wanted) = self.engine.collect_sync();

        // Neue Deltas mit dem Rückstau zusammenfassen (je Zeile höchstens ein Eintrag,
        // sonst scheitert ON CONFLICT an doppelten Schlüsseln).
        let mut agg =
            std::mem::take(&mut *self.backlog.lock().unwrap_or_else(PoisonError::into_inner));
        for d in deltas {
            *agg.entry((d.key, d.window, d.bucket)).or_default() += d.delta;
        }
        let before = agg.len();
        agg.retain(|(_, w, bucket), delta| *delta != 0 && bucket + 1 >= w.bucket(now));
        let mut report = SyncReport {
            dropped: before - agg.len(),
            ..SyncReport::default()
        };

        let mut d_keys = Vec::with_capacity(agg.len());
        let mut d_starts = Vec::with_capacity(agg.len());
        let mut d_values = Vec::with_capacity(agg.len());
        for ((key, w, bucket), delta) in &agg {
            if let Some(ws) = window_start(*w, *bucket) {
                d_keys.push(db_key(*w, key));
                d_starts.push(ws);
                d_values.push(*delta);
            }
        }
        let mut w_keys = Vec::with_capacity(wanted.len());
        let mut w_starts = Vec::with_capacity(wanted.len());
        for w in &wanted {
            if let Some(ws) = window_start(w.window, w.bucket) {
                w_keys.push(db_key(w.window, &w.key));
                w_starts.push(ws);
            }
        }

        let now_i64 = i64::try_from(now).unwrap_or(i64::MAX);
        let query = sqlx::query_as::<_, (String, i64, i64, i64)>(SYNC_SQL)
            .bind(&d_keys)
            .bind(&d_starts)
            .bind(&d_values)
            .bind(&self.instance)
            .bind(now_i64)
            .bind(&w_keys)
            .bind(&w_starts);
        let result = if d_keys.is_empty() && w_keys.is_empty() {
            Ok(Vec::new())
        } else {
            match tokio::time::timeout(self.opts.timeout, query.fetch_all(&self.pool)).await {
                Ok(r) => r,
                Err(_) => Err(sqlx::Error::PoolTimedOut),
            }
        };

        let rows = match result {
            Ok(rows) => rows,
            Err(e) => {
                self.restore_backlog(agg);
                let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                if st.failing.is_none() {
                    tracing::warn!(
                        error = %e,
                        "Zähler-Abgleich mit Postgres fehlgeschlagen – zähle lokal weiter, Deltas werden nachgeholt"
                    );
                } else {
                    tracing::debug!(error = %e, "Zähler-Abgleich weiterhin fehlgeschlagen");
                }
                st.failing = Some(e.to_string());
                return Err(e);
            }
        };

        report.written = d_keys.len();
        report.read = rows.len();
        for (db_key, ws, others, _) in rows {
            let Some((window, key)) = parse_db_key(&db_key) else {
                continue;
            };
            let Ok(ws) = u64::try_from(ws) else { continue };
            let bucket = ws / window.length_ms();
            self.engine
                .set_remote(key, window, bucket, u64::try_from(others).unwrap_or(0));
        }

        let cleanup_due = {
            let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(err) = st.failing.take() {
                tracing::info!(
                    nachgeholt = report.written,
                    vorher = %err,
                    "Zähler-Abgleich mit Postgres wieder erfolgreich"
                );
            }
            let cleanup_ms =
                u64::try_from(self.opts.cleanup_interval.as_millis()).unwrap_or(u64::MAX);
            let due = now.saturating_sub(st.last_cleanup_ms) >= cleanup_ms;
            if due {
                st.last_cleanup_ms = now;
            }
            due
        };
        if cleanup_due {
            self.cleanup(now).await;
        }
        Ok(report)
    }

    fn restore_backlog(&self, agg: HashMap<BacklogKey, i64>) {
        let mut backlog = self.backlog.lock().unwrap_or_else(PoisonError::into_inner);
        for (k, v) in agg {
            *backlog.entry(k).or_default() += v;
        }
        if backlog.len() > self.opts.max_backlog {
            // Älteste Buckets zuerst verwerfen.
            let mut keys: Vec<BacklogKey> = backlog.keys().cloned().collect();
            keys.sort_by_key(|(_, w, b)| b.saturating_mul(w.length_ms()));
            let excess = backlog.len() - self.opts.max_backlog;
            for k in keys.into_iter().take(excess) {
                backlog.remove(&k);
            }
            tracing::warn!(
                verworfen = excess,
                "Rückstau des Zähler-Abgleichs voll – älteste Deltas verworfen"
            );
        }
    }

    /// Löscht Zeilen, deren Bucket in keinem Fenster mehr zählt.
    async fn cleanup(&self, now: u64) {
        for w in Window::ALL {
            let Some(cutoff) = window_start(w, w.bucket(now).saturating_sub(2)) else {
                continue;
            };
            let res = sqlx::query(CLEANUP_SQL)
                .bind(format!("{}|%", w.code()))
                .bind(cutoff)
                .execute(&self.pool)
                .await;
            if let Err(e) = res {
                tracing::debug!(error = %e, "Bereinigung von counter_state fehlgeschlagen");
                return;
            }
        }
    }
}

#[async_trait::async_trait]
impl CounterBackend for PgSyncCounters {
    async fn reserve(&self, checks: &[LimitCheck<'_>]) -> Result<Reservation, Rejection> {
        self.inner.engine.reserve(checks)
    }

    async fn settle(&self, reservation: Reservation, actual: u64) {
        self.inner.engine.settle(&reservation.booked, actual);
    }

    async fn get(&self, key: &str, window: Window) -> u64 {
        self.inner.engine.get(key, window)
    }

    fn kind(&self) -> CounterBackendKind {
        CounterBackendKind::PostgresSync
    }

    fn health(&self) -> Health {
        match &self
            .inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .failing
        {
            None => Health::Ok,
            Some(e) => Health::Degraded(format!("Postgres-Abgleich fehlgeschlagen: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_key_roundtrip() {
        let k = db_key(Window::Month, "team:a|b");
        assert_eq!(k, "mo|team:a|b");
        assert_eq!(parse_db_key(&k), Some((Window::Month, "team:a|b")));
        assert_eq!(parse_db_key("x|y"), None);
        assert_eq!(parse_db_key("nopipe"), None);
        assert_eq!(window_start(Window::Minute, 3), Some(180_000));
        assert_eq!(window_start(Window::Month, u64::MAX), None);
    }
}
