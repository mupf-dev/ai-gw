// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Hot Reload des Konfigurations-Snapshots (§14, AIGW-10).
//!
//! Der [`Reloader`] baut den Snapshot neu, sobald sich `config_version` geändert hat, und
//! veröffentlicht ihn über [`SnapshotHandle`]. Auslöser:
//!
//! 1. **Commit im eigenen Prozess** (SQLite und Postgres): Das Repository meldet jede neue
//!    Version über [`Store::subscribe_commits`].
//! 2. **Postgres `LISTEN aigw_config`**: Jeder Commit sendet in seiner Transaktion
//!    `NOTIFY aigw_config, '<version>'`; alle Instanzen lauschen auf einer eigenen Verbindung.
//! 3. **Versionsabfrage** alle `store.reload_poll_secs` (Standard 30 s) als Absicherung gegen
//!    verlorene Benachrichtigungen (z. B. während eines Verbindungsabbruchs).
//!
//! Mehrere Auslöser kurz hintereinander werden zusammengefasst (Debounce, Standard 100 ms).
//! Der Neubau ist inkrementell über das Änderungsjournal ([`load_incremental`], F16); bei
//! Lücken, zu vielen Änderungen usw. vollständig ([`load_snapshot`]). Eine periodische
//! Konsistenzprüfung baut vollständig neu und vergleicht mit dem aktiven Snapshot
//! ([`Reloader::verify_now`]). Schlägt der Neubau fehl – oder ist die Datenbank nicht
//! erreichbar –, bleibt der letzte Snapshot aktiv (Q8); erneut versucht wird mit
//! exponentiellem Backoff. Zähler für Metriken stehen in [`ReloadMetrics`].
//!
//! Mit Platten-Cache (F14) schreibt ein eigener Task jeden aus der Datenbank gebauten Snapshot
//! verschlüsselt auf Platte (höchstens alle `cache_write_interval`). Startet der Server aus dem
//! Cache ([`Reloader::from_cache`]), migriert der Reloader die Datenbank, sobald sie erreichbar
//! ist, und ersetzt den Cache-Stand durch einen vollständigen Neubau.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use aigw_core::config::{SnapshotConfig, StoreConfig};
use aigw_core::snapshot::{Snapshot, SnapshotHandle, SnapshotSource};
use sqlx::postgres::PgListener;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};

use crate::snapshot::{Incremental, load_incremental};
use crate::snapshot_cache::{CachedSnapshot, SnapshotCache};
use crate::{Backend, Keyring, Store, StoreError, load_snapshot};

/// Postgres-Kanal für Änderungsbenachrichtigungen (Nutzlast: neue `config_version`).
pub const NOTIFY_CHANNEL: &str = "aigw_config";

/// Einstellungen des Reloaders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReloadOptions {
    /// Abstand der Absicherungs-Abfrage von `config_version` (`None` = aus).
    pub poll_interval: Option<Duration>,
    /// Wartezeit nach dem ersten Auslöser, um weitere Änderungen zusammenzufassen.
    pub debounce: Duration,
    /// Erste Wartezeit nach einem Fehler (verdoppelt sich bis `backoff_max`).
    pub backoff_min: Duration,
    /// Höchste Wartezeit zwischen zwei Versuchen nach Fehlern.
    pub backoff_max: Duration,
    /// Inkrementeller Neubau (F16).
    pub incremental: bool,
    /// Mehr Journal-Einträge → vollständiger Neubau.
    pub max_incremental_changes: usize,
    /// Abstand der Konsistenzprüfung (`None` = aus).
    pub verify_interval: Option<Duration>,
    /// Ab diesem Alter gilt ein nicht abgleichbarer Snapshot als veraltet (F15).
    pub stale_after: Duration,
    /// Mindestabstand zweier Schreibvorgänge des Platten-Caches.
    pub cache_write_interval: Duration,
}

impl Default for ReloadOptions {
    fn default() -> Self {
        Self {
            poll_interval: Some(Duration::from_secs(30)),
            debounce: Duration::from_millis(100),
            backoff_min: Duration::from_millis(250),
            backoff_max: Duration::from_secs(30),
            incremental: true,
            max_incremental_changes: 1000,
            verify_interval: Some(Duration::from_secs(600)),
            stale_after: aigw_core::snapshot::DEFAULT_STALE_AFTER,
            cache_write_interval: Duration::from_secs(10),
        }
    }
}

impl ReloadOptions {
    /// Werte aus der Startkonfiguration (`store.reload_poll_secs`, `0` = keine Abfrage).
    pub fn from_config(cfg: &StoreConfig) -> Self {
        Self {
            poll_interval: (cfg.reload_poll_secs > 0)
                .then(|| Duration::from_secs(cfg.reload_poll_secs)),
            ..Self::default()
        }
    }

    /// Übernimmt den Abschnitt `snapshot` der Startkonfiguration (AIGW-75).
    #[must_use]
    pub fn with_snapshot(mut self, cfg: &SnapshotConfig) -> Self {
        self.incremental = cfg.incremental;
        self.max_incremental_changes = usize::try_from(cfg.max_incremental_changes).unwrap_or(0);
        self.verify_interval =
            (cfg.verify_interval_secs > 0).then(|| Duration::from_secs(cfg.verify_interval_secs));
        self.stale_after = Duration::from_secs(cfg.stale_after_secs);
        self.cache_write_interval = Duration::from_secs(cfg.cache_write_interval_secs);
        self
    }
}

/// Zähler des Reloaders (vorbereitet für den Metrik-Export, AIGW später).
#[derive(Debug, Default)]
pub struct ReloadMetrics {
    /// Erfolgreich veröffentlichte Snapshots.
    pub reloads_ok: AtomicU64,
    /// Fehlgeschlagene Neubauten (alter Snapshot blieb aktiv).
    pub reloads_failed: AtomicU64,
    /// Fehlgeschlagene Versionsabfragen (Datenbank nicht erreichbar).
    pub checks_failed: AtomicU64,
    /// Empfangene Postgres-Benachrichtigungen.
    pub notifications: AtomicU64,
    /// Verbindungsabbrüche bzw. -fehler des Postgres-Listeners.
    pub listener_errors: AtomicU64,
    /// Dauer des letzten erfolgreichen Neubaus in Mikrosekunden.
    pub last_build_micros: AtomicU64,
    /// Seit wann (Unix-ms) die Prüfung bzw. der Neubau fehlschlägt (`0` = aktuell; AIGW-44).
    pub stale_since_unix_ms: AtomicU64,
    /// Davon inkrementell gebaute Snapshots (F16).
    pub reloads_incremental: AtomicU64,
    /// Rückfälle vom inkrementellen auf den vollständigen Neubau.
    pub incremental_fallbacks: AtomicU64,
    /// Durchgeführte Konsistenzprüfungen.
    pub verify_runs: AtomicU64,
    /// Konsistenzprüfungen mit Abweichung (inkrementeller Stand ≠ vollständiger Neubau).
    pub verify_mismatches: AtomicU64,
    /// Geschriebene Platten-Caches (F14).
    pub cache_writes: AtomicU64,
    /// Fehlgeschlagene Schreibvorgänge des Platten-Caches.
    pub cache_write_errors: AtomicU64,
}

impl ReloadMetrics {
    fn inc(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn mark_stale(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(1)
            .max(1);
        // Nur der erste Fehler einer Serie setzt den Zeitpunkt.
        let _ =
            self.stale_since_unix_ms
                .compare_exchange(0, now, Ordering::Relaxed, Ordering::Relaxed);
    }

    fn mark_fresh(&self) {
        self.stale_since_unix_ms.store(0, Ordering::Relaxed);
    }

    /// Wie lange der aktive Snapshot schon nicht mehr mit der Datenbank abgeglichen werden
    /// konnte (`None` = letzte Prüfung erfolgreich).
    pub fn stale_for(&self) -> Option<Duration> {
        let since = self.stale_since_unix_ms.load(Ordering::Relaxed);
        (since != 0).then(|| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            now.saturating_sub(Duration::from_millis(since))
        })
    }
}

/// Baut den Snapshot bei Änderungen neu und veröffentlicht ihn.
#[derive(Debug, Clone)]
pub struct Reloader {
    store: Store,
    keyring: Arc<Keyring>,
    handle: SnapshotHandle,
    opts: ReloadOptions,
    metrics: Arc<ReloadMetrics>,
    cache: Option<Arc<SnapshotCache>>,
    /// Kaltstart aus dem Cache: vor dem ersten Neubau noch migrieren.
    needs_migrate: Arc<AtomicBool>,
}

impl Reloader {
    /// Neuer Reloader für `handle` (der Startsnapshot muss bereits darin stehen).
    pub fn new(
        store: Store,
        keyring: Arc<Keyring>,
        handle: SnapshotHandle,
        opts: ReloadOptions,
    ) -> Self {
        handle.set_stale_after(opts.stale_after);
        Self {
            store,
            keyring,
            handle,
            opts,
            metrics: Arc::default(),
            cache: None,
            needs_migrate: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Kaltstart aus dem Platten-Cache (F14): `store` ist (noch) nicht erreichbar, der Snapshot
    /// gilt bis zum ersten erfolgreichen Neubau als veraltet. Der Reloader migriert die
    /// Datenbank, sobald sie erreichbar ist, und baut dann vollständig neu.
    pub fn from_cache(
        store: Store,
        keyring: Arc<Keyring>,
        cached: CachedSnapshot,
        opts: ReloadOptions,
    ) -> Self {
        let handle = SnapshotHandle::from_cache(cached.snapshot, cached.saved_at);
        let this = Self::new(store, keyring, handle, opts);
        this.needs_migrate.store(true, Ordering::Relaxed);
        this
    }

    /// Schreibt aus der Datenbank gebaute Snapshots zusätzlich in `cache` (F14).
    #[must_use]
    pub fn with_cache(mut self, cache: Arc<SnapshotCache>) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Lädt den Startsnapshot und erzeugt den zugehörigen Halter und Reloader.
    pub async fn bootstrap(
        store: Store,
        keyring: Arc<Keyring>,
        opts: ReloadOptions,
    ) -> Result<Self, StoreError> {
        let snap = load_snapshot(&store, &keyring).await?;
        Ok(Self::new(store, keyring, SnapshotHandle::new(snap), opts))
    }

    /// Halter des aktuellen Snapshots.
    pub fn handle(&self) -> &SnapshotHandle {
        &self.handle
    }

    /// Zähler (geteilt mit dem laufenden Task).
    pub fn metrics(&self) -> Arc<ReloadMetrics> {
        Arc::clone(&self.metrics)
    }

    /// Prüft die Version und baut bei Abweichung neu.
    ///
    /// `Ok(Some(v))`: Snapshot `v` veröffentlicht; `Ok(None)`: bereits aktuell; `Err`: der
    /// bisherige Snapshot bleibt aktiv.
    pub async fn reload_now(&self) -> Result<Option<i64>, StoreError> {
        let base = self.handle.load();
        let current = base.version;
        if self.needs_migrate.load(Ordering::Relaxed) {
            if let Err(e) = self.store.migrate().await {
                self.check_failed();
                return Err(e);
            }
            self.needs_migrate.store(false, Ordering::Relaxed);
            tracing::info!("Datenbank wieder erreichbar und migriert");
        }
        // Ein Stand aus dem Platten-Cache wird immer vollständig ersetzt.
        let from_cache = self.handle.source() == SnapshotSource::Cache;
        let latest = match self.store.config_version().await {
            Ok(v) => v,
            Err(e) => {
                self.check_failed();
                return Err(e);
            }
        };
        if latest == current && !from_cache {
            self.synced();
            return Ok(None);
        }
        let started = Instant::now();
        let mut incremental = false;
        let result = if self.opts.incremental && !from_cache {
            match load_incremental(
                &self.store,
                &self.keyring,
                &base,
                self.opts.max_incremental_changes,
            )
            .await
            {
                Ok(Incremental::UpToDate) => {
                    self.synced();
                    return Ok(None);
                }
                Ok(Incremental::Built { snapshot, changes }) => {
                    incremental = true;
                    tracing::debug!(changes, "Snapshot inkrementell gebaut");
                    Ok(*snapshot)
                }
                Ok(Incremental::NeedFull(reason)) => {
                    ReloadMetrics::inc(&self.metrics.incremental_fallbacks);
                    tracing::info!(
                        reason = reason.as_str(),
                        detail = ?reason,
                        "inkrementeller Snapshot-Neubau nicht möglich – vollständig"
                    );
                    load_snapshot(&self.store, &self.keyring).await
                }
                Err(e) => Err(e),
            }
        } else {
            load_snapshot(&self.store, &self.keyring).await
        };
        match result {
            Ok(snap) => {
                let version = snap.version;
                let took = started.elapsed();
                self.publish(snap);
                if incremental {
                    ReloadMetrics::inc(&self.metrics.reloads_incremental);
                }
                self.metrics.last_build_micros.store(
                    u64::try_from(took.as_micros()).unwrap_or(u64::MAX),
                    Ordering::Relaxed,
                );
                tracing::info!(
                    from = current,
                    to = version,
                    incremental,
                    from_cache,
                    build_ms = took.as_millis() as u64,
                    "Konfigurations-Snapshot neu geladen"
                );
                Ok(Some(version))
            }
            Err(e) => {
                ReloadMetrics::inc(&self.metrics.reloads_failed);
                self.metrics.mark_stale();
                self.handle.mark_failing();
                tracing::error!(
                    error = %e,
                    active = current,
                    wanted = latest,
                    "Snapshot-Neubau fehlgeschlagen – bisheriger Snapshot bleibt aktiv"
                );
                Err(e)
            }
        }
    }

    /// Konsistenzprüfung (F16): baut vollständig neu und vergleicht mit dem aktiven Snapshot.
    ///
    /// `Ok(true)`: gleich (oder der Neubau ist neuer und wurde veröffentlicht); `Ok(false)`:
    /// Abweichung bei gleicher Version – der vollständige Snapshot ersetzt den aktiven, die
    /// Abweichung wird gezählt und als Fehler geloggt.
    pub async fn verify_now(&self) -> Result<bool, StoreError> {
        let full = match load_snapshot(&self.store, &self.keyring).await {
            Ok(s) => s,
            Err(e) => {
                self.check_failed();
                return Err(e);
            }
        };
        ReloadMetrics::inc(&self.metrics.verify_runs);
        let current = self.handle.load();
        if self.handle.source() == SnapshotSource::Cache || full.version > current.version {
            self.publish(full);
            return Ok(true);
        }
        if full.version < current.version {
            return Ok(true);
        }
        if full == *current {
            self.synced();
            return Ok(true);
        }
        ReloadMetrics::inc(&self.metrics.verify_mismatches);
        tracing::error!(
            version = current.version,
            "Konsistenzprüfung: inkrementeller Snapshot weicht vom vollständigen Neubau ab, \
             ersetze ihn (bitte als Fehler melden)"
        );
        self.publish(full);
        Ok(false)
    }

    fn publish(&self, snap: Snapshot) {
        self.handle.store(snap);
        ReloadMetrics::inc(&self.metrics.reloads_ok);
        self.metrics.mark_fresh();
    }

    fn synced(&self) {
        self.metrics.mark_fresh();
        self.handle.mark_synced();
    }

    fn check_failed(&self) {
        ReloadMetrics::inc(&self.metrics.checks_failed);
        self.metrics.mark_stale();
        self.handle.mark_failing();
    }

    /// Startet den Reloader (und bei Postgres den Listener) als Hintergrund-Tasks.
    pub fn spawn(self) -> ReloaderTask {
        let wake = Arc::new(Notify::new());
        let mut tasks = Vec::with_capacity(2);
        if self.store.backend() == Backend::Postgres {
            tasks.push(tokio::spawn(listen(
                self.store.clone(),
                Arc::clone(&wake),
                Arc::clone(&self.metrics),
                self.opts.clone(),
            )));
        }
        if let Some(cache) = &self.cache {
            tasks.push(tokio::spawn(write_cache(
                self.handle.clone(),
                Arc::clone(cache),
                self.opts.cache_write_interval,
                Arc::clone(&self.metrics),
            )));
        }
        // Kaltstart aus dem Cache: sofort (und dann mit Backoff) die Datenbank versuchen.
        if self.handle.source() == SnapshotSource::Cache {
            wake.notify_one();
        }
        tasks.push(tokio::spawn(self.run(wake)));
        ReloaderTask { tasks }
    }

    async fn run(self, wake: Arc<Notify>) {
        let mut commits = self.store.subscribe_commits();
        let mut poll = self.opts.poll_interval.map(|every| {
            let mut i = tokio::time::interval_at(Instant::now() + every, every);
            i.set_missed_tick_behavior(MissedTickBehavior::Delay);
            i
        });
        let mut verify = self.opts.verify_interval.map(|every| {
            let mut i = tokio::time::interval_at(Instant::now() + every, every);
            i.set_missed_tick_behavior(MissedTickBehavior::Delay);
            i
        });
        let mut retry_at: Option<Instant> = None;
        let mut backoff = self.opts.backoff_min;
        loop {
            tokio::select! {
                changed = commits.changed() => {
                    if changed.is_err() {
                        // Kann nicht passieren (wir halten selbst einen `Store`); dann eben
                        // nur noch über die übrigen Auslöser.
                        commits = self.store.subscribe_commits();
                    }
                }
                () = wake.notified() => {}
                () = maybe_tick(poll.as_mut()) => {}
                () = maybe_sleep_until(retry_at) => {}
                () = maybe_tick(verify.as_mut()) => {
                    if let Err(e) = self.verify_now().await {
                        tracing::warn!(error = %e, "Konsistenzprüfung des Snapshots fehlgeschlagen");
                    }
                    continue;
                }
            }
            if !self.opts.debounce.is_zero() {
                tokio::time::sleep(self.opts.debounce).await;
            }
            commits.borrow_and_update();
            match self.reload_now().await {
                Ok(_) => {
                    retry_at = None;
                    backoff = self.opts.backoff_min;
                }
                Err(e) => {
                    if matches!(e, StoreError::Db(_)) {
                        tracing::warn!(
                            error = %e,
                            retry_in_ms = backoff.as_millis() as u64,
                            "Konfiguration nicht prüfbar (Datenbank?) – letzter Snapshot bleibt aktiv"
                        );
                    }
                    retry_at = Some(Instant::now() + backoff);
                    backoff = (backoff * 2).min(self.opts.backoff_max);
                }
            }
        }
    }
}

/// Schreibt den aktiven Snapshot in den Platten-Cache, sobald er aus der Datenbank stammt und
/// eine neue Version hat (höchstens alle `every`).
async fn write_cache(
    handle: SnapshotHandle,
    cache: Arc<SnapshotCache>,
    every: Duration,
    metrics: Arc<ReloadMetrics>,
) {
    let mut rx = handle.subscribe();
    let mut written: Option<i64> = None;
    loop {
        let snap = handle.load();
        if handle.source() == SnapshotSource::Database && written != Some(snap.version) {
            let version = snap.version;
            let c = Arc::clone(&cache);
            match tokio::task::spawn_blocking(move || c.write(&snap)).await {
                Ok(Ok(())) => {
                    written = Some(version);
                    ReloadMetrics::inc(&metrics.cache_writes);
                    tracing::debug!(
                        version,
                        path = %cache.path().display(),
                        "Snapshot-Cache geschrieben"
                    );
                }
                Ok(Err(e)) => {
                    ReloadMetrics::inc(&metrics.cache_write_errors);
                    tracing::warn!(error = %e, "Snapshot-Cache nicht geschrieben");
                }
                Err(e) => {
                    ReloadMetrics::inc(&metrics.cache_write_errors);
                    tracing::warn!(error = %e, "Snapshot-Cache: Schreib-Task abgebrochen");
                }
            }
        }
        if rx.changed().await.is_err() {
            return;
        }
        // Weitere Änderungen in diesem Zeitraum zusammenfassen.
        tokio::time::sleep(every).await;
    }
}

async fn maybe_tick(interval: Option<&mut tokio::time::Interval>) {
    match interval {
        Some(i) => {
            i.tick().await;
        }
        None => std::future::pending().await,
    }
}

async fn maybe_sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Lauscht auf `NOTIFY aigw_config` (eigene Verbindung, Reconnect mit Backoff).
async fn listen(store: Store, wake: Arc<Notify>, metrics: Arc<ReloadMetrics>, opts: ReloadOptions) {
    let crate::db::Pool::Postgres(pool) = &store.pool else {
        return;
    };
    // Eigener Pool mit einer Verbindung (gleiche Optionen), damit die dauerhaft belegte
    // LISTEN-Verbindung dem Store-Pool keinen Platz wegnimmt.
    let listen_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy_with(pool.connect_options().as_ref().clone());
    let mut backoff = opts.backoff_min;
    loop {
        match PgListener::connect_with(&listen_pool).await {
            Ok(mut listener) => match listener.listen(NOTIFY_CHANNEL).await {
                Ok(()) => {
                    tracing::debug!(
                        channel = NOTIFY_CHANNEL,
                        "Lausche auf Konfigurationsänderungen"
                    );
                    backoff = opts.backoff_min;
                    // Während der Verbindungslosigkeit verpasste Änderungen nachholen.
                    wake.notify_one();
                    loop {
                        match listener.try_recv().await {
                            Ok(Some(n)) => {
                                ReloadMetrics::inc(&metrics.notifications);
                                tracing::debug!(version = n.payload(), "Änderung gemeldet");
                                wake.notify_one();
                            }
                            Ok(None) => {
                                // Verbindung verloren; der nächste Aufruf verbindet neu und
                                // lauscht wieder. Dazwischen Verpasstes holt die Prüfung nach.
                                ReloadMetrics::inc(&metrics.listener_errors);
                                tracing::warn!("LISTEN-Verbindung verloren, verbinde neu");
                                wake.notify_one();
                            }
                            Err(e) => {
                                ReloadMetrics::inc(&metrics.listener_errors);
                                tracing::warn!(error = %e, "LISTEN fehlgeschlagen");
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    ReloadMetrics::inc(&metrics.listener_errors);
                    tracing::warn!(error = %e, "LISTEN {NOTIFY_CHANNEL} fehlgeschlagen");
                }
            },
            Err(e) => {
                ReloadMetrics::inc(&metrics.listener_errors);
                tracing::warn!(
                    error = %e,
                    retry_in_ms = backoff.as_millis() as u64,
                    "Listener-Verbindung zu Postgres fehlgeschlagen"
                );
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(opts.backoff_max);
    }
}

/// Laufende Reloader-Tasks; werden beim Fallenlassen beendet.
#[derive(Debug)]
pub struct ReloaderTask {
    tasks: Vec<JoinHandle<()>>,
}

impl ReloaderTask {
    /// Beendet die Tasks und wartet darauf.
    pub async fn shutdown(mut self) {
        for t in self.tasks.drain(..) {
            t.abort();
            let _ = t.await;
        }
    }
}

impl Drop for ReloaderTask {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_from_config() {
        let mut cfg = StoreConfig::default();
        assert_eq!(
            ReloadOptions::from_config(&cfg).poll_interval,
            Some(Duration::from_secs(30))
        );
        cfg.reload_poll_secs = 0;
        assert_eq!(ReloadOptions::from_config(&cfg).poll_interval, None);
    }
}
