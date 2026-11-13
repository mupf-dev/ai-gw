// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Prozesslokale Sliding-Window-Engine (Grundlage von `local`, `postgres-sync` und dem
//! Rückfall von `redis`).
//!
//! Aufbau: je Fenster eine geshardete [`DashMap`] `Schlüssel → Arc<Mutex<Slot>>`. Ein Slot
//! hält den laufenden und den vorigen Bucket. Kritische Abschnitte sind wenige Additionen;
//! Konkurrenz entsteht nur bei gleichem Schlüssel.
//!
//! **Sperrregel:** Wer einen Slot-Mutex hält, greift nicht auf die Maps zu. Mehrere Slots
//! werden in Adressreihenfolge gesperrt (deadlock-frei, Duplikate werden erkannt).

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use dashmap::DashMap;

use crate::window::{Window, estimate, evaluate, reset_after_ms, retry_after_ms};
use crate::{Booked, Clock, LimitCheck, Origin, Quota, Rejection, Reservation, SettleMode};

/// Stand eines Buckets.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Bucket {
    /// Von dieser Instanz gebucht.
    pub(crate) local: u64,
    /// Summe der anderen Instanzen laut letztem Abgleich (nur `postgres-sync`).
    pub(crate) remote: u64,
    /// Noch nicht abgeglichene lokale Änderung (nur `postgres-sync`).
    pub(crate) unsynced: i64,
}

impl Bucket {
    fn total(&self) -> u64 {
        self.local.saturating_add(self.remote)
    }

    fn is_empty(&self) -> bool {
        self.local == 0 && self.remote == 0 && self.unsynced == 0
    }
}

/// Zwei Buckets eines Zählers: `cur` gehört zu `idx`, `prev` zu `idx − 1`.
#[derive(Debug)]
pub(crate) struct Slot {
    idx: u64,
    cur: Bucket,
    prev: Bucket,
}

impl Slot {
    fn new(idx: u64) -> Self {
        Self {
            idx,
            cur: Bucket::default(),
            prev: Bucket::default(),
        }
    }

    /// Rückt auf Bucket `now_idx` vor. Herausfallende, noch nicht abgeglichene Änderungen
    /// werden an `spill` übergeben. Läuft die Uhr rückwärts, bleibt der Slot unverändert.
    fn roll(&mut self, now_idx: u64, mut spill: impl FnMut(u64, i64)) {
        if now_idx <= self.idx {
            return;
        }
        if now_idx == self.idx + 1 {
            if self.prev.unsynced != 0 {
                spill(self.idx.wrapping_sub(1), self.prev.unsynced);
            }
            self.prev = self.cur;
        } else {
            if self.prev.unsynced != 0 {
                spill(self.idx.wrapping_sub(1), self.prev.unsynced);
            }
            if self.cur.unsynced != 0 {
                spill(self.idx, self.cur.unsynced);
            }
            self.prev = Bucket::default();
        }
        self.cur = Bucket::default();
        self.idx = now_idx;
    }

    fn bucket_mut(&mut self, idx: u64) -> Option<&mut Bucket> {
        if idx == self.idx {
            Some(&mut self.cur)
        } else if idx + 1 == self.idx {
            Some(&mut self.prev)
        } else {
            None
        }
    }

    fn is_empty(&self) -> bool {
        self.cur.is_empty() && self.prev.is_empty()
    }
}

/// Noch nicht abgeglichene Änderung eines Buckets (für `postgres-sync`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SyncDelta {
    pub(crate) key: Box<str>,
    pub(crate) window: Window,
    pub(crate) bucket: u64,
    pub(crate) delta: i64,
}

/// Ein Bucket, dessen Fremdanteil beim Abgleich gelesen werden soll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Wanted {
    pub(crate) key: Box<str>,
    pub(crate) window: Window,
    pub(crate) bucket: u64,
}

type SlotRef = Arc<Mutex<Slot>>;

fn lock(slot: &SlotRef) -> MutexGuard<'_, Slot> {
    // Ein Panic im kritischen Abschnitt (nur Arithmetik) hinterlässt keinen inkonsistenten
    // Zustand, daher wird die Vergiftung ignoriert.
    slot.lock().unwrap_or_else(PoisonError::into_inner)
}

fn apply_delta(b: &mut Bucket, delta: i128, track: bool) {
    if delta >= 0 {
        let d = u64::try_from(delta).unwrap_or(u64::MAX);
        b.local = b.local.saturating_add(d);
        if track {
            b.unsynced = b
                .unsynced
                .saturating_add(i64::try_from(d).unwrap_or(i64::MAX));
        }
    } else {
        let want = u64::try_from(-delta).unwrap_or(u64::MAX);
        let dec = want.min(b.local);
        b.local -= dec;
        if track {
            b.unsynced = b
                .unsynced
                .saturating_sub(i64::try_from(dec).unwrap_or(i64::MAX));
        }
    }
}

/// Sliding-Window-Zähler im Prozess.
#[derive(Debug)]
pub(crate) struct Engine {
    maps: [DashMap<Box<str>, SlotRef>; 5],
    clock: Arc<dyn Clock>,
    track_sync: bool,
    spill: Mutex<Vec<SyncDelta>>,
}

impl Engine {
    /// `track_sync`: Änderungen für den Abgleich mit Postgres mitschreiben.
    pub(crate) fn new(clock: Arc<dyn Clock>, track_sync: bool) -> Self {
        Self {
            maps: std::array::from_fn(|_| DashMap::new()),
            clock,
            track_sync,
            spill: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    /// Anzahl gehaltener Zähler (alle Fenster).
    pub(crate) fn len(&self) -> usize {
        self.maps.iter().map(DashMap::len).sum()
    }

    fn slot(&self, key: &str, window: Window, now: u64) -> SlotRef {
        let map = &self.maps[window.index()];
        if let Some(s) = map.get(key) {
            return Arc::clone(s.value());
        }
        Arc::clone(
            map.entry(key.into())
                .or_insert_with(|| Arc::new(Mutex::new(Slot::new(window.bucket(now)))))
                .value(),
        )
    }

    fn existing(&self, key: &str, window: Window) -> Option<SlotRef> {
        self.maps[window.index()]
            .get(key)
            .map(|s| Arc::clone(s.value()))
    }

    fn roll(&self, slot: &mut Slot, key: &str, window: Window, now_idx: u64) {
        if self.track_sync {
            slot.roll(now_idx, |bucket, delta| {
                self.spill
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(SyncDelta {
                        key: key.into(),
                        window,
                        bucket,
                        delta,
                    });
            });
        } else {
            slot.roll(now_idx, |_, _| {});
        }
    }

    /// Prüft und bucht alle `checks` atomar (alle oder keiner).
    pub(crate) fn reserve(&self, checks: &[LimitCheck<'_>]) -> Result<Reservation, Rejection> {
        if checks.is_empty() {
            return Ok(Reservation::empty(Origin::Local));
        }
        let now = self.clock.now_ms();
        let n = checks.len();
        let slots: Vec<SlotRef> = checks
            .iter()
            .map(|c| self.slot(c.key, c.window, now))
            .collect();

        // In Adressreihenfolge sperren; Duplikate teilen sich die Sperre des ersten.
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_unstable_by_key(|&i| Arc::as_ptr(&slots[i]).addr());
        let mut guards: Vec<Option<MutexGuard<'_, Slot>>> = (0..n).map(|_| None).collect();
        let mut owner = vec![0usize; n];
        let mut last: Option<usize> = None;
        for &i in &order {
            match last {
                Some(j) if Arc::ptr_eq(&slots[i], &slots[j]) => owner[i] = j,
                _ => {
                    let mut g = lock(&slots[i]);
                    let c = &checks[i];
                    self.roll(&mut g, c.key, c.window, c.window.bucket(now));
                    guards[i] = Some(g);
                    owner[i] = i;
                    last = Some(i);
                }
            }
        }

        // Prüfen (ohne zu buchen).
        let mut pending = vec![0u64; n];
        let mut stats = Vec::with_capacity(n);
        // (Index, Wartezeit); `None` = passt nie und dominiert.
        let mut worst: Option<(usize, Option<u64>, u64)> = None;
        for (i, c) in checks.iter().enumerate() {
            let o = owner[i];
            let Some(g) = guards[o].as_ref() else {
                continue; // unerreichbar: Eigentümer hält immer eine Sperre
            };
            let len = c.window.length_ms();
            let elapsed = now.saturating_sub(g.idx * len);
            let cur = g.cur.total().saturating_add(pending[o]);
            let prev = g.prev.total();
            let ev = evaluate(cur, prev, elapsed, len, c.limit, c.amount);
            if !ev.admit {
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
            pending[o] = pending[o].saturating_add(c.amount);
            stats.push((g.idx, cur, prev, elapsed, ev.estimate));
        }

        if let Some((i, ra, used)) = worst {
            let c = &checks[i];
            return Err(Rejection {
                index: i,
                key: c.key.to_owned(),
                window: c.window,
                limit: c.limit,
                used,
                retry_after: ra.map(std::time::Duration::from_millis),
            });
        }

        // Buchen.
        for (i, g) in guards.iter_mut().enumerate() {
            if let Some(g) = g.as_mut() {
                apply_delta(&mut g.cur, i128::from(pending[i]), self.track_sync);
            }
        }
        drop(guards);

        let mut booked = Vec::with_capacity(n);
        let mut quotas = Vec::with_capacity(n);
        for (c, &(idx, cur, prev, elapsed, est)) in checks.iter().zip(&stats) {
            let len = c.window.length_ms();
            booked.push(Booked {
                key: c.key.into(),
                window: c.window,
                bucket: idx,
                amount: c.amount,
                settle: c.settle,
            });
            quotas.push(Quota {
                limit: c.limit,
                remaining: c.limit.saturating_sub(est.saturating_add(c.amount)),
                reset_after: std::time::Duration::from_millis(reset_after_ms(
                    cur.saturating_add(c.amount),
                    prev,
                    elapsed,
                    len,
                )),
            });
        }
        Ok(Reservation {
            booked,
            quotas,
            origin: Origin::Local,
        })
    }

    /// Verrechnet `actual − amount` für jede Buchung mit [`SettleMode::Adjust`].
    pub(crate) fn settle(&self, booked: &[Booked], actual: u64) {
        let now = self.clock.now_ms();
        for b in booked {
            if b.settle == SettleMode::Keep {
                continue;
            }
            let delta = i128::from(actual) - i128::from(b.amount);
            if delta == 0 {
                continue;
            }
            let now_idx = b.window.bucket(now);
            if b.bucket + 1 < now_idx {
                continue; // Bucket zählt nicht mehr zum Fenster
            }
            let slot = if delta > 0 {
                self.slot(&b.key, b.window, now)
            } else {
                match self.existing(&b.key, b.window) {
                    Some(s) => s,
                    None => continue,
                }
            };
            let mut g = lock(&slot);
            self.roll(&mut g, &b.key, b.window, now_idx);
            if let Some(bucket) = g.bucket_mut(b.bucket) {
                apply_delta(bucket, delta, self.track_sync);
            }
        }
    }

    /// Geschätzter Verbrauch im gleitenden Fenster.
    pub(crate) fn get(&self, key: &str, window: Window) -> u64 {
        let Some(slot) = self.existing(key, window) else {
            return 0;
        };
        let now = self.clock.now_ms();
        let len = window.length_ms();
        let mut g = lock(&slot);
        self.roll(&mut g, key, window, window.bucket(now));
        estimate(
            g.cur.total(),
            g.prev.total(),
            now.saturating_sub(g.idx * len),
            len,
        )
    }

    /// Entfernt Zähler, deren beide Buckets abgelaufen bzw. leer sind. Zähler, die gerade
    /// in Benutzung sind (weitere `Arc`-Referenz), bleiben erhalten.
    pub(crate) fn purge_expired(&self) -> usize {
        let now = self.clock.now_ms();
        let mut removed = 0;
        for w in Window::ALL {
            let now_idx = w.bucket(now);
            self.maps[w.index()].retain(|key, slot| {
                if Arc::strong_count(slot) > 1 {
                    return true;
                }
                let Ok(mut g) = slot.try_lock() else {
                    return true;
                };
                self.roll(&mut g, key, w, now_idx);
                let keep = !g.is_empty();
                removed += usize::from(!keep);
                keep
            });
        }
        removed
    }

    /// Entnimmt alle nicht abgeglichenen Änderungen und listet die Buckets, deren
    /// Fremdanteil gelesen werden soll.
    pub(crate) fn collect_sync(&self) -> (Vec<SyncDelta>, Vec<Wanted>) {
        let now = self.clock.now_ms();
        let mut deltas =
            std::mem::take(&mut *self.spill.lock().unwrap_or_else(PoisonError::into_inner));
        let mut wanted = Vec::new();
        for w in Window::ALL {
            let now_idx = w.bucket(now);
            let entries: Vec<(Box<str>, SlotRef)> = self.maps[w.index()]
                .iter()
                .map(|e| (e.key().clone(), Arc::clone(e.value())))
                .collect();
            for (key, slot) in entries {
                let mut g = lock(&slot);
                self.roll(&mut g, &key, w, now_idx);
                let idx = g.idx;
                let slot = &mut *g;
                for (bucket, b) in [(idx, &mut slot.cur), (idx.wrapping_sub(1), &mut slot.prev)] {
                    if b.unsynced != 0 {
                        deltas.push(SyncDelta {
                            key: key.clone(),
                            window: w,
                            bucket,
                            delta: std::mem::take(&mut b.unsynced),
                        });
                    }
                }
                drop(g);
                wanted.push(Wanted {
                    key: key.clone(),
                    window: w,
                    bucket: idx,
                });
                wanted.push(Wanted {
                    key,
                    window: w,
                    bucket: idx.wrapping_sub(1),
                });
            }
        }
        // Die Uhr kann zwischen dem Leeren von `spill` und den Rolls weitergelaufen sein.
        deltas.append(&mut self.spill.lock().unwrap_or_else(PoisonError::into_inner));
        (deltas, wanted)
    }

    /// Setzt den Fremdanteil eines Buckets (falls der Zähler noch existiert).
    pub(crate) fn set_remote(&self, key: &str, window: Window, bucket: u64, others: u64) {
        let Some(slot) = self.existing(key, window) else {
            return;
        };
        let now_idx = window.bucket(self.clock.now_ms());
        let mut g = lock(&slot);
        self.roll(&mut g, key, window, now_idx);
        if let Some(b) = g.bucket_mut(bucket) {
            b.remote = others;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, unused_must_use)]

    use std::time::Duration;

    use super::*;
    use crate::ManualClock;

    const MIN: u64 = 60_000;
    const T0: u64 = 1_000 * MIN; // Bucket-Grenze für Minute/Sekunde

    fn engine(track: bool) -> (Arc<ManualClock>, Engine) {
        let clock = Arc::new(ManualClock::new(T0));
        (clock.clone(), Engine::new(clock, track))
    }

    #[test]
    fn reserve_until_limit_then_reject() {
        let (_c, e) = engine(false);
        let chk = [LimitCheck::new("k", Window::Minute, 100, 40)];
        let r1 = e.reserve(&chk).unwrap();
        assert_eq!(r1.quotas()[0].remaining, 60);
        let r2 = e.reserve(&chk).unwrap();
        assert_eq!(r2.quotas()[0].remaining, 20);
        let rej = e.reserve(&chk).unwrap_err();
        assert_eq!(rej.index, 0);
        assert_eq!(rej.used, 80);
        assert_eq!(rej.remaining(), 20);
        // cur=80 > need=60 → erst im nächsten Bucket: 80·(1 − t/L) ≤ 60 → t = L/4.
        assert_eq!(rej.retry_after, Some(Duration::from_millis(MIN + MIN / 4)));
        assert_eq!(e.get("k", Window::Minute), 80);
    }

    #[test]
    fn amount_above_limit_never_fits() {
        let (_c, e) = engine(false);
        let rej = e
            .reserve(&[LimitCheck::new("k", Window::Minute, 10, 11)])
            .unwrap_err();
        assert_eq!(rej.retry_after, None);
        assert_eq!(e.get("k", Window::Minute), 0);
    }

    #[test]
    fn sliding_window_weights_previous_bucket() {
        let (c, e) = engine(false);
        let r = e
            .reserve(&[LimitCheck::new("k", Window::Minute, 100, 100)])
            .unwrap();
        e.settle(&r.booked, 100);
        c.advance(MIN + MIN / 4); // 75 % des vorigen Buckets zählen
        assert_eq!(e.get("k", Window::Minute), 75);
        assert!(
            e.reserve(&[LimitCheck::new("k", Window::Minute, 100, 26)])
                .is_err()
        );
        assert!(
            e.reserve(&[LimitCheck::new("k", Window::Minute, 100, 25)])
                .is_ok()
        );
        c.advance(MIN); // vorig = 25, nichts Neues
        assert_eq!(e.get("k", Window::Minute), 25 * 3 / 4);
        c.advance(2 * MIN);
        assert_eq!(e.get("k", Window::Minute), 0);
    }

    #[test]
    fn multi_key_all_or_nothing() {
        let (_c, e) = engine(false);
        e.reserve(&[LimitCheck::new("team", Window::Minute, 100, 95)])
            .unwrap();
        let checks = [
            LimitCheck::new("key", Window::Minute, 1_000, 10),
            LimitCheck::new("team", Window::Minute, 100, 10),
            LimitCheck::fixed("org", Window::Hour, 5, 1),
        ];
        let rej = e.reserve(&checks).unwrap_err();
        assert_eq!(rej.index, 1);
        assert_eq!(rej.key, "team");
        assert_eq!(e.get("key", Window::Minute), 0);
        assert_eq!(e.get("org", Window::Hour), 0);
        assert_eq!(e.get("team", Window::Minute), 95);

        let ok = e
            .reserve(&[
                LimitCheck::new("key", Window::Minute, 1_000, 5),
                LimitCheck::new("team", Window::Minute, 100, 5),
                LimitCheck::fixed("org", Window::Hour, 5, 1),
            ])
            .unwrap();
        assert_eq!(ok.quotas().len(), 3);
        assert_eq!(ok.tightest().unwrap().0, 1);
        assert_eq!(ok.quotas()[1].remaining, 0);
        assert_eq!(ok.quotas()[2].remaining, 4);
    }

    #[test]
    fn rejection_reports_longest_wait() {
        let (_c, e) = engine(false);
        e.reserve(&[LimitCheck::new("a", Window::Second, 10, 10)])
            .unwrap();
        e.reserve(&[LimitCheck::new("b", Window::Hour, 10, 10)])
            .unwrap();
        let rej = e
            .reserve(&[
                LimitCheck::new("a", Window::Second, 10, 1),
                LimitCheck::new("b", Window::Hour, 10, 1),
            ])
            .unwrap_err();
        assert_eq!(rej.key, "b");
        let rej = e
            .reserve(&[
                LimitCheck::new("a", Window::Second, 10, 1),
                LimitCheck::new("c", Window::Second, 1, 2),
                LimitCheck::new("b", Window::Hour, 10, 1),
            ])
            .unwrap_err();
        assert_eq!(rej.key, "c");
        assert_eq!(rej.retry_after, None);
    }

    #[test]
    fn duplicate_keys_count_cumulatively() {
        let (_c, e) = engine(false);
        let dup = [
            LimitCheck::new("k", Window::Minute, 10, 6),
            LimitCheck::new("k", Window::Minute, 10, 6),
        ];
        assert!(e.reserve(&dup).is_err());
        assert_eq!(e.get("k", Window::Minute), 0);
        let dup = [
            LimitCheck::new("k", Window::Minute, 10, 5),
            LimitCheck::new("k", Window::Minute, 10, 5),
        ];
        e.reserve(&dup).unwrap();
        assert_eq!(e.get("k", Window::Minute), 10);
        // gleicher Schlüssel, anderes Fenster = anderer Zähler
        e.reserve(&[LimitCheck::new("k", Window::Hour, 10, 10)])
            .unwrap();
    }

    #[test]
    fn settle_adjusts_both_directions_and_keeps_fixed() {
        let (_c, e) = engine(false);
        let r = e
            .reserve(&[
                LimitCheck::new("tok", Window::Minute, 1_000, 500),
                LimitCheck::fixed("req", Window::Minute, 10, 1),
            ])
            .unwrap();
        e.settle(&r.booked, 120);
        assert_eq!(e.get("tok", Window::Minute), 120);
        assert_eq!(e.get("req", Window::Minute), 1);
        let r = e
            .reserve(&[LimitCheck::new("tok", Window::Minute, 1_000, 100)])
            .unwrap();
        e.settle(&r.booked, 300); // mehr verbraucht als reserviert
        assert_eq!(e.get("tok", Window::Minute), 420);
    }

    #[test]
    fn settle_hits_original_bucket_or_nothing() {
        let (c, e) = engine(false);
        let r = e
            .reserve(&[LimitCheck::new("k", Window::Minute, 1_000, 500)])
            .unwrap();
        c.advance(MIN + MIN / 2); // Buchung liegt nun im vorigen Bucket (50 % Gewicht)
        assert_eq!(e.get("k", Window::Minute), 250);
        e.settle(&r.booked, 100);
        assert_eq!(e.get("k", Window::Minute), 50);

        let r = e
            .reserve(&[LimitCheck::new("x", Window::Minute, 1_000, 500)])
            .unwrap();
        c.advance(2 * MIN);
        e.settle(&r.booked, 900); // Bucket abgelaufen: ignoriert
        assert_eq!(e.get("x", Window::Minute), 0);
    }

    #[test]
    fn purge_removes_expired_only() {
        let (c, e) = engine(false);
        e.reserve(&[LimitCheck::new("a", Window::Second, 10, 1)])
            .unwrap();
        e.reserve(&[LimitCheck::new("b", Window::Hour, 10, 1)])
            .unwrap();
        assert_eq!(e.len(), 2);
        assert_eq!(e.purge_expired(), 0);
        c.advance(2_000);
        let held = e.slot("a", Window::Second, c.now_ms());
        assert_eq!(e.purge_expired(), 0, "gehaltener Slot bleibt");
        drop(held);
        assert_eq!(e.purge_expired(), 1);
        assert_eq!(e.len(), 1);
        assert_eq!(e.get("b", Window::Hour), 1);
    }

    #[test]
    fn sync_tracking_collects_deltas_and_spills_on_roll() {
        let (c, e) = engine(true);
        let r = e
            .reserve(&[LimitCheck::new("k", Window::Second, 1_000, 50)])
            .unwrap();
        e.settle(&r.booked, 30);
        let (d, w) = e.collect_sync();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].delta, 30);
        assert_eq!(w.len(), 2);
        assert!(e.collect_sync().0.is_empty());

        e.reserve(&[LimitCheck::new("k", Window::Second, 1_000, 7)])
            .unwrap();
        c.advance(5_000); // beide Buckets fallen heraus → spill
        let (d, _) = e.collect_sync();
        assert_eq!(d.iter().map(|d| d.delta).sum::<i64>(), 7);

        // Fremdanteil zählt mit
        e.set_remote("k", Window::Second, Window::Second.bucket(c.now_ms()), 990);
        assert_eq!(e.get("k", Window::Second), 990);
        assert!(
            e.reserve(&[LimitCheck::new("k", Window::Second, 1_000, 11)])
                .is_err()
        );
    }

    #[test]
    fn concurrent_reservations_never_exceed_limit() {
        let (_c, e) = engine(false);
        let e = Arc::new(e);
        let granted = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let threads: Vec<_> = (0..16)
            .map(|t| {
                let e = Arc::clone(&e);
                let granted = Arc::clone(&granted);
                std::thread::spawn(move || {
                    for i in 0..2_000u64 {
                        let own = format!("key{}", (t + i) % 4);
                        let checks = [
                            LimitCheck::new(&own, Window::Minute, 5_000, 3),
                            LimitCheck::new("team", Window::Minute, 10_000, 3),
                        ];
                        if e.reserve(&checks).is_ok() {
                            granted.fetch_add(3, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let team = e.get("team", Window::Minute);
        assert_eq!(team, granted.load(std::sync::atomic::Ordering::Relaxed));
        assert!(team <= 10_000);
        assert!(team > 10_000 - 3, "Limit wird ausgeschöpft: {team}");
        let keys: u64 = (0..4)
            .map(|i| e.get(&format!("key{i}"), Window::Minute))
            .sum();
        assert_eq!(keys, team);
    }
}
