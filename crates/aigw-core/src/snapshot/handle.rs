// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Halter des aktuellen Snapshots (§14, AIGW-10).
//!
//! Der Hot Path liest den Snapshot lock-frei über [`SnapshotHandle::load`]; der Reloader
//! (`aigw_store::reload`) ersetzt ihn atomar über [`SnapshotHandle::store`]. Laufende Anfragen
//! behalten den Snapshot, den sie zu Beginn geladen haben (`Arc`), auch wenn währenddessen ein
//! neuer veröffentlicht wird.

use std::sync::Arc;

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
}

impl SnapshotHandle {
    /// Neuer Halter mit `initial` als aktuellem Snapshot.
    pub fn new(initial: Snapshot) -> Self {
        let version = initial.version;
        Self {
            inner: Arc::new(Inner {
                current: ArcSwap::from_pointee(initial),
                version: watch::Sender::new(version),
            }),
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
    pub fn store(&self, snapshot: Snapshot) -> Arc<Snapshot> {
        let version = snapshot.version;
        let old = self.inner.current.swap(Arc::new(snapshot));
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
}
