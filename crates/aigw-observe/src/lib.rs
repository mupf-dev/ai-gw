// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Request-Log von ai-gw (AIGW-43): Puffer, Batch-Writer, Minuten-Rollups,
//! Payload-Maskierung, Aufbewahrung und Exporter.
//!
//! ```text
//! Proxy ──record()──► RequestLogger ──try_send──► [begrenzter Kanal] ──► Writer-Task
//!        (< 1 µs, nie wartend; voll → aigw_log_dropped_total)        │ alle 500 ms / 1000 Einträge
//!                                                                     ▼
//!                       Aufbereitung (spawn_blocking): Zeilen, Payload maskiert, Rollup-Deltas
//!                                                                     ▼
//!                  Store: eine Transaktion (request_log, request_payload, usage_rollup UPSERT)
//!                                                                     ▼
//!                                        Exporter (JSON-Lines, OTLP-Logs, später Webhook)
//! ```
//!
//! Dazu (AIGW-44/45): [`prom`] – Prometheus-Metriken über einen Fan-out-Beobachter vor dem
//! Request-Log; [`otel`] – OTLP/HTTP-Export von Traces (`tracing`-Layer) und Logs.
//!
//! Eigene Crate statt Modul in `aigw-proxy`: Der Proxy kennt nur den Trait
//! [`aigw_core::observe::RequestObserver`] und bleibt frei von Datenbank-, Regex- und
//! OTLP-Abhängigkeiten; das Request-Log ist eigenständig testbar und austauschbar.
//! Details: `docs/konzept/07-beobachtung.md`.

pub mod export;
pub mod logger;
pub mod mask;
pub mod metrics;
pub mod otel;
pub mod process;
pub mod prom;
pub mod retention;
mod writer;

use std::sync::Arc;
use std::time::Duration;

use aigw_core::config::ObserveConfig;
use aigw_core::observe::RequestObserver;
use aigw_store::Store;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

pub use export::{ExportError, ExportFuture, JsonLinesExporter, LogExporter};
pub use logger::RequestLogger;
pub use mask::{PayloadMasker, PiiMasker};
pub use metrics::{MetricsSnapshot, ObserveMetrics};
pub use process::LogEntry;
pub use prom::{DeploymentGauge, GatewayMetrics, MetricsObserver};

use crate::process::Processor;
use crate::writer::Writer;

/// Startet das Request-Log.
#[derive(Debug)]
pub struct ObserveBuilder {
    cfg: ObserveConfig,
    store: Option<Store>,
    exporters: Vec<Arc<dyn LogExporter>>,
    masker: Option<Arc<dyn PayloadMasker>>,
}

impl ObserveBuilder {
    /// Builder aus dem Konfigurationsabschnitt `observe`.
    pub fn new(cfg: ObserveConfig) -> Self {
        Self {
            cfg,
            store: None,
            exporters: Vec::new(),
            masker: None,
        }
    }

    /// Datenbank für Rohlogs, Payloads, Rollups und Aufbewahrung (ohne: nur Exporter).
    pub fn store(mut self, store: Store) -> Self {
        self.store = Some(store);
        self
    }

    /// Zusätzlicher Exporter (der JSON-Lines-Exporter kommt aus der Konfiguration).
    pub fn exporter(mut self, exporter: Arc<dyn LogExporter>) -> Self {
        self.exporters.push(exporter);
        self
    }

    /// Eigene Maskierung; Standard ist [`PiiMasker`] (PII-Engine, AIGW-37).
    pub fn masker(mut self, masker: Arc<dyn PayloadMasker>) -> Self {
        self.masker = Some(masker);
        self
    }

    /// Startet Writer- und Aufbewahrungs-Task (benötigt eine laufende Tokio-Runtime).
    pub fn start(self) -> ObserveHandle {
        let cfg = self.cfg;
        let metrics = Arc::new(ObserveMetrics::default());
        let (tx, rx) = mpsc::channel(cfg.queue_capacity.max(1));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let mut exporters = self.exporters;
        let jsonl = cfg.jsonl.enabled.then(|| {
            Arc::new(
                JsonLinesExporter::new(cfg.jsonl.path.clone(), cfg.jsonl.include_payload)
                    .with_rotation(cfg.jsonl.rotate.clone()),
            )
        });
        if let Some(j) = &jsonl {
            exporters.push(Arc::clone(j) as Arc<dyn LogExporter>);
        }
        if !cfg.rollups {
            tracing::warn!(
                "observe.rollups: false – keine Rollups, Dashboards und Analytics bleiben leer"
            );
        }
        let writer = Writer {
            store: self.store.clone(),
            store_request_log: cfg.request_log,
            exporters,
            processor: Processor {
                payload_max_bytes: cfg.payload_max_bytes,
                masker: self.masker.unwrap_or_else(|| Arc::new(PiiMasker::new())),
                rollups: cfg.rollups,
            },
            metrics: Arc::clone(&metrics),
            batch_size: cfg.batch_size,
            flush_interval: Duration::from_millis(cfg.flush_interval_ms),
        };
        let writer = tokio::spawn(writer.run(rx, shutdown_rx.clone()));
        let retention = self.store.map(|store| {
            tokio::spawn(retention::run(
                store,
                cfg.retention.clone(),
                Arc::clone(&metrics),
                shutdown_rx,
            ))
        });
        let gateway = cfg
            .metrics
            .enabled
            .then(|| Arc::new(GatewayMetrics::new(&cfg.metrics)));
        ObserveHandle {
            logger: RequestLogger::new(tx, Arc::clone(&metrics), cfg.payload),
            jsonl,
            gateway,
            metrics,
            shutdown: shutdown_tx,
            writer,
            retention,
            shutdown_timeout: Duration::from_millis(cfg.shutdown_timeout_ms),
        }
    }
}

/// Laufendes Request-Log.
#[derive(Debug)]
pub struct ObserveHandle {
    logger: RequestLogger,
    jsonl: Option<Arc<JsonLinesExporter>>,
    gateway: Option<Arc<GatewayMetrics>>,
    metrics: Arc<ObserveMetrics>,
    shutdown: watch::Sender<bool>,
    writer: JoinHandle<()>,
    retention: Option<JoinHandle<()>>,
    shutdown_timeout: Duration,
}

impl ObserveHandle {
    /// Logger für den Proxy (klonbar).
    pub fn logger(&self) -> RequestLogger {
        self.logger.clone()
    }

    /// Beobachter für den Proxy-Zustand: Gateway-Metriken (falls `observe.metrics.enabled`)
    /// und danach das Request-Log.
    pub fn observer(&self) -> Arc<dyn RequestObserver> {
        let log: Arc<dyn RequestObserver> = Arc::new(self.logger.clone());
        match &self.gateway {
            Some(g) => Arc::new(MetricsObserver::new(Arc::clone(g), log)),
            None => log,
        }
    }

    /// Prometheus-Metriken des Gateways (`None`, wenn abgeschaltet).
    pub fn gateway_metrics(&self) -> Option<&Arc<GatewayMetrics>> {
        self.gateway.as_ref()
    }

    /// Zähler.
    pub fn metrics(&self) -> &Arc<ObserveMetrics> {
        &self.metrics
    }

    /// Öffnet die Exportdateien neu (`SIGHUP`, F31): nach einem externen logrotate schreibt
    /// der JSON-Lines-Exporter in die neue Datei unter dem konfigurierten Pfad.
    pub fn reopen(&self) {
        if let Some(j) = &self.jsonl {
            j.reopen();
        }
    }

    /// Wie [`ObserveHandle::reopen`], als eigenständige Funktion (z. B. für einen
    /// Signal-Task).
    pub fn reopener(&self) -> impl Fn() + Send + Sync + 'static {
        let jsonl = self.jsonl.clone();
        move || {
            if let Some(j) = &jsonl {
                j.reopen();
            }
        }
    }

    /// Graceful Shutdown: nimmt nichts Neues mehr an und schreibt den Puffer vollständig
    /// (höchstens `shutdown_timeout_ms`). Liefert `false` bei Zeitüberschreitung.
    pub async fn shutdown(self) -> bool {
        let _ = self.shutdown.send(true);
        if let Some(r) = self.retention {
            r.abort();
        }
        let mut writer = self.writer;
        match tokio::time::timeout(self.shutdown_timeout, &mut writer).await {
            Ok(_) => true,
            Err(_) => {
                writer.abort();
                tracing::warn!(
                    "Request-Log: Puffer beim Herunterfahren nicht vollständig geschrieben"
                );
                false
            }
        }
    }
}
