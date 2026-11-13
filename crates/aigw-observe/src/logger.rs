// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Hot-Path-Seite: [`RequestLogger`] implementiert [`RequestObserver`].

use std::sync::Arc;
use std::sync::atomic::Ordering;

use aigw_core::observe::{PayloadMode, RequestObserver, RequestRecord};
use tokio::sync::mpsc;

use crate::metrics::ObserveMetrics;

/// Gibt Einträge nicht-blockierend an den Writer weiter (`try_send` in einen begrenzten
/// Kanal). Ist er voll oder geschlossen, wird verworfen und `aigw_log_dropped_total` erhöht.
///
/// Klonen ist billig; jede Proxy-Instanz kann einen eigenen Klon halten.
#[derive(Debug, Clone)]
pub struct RequestLogger {
    tx: mpsc::Sender<RequestRecord>,
    metrics: Arc<ObserveMetrics>,
    payload_mode: PayloadMode,
}

impl RequestLogger {
    /// Logger über einen bestehenden Kanal (der Empfänger gehört dem Writer).
    pub fn new(
        tx: mpsc::Sender<RequestRecord>,
        metrics: Arc<ObserveMetrics>,
        payload_mode: PayloadMode,
    ) -> Self {
        Self {
            tx,
            metrics,
            payload_mode,
        }
    }

    /// Zähler.
    pub fn metrics(&self) -> &Arc<ObserveMetrics> {
        &self.metrics
    }
}

impl RequestObserver for RequestLogger {
    #[inline]
    fn record(&self, mut record: RequestRecord) {
        if !self.payload_mode.captures() {
            record.payload = None;
        }
        if self.tx.try_send(record).is_err() {
            self.metrics.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn payload_mode(&self) -> PayloadMode {
        self.payload_mode
    }
}
