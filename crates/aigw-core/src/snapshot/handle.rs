// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Halter des aktuellen Snapshots (§14, AIGW-10).
//!
//! Der Hot Path liest den Snapshot lock-frei über [`SnapshotHandle::load`]; der Reloader
//! (`aigw_store::reload`) ersetzt ihn atomar über [`SnapshotHandle::store`]. Laufende Anfragen
//! behalten den Snapshot, den sie zu Beginn geladen haben (`Arc`), auch wenn währenddessen ein
//! neuer veröffentlicht wird.
//!
//! Zusätzlich hält der Halter den Zustand für Readiness und UI (F14/F15, AIGW-75): Quelle des
//! Snapshots (Datenbank oder Platten-Cache), Zeitpunkt des letzten erfolgreichen Abgleichs mit
//! der Datenbank und ob der Abgleich gerade fehlschlägt → [`SnapshotHandle::status`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arc_swap::{ArcSwap, Guard};
use tokio::sync::watch;

use super::Snapshot;

/// Geteilter, billig klonbarer Halter des aktuellen [`Snapshot`].
#[derive(Debug, Clone)]
pub struct SnapshotHandle {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    current: ArcSwap<Snapshot>,
    /// Version des aktuell veröffentlichten Snapshots (für Abonnenten).
    version: watch::Sender<i64>,
    /// [`SnapshotSource`] als Zahl.
    source: AtomicU8,
    /// Letzter erfolgreicher Abgleich mit der Datenbank (Unix-ms; Cache: Schreibzeitpunkt).
    synced_at_ms: AtomicU64,
    /// Schlägt der Abgleich (Versionsprüfung/Neubau) gerade fehl?
    failing: AtomicBool,
    /// Ab diesem Alter gilt ein nicht abgleichbarer Snapshot als veraltet (ms).
    stale_after_ms: AtomicU64,
}

/// Standard für `snapshot.stale_after_secs` (F15: 5 min).
pub const DEFAULT_STALE_AFTER: Duration = Duration::from_secs(300);

/// Herkunft des aktiven Snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SnapshotSource {
    /// Aus der Datenbank gebaut (Normalfall).
    Database,
    /// Beim Kaltstart aus dem verschlüsselten Platten-Cache geladen, Datenbank (noch) nicht
    /// erreichbar (F14).
    Cache,
}

impl SnapshotSource {
    /// `database` | `cache`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Database => "database",
            Self::Cache => "cache",
        }
    }

    fn from_u8(v: u8) -> Self {
        if v == 1 { Self::Cache } else { Self::Database }
    }

    fn to_u8(self) -> u8 {
        match self {
            Self::Database => 0,
            Self::Cache => 1,
        }
    }
}

/// Zustand des aktiven Snapshots für `/readyz` und `GET /admin/api/v1/version` (F15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotStatus {
    /// `config_version` des aktiven Snapshots.
    pub version: i64,
    /// Herkunft.
    pub source: SnapshotSource,
    /// Zeit seit dem letzten erfolgreichen Abgleich mit der Datenbank (bei
    /// [`SnapshotSource::Cache`]: seit dem Schreiben des Caches).
    pub age: Duration,
    /// Schlägt der Abgleich mit der Datenbank gerade fehl?
    pub failing: bool,
    /// Veraltet: aus dem Platten-Cache oder seit mehr als `stale_after` nicht abgleichbar.
    /// Readiness meldet dann `degraded` (HTTP 200).
    pub stale: bool,
}

fn unix_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn now_ms() -> u64 {
    unix_ms(SystemTime::now())
}

impl SnapshotHandle {
    /// Neuer Halter mit `initial` als aktuellem Snapshot.
    pub fn new(initial: Snapshot) -> Self {
        Self::with_source(initial, SnapshotSource::Database, SystemTime::now())
    }

    /// Halter mit einem Snapshot aus dem Platten-Cache (F14), geschrieben um `saved_at`.
    pub fn from_cache(initial: Snapshot, saved_at: SystemTime) -> Self {
        Self::with_source(initial, SnapshotSource::Cache, saved_at)
    }

    fn with_source(initial: Snapshot, source: SnapshotSource, synced_at: SystemTime) -> Self {
        let version = initial.version;
        Self {
            inner: Arc::new(Inner {
                current: ArcSwap::from_pointee(initial),
                version: watch::Sender::new(version),
                source: AtomicU8::new(source.to_u8()),
                synced_at_ms: AtomicU64::new(unix_ms(synced_at)),
                failing: AtomicBool::new(false),
                stale_after_ms: AtomicU64::new(
                    u64::try_from(DEFAULT_STALE_AFTER.as_millis()).unwrap_or(u64::MAX),
                ),
            }),
        }
    }

    /// Setzt `snapshot.stale_after` (Standard [`DEFAULT_STALE_AFTER`]).
    pub fn set_stale_after(&self, after: Duration) {
        self.inner.stale_after_ms.store(
            u64::try_from(after.as_millis()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    /// Herkunft des aktiven Snapshots.
    pub fn source(&self) -> SnapshotSource {
        SnapshotSource::from_u8(self.inner.source.load(Ordering::Relaxed))
    }

    /// Der aktive Snapshot ist mit der Datenbank abgeglichen (Version geprüft, nichts zu tun).
    pub fn mark_synced(&self) {
        self.inner.synced_at_ms.store(now_ms(), Ordering::Relaxed);
        self.inner.failing.store(false, Ordering::Relaxed);
    }

    /// Der Abgleich mit der Datenbank ist fehlgeschlagen (Snapshot bleibt aktiv).
    pub fn mark_failing(&self) {
        self.inner.failing.store(true, Ordering::Relaxed);
    }

    /// Zustand für Readiness und Version-Endpunkt (F15).
    pub fn status(&self) -> SnapshotStatus {
        let source = self.source();
        let synced = self.inner.synced_at_ms.load(Ordering::Relaxed);
        let age = Duration::from_millis(now_ms().saturating_sub(synced));
        let failing = self.inner.failing.load(Ordering::Relaxed);
        let stale_after = Duration::from_millis(self.inner.stale_after_ms.load(Ordering::Relaxed));
        SnapshotStatus {
            version: self.version(),
            source,
            age,
            failing,
            stale: source == SnapshotSource::Cache || (failing && age > stale_after),
        }
    }

    /// Aktueller Snapshot für eine ganze Anfrage (lock-frei, ein atomares Inkrement).
    ///
    /// Das `Arc` darf über `.await` hinweg gehalten werden; die Anfrage sieht bis zum Ende
    /// denselben, unveränderlichen Stand.
    #[inline]
    pub fn load(&self) -> Arc<Snapshot> {
        self.inner.current.load_full()
    }

    /// Noch billigerer Kurzzugriff (im Normalfall ohne Referenzzähler).
    ///
    /// Nur für kurze, synchrone Lesezugriffe gedacht – **nicht** über `.await` halten
    /// (dafür [`SnapshotHandle::load`]).
    #[inline]
    pub fn peek(&self) -> Guard<Arc<Snapshot>> {
        self.inner.current.load()
    }

    /// Version des aktuellen Snapshots.
    #[inline]
    pub fn version(&self) -> i64 {
        self.inner.current.load().version
    }

    /// Veröffentlicht `snapshot` als neuen aktuellen Stand und benachrichtigt Abonnenten.
    ///
    /// Gibt den bisherigen Snapshot zurück (er wird freigegeben, sobald die letzte Anfrage,
    /// die ihn hält, fertig ist).
    ///
    /// Der neue Snapshot gilt als aus der Datenbank gebaut und abgeglichen.
    pub fn store(&self, snapshot: Snapshot) -> Arc<Snapshot> {
        let version = snapshot.version;
        let old = self.inner.current.swap(Arc::new(snapshot));
        self.inner
            .source
            .store(SnapshotSource::Database.to_u8(), Ordering::Relaxed);
        self.mark_synced();
        self.inner.version.send_replace(version);
        old
    }

    /// Abonnement auf Versionswechsel (`changed().await`, danach `borrow()` = neue Version).
    pub fn subscribe(&self) -> watch::Receiver<i64> {
        self.inner.version.subscribe()
    }
}

impl Default for SnapshotHandle {
    fn default() -> Self {
        Self::new(Snapshot::default())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn snap(version: i64) -> Snapshot {
        Snapshot {
            version,
            ..Snapshot::default()
        }
    }

    #[test]
    fn store_replaces_and_old_readers_keep_their_snapshot() {
        let h = SnapshotHandle::new(snap(1));
        let held = h.load();
        let mut rx = h.subscribe();
        assert_eq!(*rx.borrow(), 1);

        let old = h.store(snap(2));
        assert_eq!(old.version, 1);
        assert_eq!(held.version, 1, "laufende Anfrage behält ihren Snapshot");
        assert_eq!(h.load().version, 2);
        assert_eq!(h.peek().version, 2);
        assert_eq!(h.version(), 2);
        assert!(rx.has_changed().unwrap());
        assert_eq!(*rx.borrow_and_update(), 2);

        let clone = h.clone();
        clone.store(snap(3));
        assert_eq!(h.version(), 3, "Klone teilen denselben Stand");
    }

    #[test]
    fn status_cache_failing_and_stale() {
        let h = SnapshotHandle::new(snap(1));
        let st = h.status();
        assert_eq!(st.source, SnapshotSource::Database);
        assert!(!st.stale && !st.failing);
        assert!(st.age < Duration::from_secs(5));

        // Fehlschlag allein ist noch nicht „veraltet“ – erst nach stale_after.
        h.set_stale_after(Duration::from_millis(30));
        h.mark_failing();
        assert!(h.status().failing);
        assert!(!h.status().stale);
        std::thread::sleep(Duration::from_millis(60));
        let st = h.status();
        assert!(st.stale, "nach stale_after veraltet: {st:?}");
        assert!(st.age >= Duration::from_millis(60));
        h.mark_synced();
        assert!(!h.status().stale, "Abgleich wieder erfolgreich");

        // Aus dem Platten-Cache: immer veraltet, bis ein Snapshot aus der DB veröffentlicht ist.
        let saved = SystemTime::now() - Duration::from_secs(120);
        let c = SnapshotHandle::from_cache(snap(7), saved);
        let st = c.status();
        assert_eq!(st.source, SnapshotSource::Cache);
        assert!(st.stale);
        assert!(st.age >= Duration::from_secs(119));
        c.mark_synced();
        assert!(
            c.status().stale,
            "Versionsprüfung allein ersetzt den Cache-Stand nicht"
        );
        c.store(snap(8));
        let st = c.status();
        assert_eq!(
            (st.source, st.stale, st.version),
            (SnapshotSource::Database, false, 8)
        );
    }
}
