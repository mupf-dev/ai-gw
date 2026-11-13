// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Backend `local`: exakt je Instanz, ohne externe Abhängigkeit.

use std::sync::{Arc, Weak};
use std::time::Duration;

use aigw_core::config::CounterBackendKind;

use crate::engine::Engine;
use crate::{Clock, CounterBackend, LimitCheck, Rejection, Reservation, SystemClock, Window};

/// Zähler im Prozess-RAM (Sliding Window, geshardete Map, kurze Sperren je Schlüssel).
#[derive(Debug)]
pub struct LocalCounters {
    engine: Arc<Engine>,
}

impl Default for LocalCounters {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalCounters {
    /// Backend mit Systemuhr.
    pub fn new() -> Self {
        Self::with_clock(Arc::new(SystemClock))
    }

    /// Backend mit eigener Zeitquelle (Tests).
    pub fn with_clock(clock: Arc<dyn Clock>) -> Self {
        Self {
            engine: Arc::new(Engine::new(clock, false)),
        }
    }

    /// Entfernt abgelaufene Zähler; liefert deren Anzahl.
    pub fn purge_expired(&self) -> usize {
        self.engine.purge_expired()
    }

    /// Anzahl gehaltener Zähler.
    pub fn len(&self) -> usize {
        self.engine.len()
    }

    /// `true`, wenn keine Zähler gehalten werden.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Startet die periodische Speicherbereinigung (endet mit dem Backend).
    pub fn spawn_janitor(&self, interval: Duration) {
        spawn_janitor(Arc::downgrade(&self.engine), interval);
    }

    /// Synchrone Variante von [`CounterBackend::reserve`].
    pub fn try_reserve(&self, checks: &[LimitCheck<'_>]) -> Result<Reservation, Rejection> {
        self.engine.reserve(checks)
    }

    /// Synchrone Variante von [`CounterBackend::settle`].
    pub fn settle_now(&self, reservation: Reservation, actual: u64) {
        self.engine.settle(&reservation.booked, actual);
    }

    /// Synchrone Variante von [`CounterBackend::get`].
    pub fn current(&self, key: &str, window: Window) -> u64 {
        self.engine.get(key, window)
    }
}

/// Bereinigt die Engine alle `interval`, solange sie existiert.
pub(crate) fn spawn_janitor(engine: Weak<Engine>, interval: Duration) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tick.tick().await;
            let Some(engine) = engine.upgrade() else {
                break;
            };
            let removed = engine.purge_expired();
            if removed > 0 {
                tracing::debug!(removed, remaining = engine.len(), "Zähler bereinigt");
            }
        }
    });
}

#[async_trait::async_trait]
impl CounterBackend for LocalCounters {
    async fn reserve(&self, checks: &[LimitCheck<'_>]) -> Result<Reservation, Rejection> {
        self.engine.reserve(checks)
    }

    async fn settle(&self, reservation: Reservation, actual: u64) {
        self.engine.settle(&reservation.booked, actual);
    }

    async fn get(&self, key: &str, window: Window) -> u64 {
        self.engine.get(key, window)
    }

    fn kind(&self) -> CounterBackendKind {
        CounterBackendKind::Local
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, unused_must_use)]

    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::ManualClock;

    #[tokio::test]
    async fn trait_roundtrip() {
        let c: Arc<dyn CounterBackend> = Arc::new(LocalCounters::new());
        assert_eq!(c.kind(), CounterBackendKind::Local);
        let r = c
            .reserve(&[LimitCheck::new("k", Window::Minute, 10, 4)])
            .await
            .unwrap();
        assert!(r.is_local_fallback());
        c.settle(r, 2).await;
        assert_eq!(c.get("k", Window::Minute).await, 2);
        assert!(c.reserve(&[]).await.unwrap().quotas().is_empty());
    }

    #[tokio::test]
    async fn split_off_settles_parts_separately() {
        let c = LocalCounters::new();
        let mut r = c
            .try_reserve(&[
                LimitCheck::fixed("req", Window::Minute, 10, 1),
                LimitCheck::new("tok", Window::Minute, 1000, 100),
                LimitCheck::new("in", Window::Minute, 1000, 40),
            ])
            .unwrap();
        assert_eq!(r.len(), 3);
        let mut tokens = r.split_off(1);
        let input = tokens.split_off(1);
        assert_eq!((r.len(), tokens.len(), input.len()), (1, 1, 1));
        assert_eq!(tokens.quotas()[0].limit, 1000);
        c.settle_now(r, 0);
        c.settle_now(tokens, 70);
        c.settle_now(input, 10);
        assert_eq!(c.current("req", Window::Minute), 1);
        assert_eq!(c.current("tok", Window::Minute), 70);
        assert_eq!(c.current("in", Window::Minute), 10);
        let mut e = c.try_reserve(&[]).unwrap();
        assert!(e.split_off(5).is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn many_tasks_never_exceed_limit() {
        let c = Arc::new(LocalCounters::new());
        let granted = Arc::new(AtomicU64::new(0));
        let mut tasks = Vec::new();
        for t in 0..64u64 {
            let c = Arc::clone(&c);
            let granted = Arc::clone(&granted);
            tasks.push(tokio::spawn(async move {
                for i in 0..500u64 {
                    let key = format!("key{}", t % 8);
                    let amount = 1 + (t + i) % 7;
                    let checks = [
                        LimitCheck::new(&key, Window::Hour, 2_000, amount),
                        LimitCheck::new("team", Window::Hour, 5_000, amount),
                        LimitCheck::fixed("org-req", Window::Hour, 20_000, 1),
                    ];
                    if let Ok(r) = c.reserve(&checks).await {
                        // Hälfte der Reservierung tatsächlich verbraucht
                        let actual = amount / 2;
                        granted.fetch_add(actual, Ordering::Relaxed);
                        c.settle(r, actual).await;
                    }
                }
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        let team = c.get("team", Window::Hour).await;
        assert_eq!(team, granted.load(Ordering::Relaxed));
        assert!(team <= 5_000);
        let keys: u64 = (0..8)
            .map(|i| c.current(&format!("key{i}"), Window::Hour))
            .sum();
        assert_eq!(keys, team);
    }

    #[tokio::test(start_paused = true)]
    async fn janitor_purges() {
        let clock = Arc::new(ManualClock::new(10_000_000));
        let c = LocalCounters::with_clock(clock.clone());
        c.try_reserve(&[LimitCheck::new("k", Window::Second, 1, 1)])
            .unwrap();
        c.spawn_janitor(Duration::from_secs(1));
        clock.advance(5_000);
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        assert!(c.is_empty());
    }
}
