// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Laufzeitzustand je Deployment: offene Anfragen, EWMA von TTFT und Latenz, Circuit Breaker,
//! Sperre nach `Retry-After` und Sperre wegen erschöpfter Kapazität.
//!
//! Der Zustand hängt an der Deployment-ID und überlebt Snapshot-Wechsel, solange sich das
//! Deployment nicht wesentlich ändert (Anbieter, Endpunkt, Anbieter-Modell, Zugangsdaten-Name,
//! Einstellungen; siehe [`fingerprint`]). Gewicht, Priorität und Kapazität dürfen sich ändern,
//! ohne dass EWMA und Breaker verloren gehen.
//!
//! Die Kapazität (`capacity.rpm`/`tpm`) zählt `aigw-counters` (dasselbe Backend wie die
//! Limits, also je nach Konfiguration instanzübergreifend, siehe `exec`). Lehnt der Zähler
//! ab, merkt sich der Zustand die Wartezeit (`capacity_until_ms`), damit die Auswahl solange
//! ohne Zählerzugriff (nur Atomics) an dem Deployment vorbeigeht.

use std::hash::{Hash as _, Hasher as _};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use aigw_core::snapshot::DeploymentCtx;

use super::breaker::{Breaker, BreakerState};

/// Millisekunden seit dem ersten Aufruf (monotone Prozessuhr für Atomics).
pub(crate) fn now_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    let epoch = EPOCH.get_or_init(Instant::now);
    u64::try_from(epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
}

pub(crate) fn duration_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Exponentiell gleitender Mittelwert in Nanosekunden (`0` = noch kein Messwert).
#[derive(Debug, Default)]
pub(crate) struct Ewma(AtomicU64);

/// Gewicht eines neuen Messwerts (in Zehnteln).
const EWMA_ALPHA_TENTHS: i128 = 3;

impl Ewma {
    pub(crate) fn get(&self) -> Option<u64> {
        match self.0.load(Ordering::Relaxed) {
            0 => None,
            v => Some(v),
        }
    }

    pub(crate) fn update(&self, sample: Duration) {
        let sample = u64::try_from(sample.as_nanos()).unwrap_or(u64::MAX).max(1);
        let mut old = self.0.load(Ordering::Relaxed);
        loop {
            let new = if old == 0 {
                sample
            } else {
                let o = i128::from(old);
                let v = o + (i128::from(sample) - o) * EWMA_ALPHA_TENTHS / 10;
                u64::try_from(v).unwrap_or(sample).max(1)
            };
            match self
                .0
                .compare_exchange_weak(old, new, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(current) => old = current,
            }
        }
    }
}

/// Zustand eines Deployments.
#[derive(Debug, Default)]
pub struct DeploymentState {
    pub(crate) fingerprint: u64,
    pub(crate) in_flight: AtomicU32,
    pub(crate) ttft: Ewma,
    pub(crate) latency: Ewma,
    pub(crate) breaker: Breaker,
    /// Gesperrt bis (ms, `Retry-After` des Anbieters).
    pub(crate) cooldown_until_ms: AtomicU64,
    /// Kapazität erschöpft bis (ms, Wartezeit laut Zähler).
    pub(crate) capacity_until_ms: AtomicU64,
    /// Letzter Health Check (ms).
    pub(crate) last_health_ms: AtomicU64,
    /// Aufeinanderfolgende fehlgeschlagene Health Checks.
    pub(crate) health_failures: AtomicU32,
}

impl DeploymentState {
    pub(crate) fn new(fingerprint: u64) -> Self {
        Self {
            fingerprint,
            ..Self::default()
        }
    }

    /// Offene Anfragen.
    pub fn in_flight(&self) -> u32 {
        self.in_flight.load(Ordering::Relaxed)
    }

    /// EWMA der Zeit bis zum ersten Ereignis (Streams).
    pub fn ewma_ttft(&self) -> Option<Duration> {
        self.ttft.get().map(Duration::from_nanos)
    }

    /// EWMA der Antwortzeit (nicht gestreamt).
    pub fn ewma_latency(&self) -> Option<Duration> {
        self.latency.get().map(Duration::from_nanos)
    }

    /// Aufeinanderfolgende fehlgeschlagene Health Checks.
    pub fn health_failures(&self) -> u32 {
        self.health_failures.load(Ordering::Relaxed)
    }

    /// Zustand des Circuit Breakers.
    pub fn breaker_state(&self) -> BreakerState {
        self.breaker.state()
    }

    /// Restliche Sperre wegen erschöpfter Kapazität (`None` = verfügbar).
    pub fn capacity_blocked_for(&self) -> Option<Duration> {
        match self.capacity_remaining_ms(now_ms()) {
            0 => None,
            ms => Some(Duration::from_millis(ms)),
        }
    }

    pub(crate) fn capacity_remaining_ms(&self, now_ms: u64) -> u64 {
        self.capacity_until_ms
            .load(Ordering::Acquire)
            .saturating_sub(now_ms)
    }

    pub(crate) fn block_capacity(&self, now_ms: u64, d: Duration) {
        let until = now_ms.saturating_add(duration_ms(d).max(1));
        self.capacity_until_ms.fetch_max(until, Ordering::AcqRel);
    }

    pub(crate) fn cooldown_remaining_ms(&self, now_ms: u64) -> u64 {
        self.cooldown_until_ms
            .load(Ordering::Acquire)
            .saturating_sub(now_ms)
    }

    pub(crate) fn set_cooldown(&self, now_ms: u64, d: Duration) {
        let until = now_ms.saturating_add(duration_ms(d));
        self.cooldown_until_ms.fetch_max(until, Ordering::AcqRel);
    }
}

/// Fingerabdruck der Eigenschaften, deren Änderung den Laufzeitzustand ungültig macht.
pub(crate) fn fingerprint(dep: &DeploymentCtx) -> u64 {
    let spec = &dep.deployment.spec;
    let mut h = std::hash::DefaultHasher::new();
    spec.provider.as_str().hash(&mut h);
    spec.endpoint.hash(&mut h);
    spec.provider_model.hash(&mut h);
    spec.credential.hash(&mut h);
    serde_json::to_string(&spec.settings)
        .unwrap_or_default()
        .hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ewma_moves_towards_samples() {
        let e = Ewma::default();
        assert_eq!(e.get(), None);
        e.update(Duration::from_millis(100));
        assert_eq!(e.get(), Some(100_000_000));
        e.update(Duration::from_millis(200));
        assert_eq!(e.get(), Some(130_000_000));
    }

    #[test]
    fn capacity_block() {
        let s = DeploymentState::default();
        assert_eq!(s.capacity_remaining_ms(1_000), 0);
        s.block_capacity(1_000, Duration::from_millis(500));
        assert_eq!(s.capacity_remaining_ms(1_200), 300);
        // Kürzere Sperre verkürzt nicht.
        s.block_capacity(1_200, Duration::from_millis(10));
        assert_eq!(s.capacity_remaining_ms(1_200), 300);
        assert_eq!(s.capacity_remaining_ms(2_000), 0);
    }
}
