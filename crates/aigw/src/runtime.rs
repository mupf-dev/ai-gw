// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Laufzeitzustand des Servers: Datenbank, Master-Key, Konfigurations-Snapshot mit Hot Reload
//! (§14, AIGW-10), Request-Log (§12, AIGW-43), Metriken und OTLP-Export (AIGW-44/45).

use std::sync::Arc;

use aigw_core::Config;
use aigw_core::observe::RequestObserver;
use aigw_core::snapshot::SnapshotHandle;
use aigw_observe::ObserveMetrics;
use aigw_observe::otel::TraceActivator;
use aigw_store::{Keyring, ReloadMetrics, ReloadOptions, Reloader, ReloaderTask, Store};

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
        let store = Store::connect_with(&config.store).await?;
        store.migrate().await?;
        let reloader = Reloader::bootstrap(
            store.clone(),
            Arc::clone(&keyring),
            ReloadOptions::from_config(&config.store),
        )
        .await?;
        let snapshot = reloader.handle().clone();
        let reload_metrics = reloader.metrics();
        let snap = snapshot.load();
        tracing::info!(
            backend = ?store.backend(),
            config_version = snap.version,
            keys = snap.keys.len(),
            model_groups = snap.model_groups.len(),
            "Konfigurations-Snapshot geladen"
        );
        let observe = crate::observe::start(config, &store, traces)?;
        Ok(Self {
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

    /// Quellen für `/metrics` (Gateway, Snapshot, Request-Log, OTLP).
    pub(crate) fn metrics_sources(&self) -> crate::observe::MetricsSources {
        crate::observe::MetricsSources {
            log: Arc::clone(&self.observe_metrics),
            gateway: self.observe.log.gateway_metrics().cloned(),
            otlp: Arc::clone(&self.observe.otlp_metrics),
            snapshot: self.snapshot.clone(),
            reload: Arc::clone(&self.reload_metrics),
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
        self.reloader.shutdown().await;
        self.store.close().await;
    }
}
