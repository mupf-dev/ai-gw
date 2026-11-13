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
        let counters =
            aigw_counters::from_config(config, aigw_counters::CounterOptions::default()).await?;
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
