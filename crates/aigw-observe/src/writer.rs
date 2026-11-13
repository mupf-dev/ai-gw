// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Batch-Writer: sammelt aus dem Kanal, schreibt alle `flush_interval` oder bei
//! `batch_size` Einträgen in die Datenbank (eine Transaktion) und an die Exporter.

use std::sync::Arc;
use std::time::Duration;

use aigw_core::observe::RequestRecord;
use aigw_store::Store;
use aigw_store::observe::{PayloadRow, RequestLogRow};
use tokio::sync::{mpsc, watch};
use tokio::time::MissedTickBehavior;

use crate::export::LogExporter;
use crate::metrics::ObserveMetrics;
use crate::process::{Batch, Processor};

/// Wiederholungen eines fehlgeschlagenen DB-Schreibvorgangs (im laufenden Betrieb).
const WRITE_ATTEMPTS: u32 = 4;
/// Erste Wartezeit vor einer Wiederholung (verdoppelt sich).
const RETRY_BASE: Duration = Duration::from_millis(250);

/// Alles, was der Writer braucht.
#[derive(Debug)]
pub(crate) struct Writer {
    pub(crate) store: Option<Store>,
    pub(crate) store_request_log: bool,
    pub(crate) exporters: Vec<Arc<dyn LogExporter>>,
    pub(crate) processor: Processor,
    pub(crate) metrics: Arc<ObserveMetrics>,
    pub(crate) batch_size: usize,
    pub(crate) flush_interval: Duration,
}

impl Writer {
    /// Hauptschleife bis Shutdown-Signal oder bis alle Sender weg sind; leert danach den
    /// Kanal vollständig.
    pub(crate) async fn run(
        self,
        mut rx: mpsc::Receiver<RequestRecord>,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let batch_size = self.batch_size.max(1);
        let mut buf: Vec<RequestRecord> = Vec::with_capacity(batch_size);
        let mut tick = tokio::time::interval(self.flush_interval.max(Duration::from_millis(1)));
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        tick.tick().await; // erster Tick kommt sofort

        loop {
            let room = batch_size - buf.len(); // > 0: volle Puffer werden sofort geschrieben
            tokio::select! {
                biased;
                _ = shutdown.changed() => break,
                n = rx.recv_many(&mut buf, room) => {
                    if n == 0 {
                        break; // alle Sender geschlossen
                    }
                    if buf.len() >= batch_size {
                        self.flush(&mut buf, false).await;
                        tick.reset();
                    }
                }
                _ = tick.tick() => {
                    if !buf.is_empty() {
                        self.flush(&mut buf, false).await;
                    }
                }
            }
        }

        // Graceful Shutdown: keine neuen Einträge mehr annehmen (zählen als verworfen),
        // Vorhandenes vollständig schreiben.
        rx.close();
        while let Some(r) = rx.recv().await {
            buf.push(r);
            if buf.len() >= batch_size {
                self.flush(&mut buf, true).await;
            }
        }
        self.flush(&mut buf, true).await;
        for e in &self.exporters {
            if let Err(err) = e.flush().await {
                tracing::warn!(exporter = e.name(), error = %err, "Exporter-Flush fehlgeschlagen");
            }
        }
    }

    /// Verarbeitet und schreibt `buf` (danach leer).
    async fn flush(&self, buf: &mut Vec<RequestRecord>, shutting_down: bool) {
        if buf.is_empty() {
            return;
        }
        let records = std::mem::replace(buf, Vec::with_capacity(self.batch_size.max(1)));
        let n = records.len();
        // Maskierung/Serialisierung ist CPU-Arbeit: nicht auf einem Runtime-Worker.
        let processor = self.processor.clone();
        let batch = match tokio::task::spawn_blocking(move || processor.process(records)).await {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(error = %e, "Aufbereitung des Request-Logs fehlgeschlagen");
                ObserveMetrics::add(&self.metrics.write_failed, n);
                return;
            }
        };

        if let Some(store) = &self.store
            && !self.write_store(store, &batch, shutting_down).await
        {
            ObserveMetrics::add(&self.metrics.write_failed, n);
            return;
        }
        for e in &self.exporters {
            if let Err(err) = e.export(&batch.entries).await {
                ObserveMetrics::add(&self.metrics.export_errors, 1u64);
                tracing::warn!(exporter = e.name(), error = %err, entries = n, "Export fehlgeschlagen");
            }
        }
        ObserveMetrics::add(&self.metrics.written, n);
        ObserveMetrics::add(&self.metrics.batches, 1u64);
    }

    /// Schreibt den Batch mit Wiederholungen; `false`, wenn er verloren ist.
    async fn write_store(&self, store: &Store, batch: &Batch, shutting_down: bool) -> bool {
        let (logs, payloads): (Vec<&RequestLogRow>, Vec<&PayloadRow>) = if self.store_request_log {
            (
                batch.entries.iter().map(|e| &e.log).collect(),
                batch
                    .entries
                    .iter()
                    .filter_map(|e| e.payload.as_ref())
                    .collect(),
            )
        } else {
            (Vec::new(), Vec::new())
        };
        let attempts = if shutting_down { 1 } else { WRITE_ATTEMPTS };
        let mut delay = RETRY_BASE;
        // F34: eine ID je Batch, gleich für alle Wiederholungen – war ein Commit trotz Fehler
        // erfolgreich, wendet die Datenbank den Batch kein zweites Mal an.
        let batch_id = aigw_core::resource::new_id();
        for attempt in 1..=attempts {
            match store
                .write_request_batch_once(batch_id, &logs, &payloads, &batch.rollups)
                .await
            {
                Ok(applied) => {
                    if !applied {
                        tracing::debug!(%batch_id, "Request-Log: Batch war bereits angewendet");
                    }
                    return true;
                }
                Err(err) if attempt < attempts => {
                    tracing::warn!(error = %err, attempt, "Request-Log: Schreiben fehlgeschlagen, wiederhole");
                    tokio::time::sleep(delay).await;
                    delay = delay.saturating_mul(2);
                }
                Err(err) => {
                    tracing::error!(error = %err, entries = batch.entries.len(), "Request-Log: Batch verworfen");
                }
            }
        }
        false
    }
}
