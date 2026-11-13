// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Circuit Breaker je Deployment (AIGW-26).
//!
//! - **Geschlossen:** Ergebnisse landen in einem gleitenden Fenster aus Sekunden-Buckets.
//!   Erreicht die Fehlerquote `failureRate` bei mindestens `minRequests` Anfragen, öffnet er.
//! - **Offen:** keine Anfragen bis `openSecs` abgelaufen sind.
//! - **Halboffen:** genau eine Anfrage (Probe) wird durchgelassen; Erfolg schließt, Fehler
//!   öffnet erneut. Kommt die Probe nicht zurück (z. B. Client-Abbruch), darf nach weiteren
//!   `openSecs` die nächste Probe laufen.
//!
//! Die Auswahl liest nur Atomics (`available`, `try_admit`); das Fenster ist nur beim
//! Verbuchen eines Ergebnisses gesperrt.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use super::policy::BreakerConfig;

/// Zustand eines Circuit Breakers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    /// Anfragen laufen normal.
    Closed,
    /// Gesperrt.
    Open,
    /// Eine Probe ist erlaubt bzw. unterwegs.
    HalfOpen,
}

const CLOSED: u8 = 0;
const OPEN: u8 = 1;
const HALF_OPEN: u8 = 2;

/// Ergebnis der Zulassung.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admit {
    /// Normaler Durchlauf (geschlossen oder Breaker aus).
    Normal,
    /// Diese Anfrage ist die Probe im halboffenen Zustand.
    Probe,
    /// Gesperrt.
    Rejected,
}

#[derive(Debug, Clone, Copy, Default)]
struct Bucket {
    sec: u64,
    ok: u32,
    fail: u32,
}

/// Circuit Breaker.
#[derive(Debug)]
pub(crate) struct Breaker {
    state: AtomicU8,
    /// Offen: bis wann gesperrt; halboffen: ab wann eine neue Probe erlaubt ist (ms seit
    /// Prozessstart, siehe [`super::state::now_ms`]).
    until_ms: AtomicU64,
    window: Mutex<Vec<Bucket>>,
}

impl Default for Breaker {
    fn default() -> Self {
        Self {
            state: AtomicU8::new(CLOSED),
            until_ms: AtomicU64::new(0),
            window: Mutex::new(Vec::new()),
        }
    }
}

impl Breaker {
    pub(crate) fn state(&self) -> BreakerState {
        match self.state.load(Ordering::Acquire) {
            CLOSED => BreakerState::Closed,
            OPEN => BreakerState::Open,
            _ => BreakerState::HalfOpen,
        }
    }

    /// Kann das Deployment jetzt Anfragen annehmen (ohne Zustandsänderung)?
    pub(crate) fn available(&self, now_ms: u64) -> bool {
        self.state.load(Ordering::Acquire) == CLOSED
            || now_ms >= self.until_ms.load(Ordering::Acquire)
    }

    /// Verbleibende Sperrzeit (für `Retry-After`), `0` wenn verfügbar.
    pub(crate) fn remaining_ms(&self, now_ms: u64) -> u64 {
        if self.state.load(Ordering::Acquire) == CLOSED {
            0
        } else {
            self.until_ms.load(Ordering::Acquire).saturating_sub(now_ms)
        }
    }

    /// Lässt eine Anfrage zu. Im offenen/halboffenen Zustand nach Ablauf der Sperre genau eine
    /// (Probe); gleichzeitige Bewerber verlieren das `compare_exchange`.
    pub(crate) fn try_admit(&self, cfg: &BreakerConfig, now_ms: u64) -> Admit {
        if !cfg.enabled || self.state.load(Ordering::Acquire) == CLOSED {
            return Admit::Normal;
        }
        let until = self.until_ms.load(Ordering::Acquire);
        if now_ms < until {
            return Admit::Rejected;
        }
        let next = now_ms.saturating_add(duration_ms(cfg));
        if self
            .until_ms
            .compare_exchange(until, next, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.state.store(HALF_OPEN, Ordering::Release);
            Admit::Probe
        } else {
            Admit::Rejected
        }
    }

    /// Verbucht ein Ergebnis (`failure = true` für Fehler, die auf das Deployment zeigen).
    pub(crate) fn record(&self, cfg: &BreakerConfig, now_ms: u64, failure: bool) {
        if !cfg.enabled {
            return;
        }
        match self.state.load(Ordering::Acquire) {
            HALF_OPEN => {
                if failure {
                    self.open(cfg, now_ms);
                } else {
                    self.close();
                }
            }
            OPEN => {}
            _ => {
                let trip = {
                    let mut w = self
                        .window
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let n = cfg.window_secs.max(1) as usize;
                    if w.len() != n {
                        w.clear();
                        w.resize(n, Bucket::default());
                    }
                    let sec = now_ms / 1000;
                    let i = (sec % n as u64) as usize;
                    if w[i].sec != sec {
                        w[i] = Bucket {
                            sec,
                            ok: 0,
                            fail: 0,
                        };
                    }
                    if failure {
                        w[i].fail = w[i].fail.saturating_add(1);
                    } else {
                        w[i].ok = w[i].ok.saturating_add(1);
                    }
                    if failure {
                        let oldest = sec.saturating_sub(n as u64 - 1);
                        let (ok, fail) = w
                            .iter()
                            .filter(|b| b.sec >= oldest && b.sec <= sec)
                            .fold((0u64, 0u64), |(o, f), b| {
                                (o + u64::from(b.ok), f + u64::from(b.fail))
                            });
                        let total = ok + fail;
                        let trip = total >= u64::from(cfg.min_requests)
                            && fail as f64 >= cfg.failure_rate * total as f64;
                        if trip {
                            w.iter_mut().for_each(|b| *b = Bucket::default());
                        }
                        trip
                    } else {
                        false
                    }
                };
                if trip {
                    self.open(cfg, now_ms);
                }
            }
        }
    }

    /// Öffnet sofort (z. B. nach fehlgeschlagenen Health Checks).
    pub(crate) fn open(&self, cfg: &BreakerConfig, now_ms: u64) {
        self.until_ms
            .store(now_ms.saturating_add(duration_ms(cfg)), Ordering::Release);
        self.state.store(OPEN, Ordering::Release);
    }

    /// Schließt sofort und leert das Fenster.
    pub(crate) fn close(&self) {
        self.state.store(CLOSED, Ordering::Release);
        self.until_ms.store(0, Ordering::Release);
        let mut w = self
            .window
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        w.iter_mut().for_each(|b| *b = Bucket::default());
    }
}

fn duration_ms(cfg: &BreakerConfig) -> u64 {
    u64::try_from(cfg.open_for.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn cfg() -> BreakerConfig {
        BreakerConfig {
            enabled: true,
            failure_rate: 0.5,
            min_requests: 4,
            window_secs: 10,
            open_for: Duration::from_secs(5),
        }
    }

    #[test]
    fn opens_on_failure_rate_and_probes_after_open_time() {
        let c = cfg();
        let b = Breaker::default();
        let t = 100_000;
        // 3 Erfolge, 2 Fehler: Quote 40 % → bleibt zu.
        for _ in 0..3 {
            b.record(&c, t, false);
        }
        b.record(&c, t, true);
        b.record(&c, t, true);
        assert_eq!(b.state(), BreakerState::Closed);
        // Dritter Fehler: 3/6 = 50 % → offen.
        b.record(&c, t + 1, true);
        assert_eq!(b.state(), BreakerState::Open);
        assert!(!b.available(t + 1000));
        assert_eq!(b.try_admit(&c, t + 1000), Admit::Rejected);
        assert_eq!(b.remaining_ms(t + 1001), 4000);
        // Nach der Offen-Dauer genau eine Probe.
        assert!(b.available(t + 5001));
        assert_eq!(b.try_admit(&c, t + 5001), Admit::Probe);
        assert_eq!(b.state(), BreakerState::HalfOpen);
        assert_eq!(b.try_admit(&c, t + 5002), Admit::Rejected);
        // Probe scheitert → wieder offen.
        b.record(&c, t + 5003, true);
        assert_eq!(b.state(), BreakerState::Open);
        // Nächste Probe gelingt → geschlossen.
        assert_eq!(b.try_admit(&c, t + 10_004), Admit::Probe);
        b.record(&c, t + 10_005, false);
        assert_eq!(b.state(), BreakerState::Closed);
        assert_eq!(b.try_admit(&c, t + 10_006), Admit::Normal);
    }

    #[test]
    fn needs_min_requests_and_forgets_old_buckets() {
        let c = cfg();
        let b = Breaker::default();
        // Zwei Fehler, alte Erfolge außerhalb des Fensters zählen nicht.
        for _ in 0..10 {
            b.record(&c, 1_000, false);
        }
        b.record(&c, 50_000, true);
        b.record(&c, 50_000, true);
        assert_eq!(
            b.state(),
            BreakerState::Closed,
            "Mindestanzahl nicht erreicht"
        );
        b.record(&c, 51_000, true);
        b.record(&c, 52_000, true);
        assert_eq!(b.state(), BreakerState::Open);
    }

    #[test]
    fn lost_probe_is_replaced_and_disabled_breaker_admits() {
        let c = cfg();
        let b = Breaker::default();
        b.open(&c, 0);
        assert_eq!(b.try_admit(&c, 5_000), Admit::Probe);
        // Probe kommt nie zurück → nach weiteren 5 s neue Probe.
        assert_eq!(b.try_admit(&c, 9_999), Admit::Rejected);
        assert_eq!(b.try_admit(&c, 10_000), Admit::Probe);
        assert_eq!(
            b.try_admit(&BreakerConfig::disabled(), 10_001),
            Admit::Normal
        );
    }
}
