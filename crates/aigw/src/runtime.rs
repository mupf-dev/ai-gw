// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Laufzeitzustand des Servers: Datenbank, Master-Key, Konfigurations-Snapshot mit Hot Reload
//! (§14, AIGW-10), Request-Log (§12, AIGW-43), Metriken und OTLP-Export (AIGW-44/45),
//! Zähler für Limits/Kapazität (AIGW-29/30) und Budgets mit Abgleich gegen `budget_usage`
//! (AIGW-31).

use std::sync::Arc;
use std::time::Duration;

use aigw_billing::{BudgetLedger, BudgetSyncTask, Budgets, Calendar};
use aigw_core::Config;
use aigw_core::observe::RequestObserver;
use aigw_core::snapshot::SnapshotHandle;
use aigw_observe::ObserveMetrics;
use aigw_observe::otel::TraceActivator;
use aigw_store::{
    Keyring, ReloadMetrics, ReloadOptions, Reloader, ReloaderTask, SnapshotCache, Store, StoreError,
};

/// Gemeinsamer Zustand für Listener und Hintergrund-Tasks.
#[derive(Debug)]
#[allow(dead_code)] // Felder werden von Gateway/Admin-API (AIGW-11 ff.) genutzt.
pub(crate) struct Runtime {
    /// Datenbank (Admin-API schreibt darüber; Commits lösen den Reloader aus).
    pub(crate) store: Store,
    /// Master-Keys für Zugangsdaten.
    pub(crate) keyring: Arc<Keyring>,
    /// Aktueller Konfigurations-Snapshot (Hot Path: `snapshot.load()`).
    pub(crate) snapshot: SnapshotHandle,
    /// Zähler des Reloaders.
    pub(crate) reload_metrics: Arc<ReloadMetrics>,
    /// Request-Log-Hook für den Proxy (AIGW-43; Verdrahtung in den Handlern: AIGW-14/15).
    pub(crate) observer: Arc<dyn RequestObserver>,
    /// Zähler des Request-Logs (`/metrics`).
    pub(crate) observe_metrics: Arc<ObserveMetrics>,
    /// Zähler-Backend (`counters.backend`) für Limits und Deployment-Kapazität.
    pub(crate) counters: Arc<dyn aigw_counters::CounterBackend>,
    /// Budgetstände (Abgleich mit `budget_usage`).
    pub(crate) budgets: Arc<Budgets>,
    /// Kalender der Budget-Zeiträume (`billing.timezone`).
    pub(crate) calendar: Arc<Calendar>,
    /// Client für ausgehende Webhooks (Plugin `webhook`, Ereignisse; AIGW-61), SSRF-Regeln
    /// aus `admin.allow_*_upstreams`.
    pub(crate) webhooks: aigw_webhook::WebhookClient,
    /// Ereignis-Webhooks (nur mit `events.webhooks`).
    events: Option<(aigw_webhook::EventDispatcher, aigw_webhook::EventTask)>,
    budget_sync: BudgetSyncTask,
    reloader: ReloaderTask,
    observe: crate::observe::Observe,
}

impl Runtime {
    /// Verbindet und migriert die Datenbank, lädt Master-Key und Startsnapshot und startet
    /// den Reloader. Muss innerhalb der Tokio-Laufzeit aufgerufen werden.
    pub(crate) async fn start(
        config: &Config,
        traces: &TraceActivator,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let keyring = Arc::new(Keyring::from_config(&config.secrets)?);
        let opts = ReloadOptions::from_config(&config.store).with_snapshot(&config.snapshot);
        let cache = config
            .snapshot
            .cache_file
            .as_ref()
            .map(|p| Arc::new(SnapshotCache::new(p.clone(), Arc::clone(&keyring))));
        let (store, reloader) = match connect_and_load(config, &keyring, opts.clone()).await {
            Ok(v) => v,
            Err(e) if e.is_unavailable() && cache.is_some() => {
                cold_start_from_cache(config, &keyring, cache.as_deref(), opts, e).await?
            }
            Err(e) => return Err(e.into()),
        };
        let reloader = match &cache {
            Some(c) => reloader.with_cache(Arc::clone(c)),
            None => reloader,
        };
        let snapshot = reloader.handle().clone();
        let reload_metrics = reloader.metrics();
        let snap = snapshot.load();
        tracing::info!(
            backend = ?store.backend(),
            config_version = snap.version,
            keys = snap.keys.len(),
            model_groups = snap.model_groups.len(),
            source = snapshot.source().as_str(),
            "Konfigurations-Snapshot geladen"
        );
        let observe = crate::observe::start(config, &store, traces)?;
        let counters =
            aigw_counters::from_config(config, aigw_counters::CounterOptions::default()).await?;
        // Zustand und Nachtragen des Zähler-Backends in /metrics (F28).
        if let Some(g) = observe.log.gateway_metrics() {
            let c = Arc::clone(&counters);
            g.set_counter_health(move || matches!(c.health(), aigw_counters::Health::Degraded(_)));
            let c = Arc::clone(&counters);
            g.set_counter_stats(move || {
                let s = c.stats();
                (s.replayed, s.dropped, s.pending)
            });
        }
        warn_limits(config, &snap);
        let calendar = Arc::new(
            Calendar::new(&config.billing.timezone)
                .map_err(|e| format!("billing.timezone: {e}"))?,
        );
        let ledger: Arc<dyn BudgetLedger> = Arc::new(store.clone());
        let webhooks = aigw_webhook::WebhookClient::new(
            aigw_core::upstream::UpstreamPolicy::from_config(&config.admin)?,
            aigw_webhook::ClientOptions::default(),
        )
        .map_err(|e| format!("Webhook-Client: {e}"))?;
        let events = start_events(config, &webhooks, &store, &snapshot)?;
        let mut budgets = Budgets::new((*calendar).clone(), Some(ledger));
        if let Some((d, _)) = &events {
            budgets = budgets.with_events(Arc::new(aigw_billing::WebhookEvents::new(
                d.clone(),
                (*calendar).clone(),
            )));
        }
        // Strikte Budgets im Cluster exakt über Redis (F106), sonst im Abgleich-Takt.
        if config.counters.backend == aigw_core::config::CounterBackendKind::Redis
            && let Some(url) = config.counters.redis_url.as_deref()
        {
            budgets = budgets.with_shared(Arc::new(
                aigw_proxy::RedisBudgets::new(url, "aigw:b:")
                    .map_err(|e| format!("Budgets (Redis): {e}"))?,
            ));
        }
        let budgets = Arc::new(budgets);
        let budget_sync = budgets.spawn_sync(Duration::from_millis(config.billing.budget_sync_ms));
        tracing::info!(
            counters = ?counters.kind(),
            timezone = calendar.timezone(),
            "Limits und Budgets bereit"
        );
        Ok(Self {
            counters,
            budgets,
            calendar,
            webhooks,
            events,
            budget_sync,
            observer: observe.log.observer(),
            observe_metrics: Arc::clone(observe.log.metrics()),
            observe,
            store,
            keyring,
            snapshot,
            reload_metrics,
            reloader: reloader.spawn(),
        })
    }

    /// Öffnet bei `SIGHUP` die Exportdateien neu (JSON-Lines, F31); `None` ohne Unix-Signale.
    pub(crate) fn spawn_reopen_on_hup(&self) -> Option<tokio::task::JoinHandle<()>> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut hup = match signal(SignalKind::hangup()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "SIGHUP-Handler nicht installierbar");
                    return None;
                }
            };
            let reopen = self.observe.log.reopener();
            Some(tokio::spawn(async move {
                while hup.recv().await.is_some() {
                    tracing::info!("SIGHUP: Exportdateien werden neu geöffnet");
                    reopen();
                }
            }))
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    /// Quellen für `/metrics` (Gateway, Snapshot, Request-Log, OTLP).
    pub(crate) fn metrics_sources(&self) -> crate::observe::MetricsSources {
        crate::observe::MetricsSources {
            log: Arc::clone(&self.observe_metrics),
            gateway: self.observe.log.gateway_metrics().cloned(),
            otlp: Arc::clone(&self.observe.otlp_metrics),
            snapshot: self.snapshot.clone(),
            reload: Arc::clone(&self.reload_metrics),
            events: self.events.as_ref().map(|(d, _)| Arc::clone(d.metrics())),
            cache: None,
            plugins: None,
        }
    }

    /// Exportiert gepufferte Spans, leert das Request-Log, beendet den Reloader und schließt
    /// die Datenbank.
    pub(crate) async fn shutdown(self) {
        drop(self.observer);
        if !self.observe.shutdown().await {
            tracing::warn!(
                "Request-Log bzw. Traces beim Herunterfahren nicht vollständig geschrieben"
            );
        }
        self.budget_sync.shutdown().await;
        if let Some((d, task)) = self.events {
            drop(d);
            task.shutdown(Duration::from_secs(5)).await;
        }
        self.reloader.shutdown().await;
        self.store.close().await;
    }
}

/// Verbindet und migriert die Datenbank und lädt den Startsnapshot.
async fn connect_and_load(
    config: &Config,
    keyring: &Arc<Keyring>,
    opts: ReloadOptions,
) -> Result<(Store, Reloader), StoreError> {
    let store = Store::connect_with(&config.store).await?;
    store.migrate().await?;
    let reloader = Reloader::bootstrap(store.clone(), Arc::clone(keyring), opts).await?;
    Ok((store, reloader))
}

/// Kaltstart ohne Datenbank aus dem verschlüsselten Snapshot-Cache (F14). Ohne brauchbaren
/// Cache bleibt es beim Abbruch mit dem ursprünglichen Fehler.
async fn cold_start_from_cache(
    config: &Config,
    keyring: &Arc<Keyring>,
    cache: Option<&SnapshotCache>,
    opts: ReloadOptions,
    db_error: StoreError,
) -> Result<(Store, Reloader), Box<dyn std::error::Error>> {
    let Some(cached) = cache.and_then(SnapshotCache::read_or_discard) else {
        return Err(format!(
            "Datenbank nicht erreichbar und kein gültiger Snapshot-Cache vorhanden: {db_error}"
        )
        .into());
    };
    let age = std::time::SystemTime::now()
        .duration_since(cached.saved_at)
        .unwrap_or_default();
    tracing::warn!(
        error = %db_error,
        config_version = cached.snapshot.version,
        cache_age_secs = age.as_secs(),
        written_by = %cached.gateway_version,
        "Datenbank nicht erreichbar – Start mit dem Snapshot-Cache (Readiness: degraded); \
         der normale Betrieb beginnt, sobald die Datenbank erreichbar ist"
    );
    let store = Store::connect_lazy_with(&config.store).await?;
    let reloader = Reloader::from_cache(store.clone(), Arc::clone(keyring), cached, opts);
    Ok((store, reloader))
}

/// F29: Sekundenlimits mit `counters.backend: postgres-sync` sind im Cluster unscharf –
/// einmal beim Start warnen (die Admin-API liefert dieselben Hinweise unter
/// `GET /admin/api/v1/usage/limit-warnings`).
fn warn_limits(config: &Config, snap: &aigw_core::snapshot::Snapshot) {
    let mut keys: Vec<_> = snap.keys.values().collect();
    keys.sort_by(|a, b| a.key.metadata.name.cmp(&b.key.metadata.name));
    keys.dedup_by(|a, b| a.key.id == b.key.id);
    let objects = snap
        .organizations
        .values()
        .map(|o| ("organization", o.metadata.name.as_str(), &o.spec.limits[..]))
        .chain(
            snap.teams
                .values()
                .map(|t| ("team", t.metadata.name.as_str(), &t.spec.limits[..])),
        )
        .chain(keys.iter().map(|k| {
            (
                "api-key",
                k.key.metadata.name.as_str(),
                &k.key.spec.limits[..],
            )
        }))
        .chain(
            snap.model_groups
                .iter()
                // Aliasse zeigen auf dieselbe Gruppe: nur unter dem kanonischen Namen.
                .filter(|(name, g)| g.group.metadata.name == **name)
                .map(|(_, g)| {
                    ("model-group", g.group.metadata.name.as_str(), &g.group.spec.limits[..])
                }),
        );
    let warnings = aigw_core::validate::bounds::limit_warnings(config.counters.backend, objects);
    if !warnings.is_empty() {
        tracing::warn!(
            anzahl = warnings.len(),
            beispiele = ?warnings
                .iter()
                .take(5)
                .map(|w| format!("{} {:?} {}", w.kind, w.name, w.field))
                .collect::<Vec<_>>(),
            sync_interval_ms = config.counters.sync_interval_ms,
            "Sekundenlimits mit counters.backend = postgres-sync: im Cluster nur ungenau \
             (Abgleich im Abstand sync_interval_ms) – redis verwenden oder längere Fenster; \
             Liste: GET /admin/api/v1/usage/limit-warnings"
        );
    }
}

/// Startet die Ereignis-Webhooks (`events.webhooks`, AIGW-61); `None` ohne Empfänger.
fn start_events(
    config: &Config,
    client: &aigw_webhook::WebhookClient,
    store: &Store,
    snapshot: &SnapshotHandle,
) -> Result<Option<(aigw_webhook::EventDispatcher, aigw_webhook::EventTask)>, String> {
    let cfg = &config.events;
    if cfg.webhooks.is_empty() {
        return Ok(None);
    }
    let source: Arc<dyn aigw_webhook::CredentialSource> = Arc::new(snapshot.clone());
    let targets = cfg
        .webhooks
        .iter()
        .map(|w| aigw_webhook::EventTarget::from_config(client, w, Some(Arc::clone(&source))))
        .collect::<Result<Vec<_>, _>>()?;
    let dedup: Option<Arc<dyn aigw_webhook::EventDedup>> = cfg
        .dedup
        .then(|| Arc::new(store.clone()) as Arc<dyn aigw_webhook::EventDedup>);
    tracing::info!(
        webhooks = targets.len(),
        dedup = cfg.dedup,
        "Ereignis-Webhooks bereit"
    );
    Ok(Some(aigw_webhook::EventDispatcher::start(
        client.clone(),
        targets,
        dedup,
        aigw_webhook::DispatcherOptions::from_config(cfg),
    )))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;
    use aigw_core::snapshot::{Snapshot, SnapshotSource};
    use aigw_store::MasterKey;

    /// Konfiguration mit Datenbank in einem (noch) fehlenden Verzeichnis und Snapshot-Cache.
    fn config(dir: &std::path::Path) -> (Config, std::path::PathBuf, Arc<Keyring>) {
        let (key, hex) = MasterKey::generate().unwrap();
        let key_file = dir.join("master.key");
        std::fs::write(&key_file, hex).unwrap();
        let db_dir = dir.join("db");
        let mut cfg = Config::default();
        cfg.secrets.master_key_file = Some(key_file);
        cfg.store.url = format!("sqlite://{}/aigw.db?mode=rwc", db_dir.display());
        cfg.snapshot.cache_file = Some(dir.join("snapshot.cache"));
        (cfg, db_dir, Arc::new(Keyring::new(key)))
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "aigw-runtime-{tag}-{}",
            aigw_core::resource::new_id()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// F14: Kaltstart ohne Datenbank mit dem letzten Stand aus dem Cache (degraded), danach
    /// normaler Betrieb, sobald die Datenbank erreichbar ist.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cold_start_from_cache_then_database_returns() {
        let dir = temp_dir("cold");
        let (cfg, db_dir, keyring) = config(&dir);
        SnapshotCache::new(cfg.snapshot.cache_file.clone().unwrap(), keyring)
            .write(&Snapshot {
                version: 5,
                ..Snapshot::default()
            })
            .unwrap();
        let (_sub, traces) = aigw_observe::otel::test_subscriber();
        let rt = Runtime::start(&cfg, &traces).await.unwrap();
        let st = rt.snapshot.status();
        assert_eq!(
            (st.source, st.version, st.stale),
            (SnapshotSource::Cache, 5, true)
        );

        std::fs::create_dir_all(&db_dir).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while rt.snapshot.source() != SnapshotSource::Database {
            assert!(
                std::time::Instant::now() < deadline,
                "kein Wechsel auf die Datenbank"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let st = rt.snapshot.status();
        assert!(!st.stale, "{st:?}");
        assert_eq!(st.version, 0);
        rt.shutdown().await;
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Ohne (gültigen) Cache bleibt es beim Abbruch; ein kaputter Cache wird verworfen.
    #[tokio::test]
    async fn no_database_without_valid_cache_fails() {
        let dir = temp_dir("nocache");
        let (cfg, _db, _) = config(&dir);
        let (_sub, traces) = aigw_observe::otel::test_subscriber();
        let err = Runtime::start(&cfg, &traces).await.unwrap_err();
        assert!(
            err.to_string().contains("kein gültiger Snapshot-Cache"),
            "{err}"
        );

        let cache = cfg.snapshot.cache_file.clone().unwrap();
        std::fs::write(&cache, b"AIGWSNAP kaputt").unwrap();
        assert!(Runtime::start(&cfg, &traces).await.is_err());
        assert!(!cache.exists(), "kaputter Cache verworfen");
        let _ = std::fs::remove_dir_all(dir);
    }
}
