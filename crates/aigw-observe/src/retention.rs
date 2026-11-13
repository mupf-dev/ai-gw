// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Aufbewahrung: periodisches Löschen von Rohlogs, Payloads und Rollups nach Frist.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aigw_core::config::RetentionConfig;
use aigw_store::{Store, StoreError};
use tokio::sync::watch;

use crate::metrics::ObserveMetrics;

const DAY_MS: i64 = 86_400_000;

/// Ergebnis eines Löschlaufs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepResult {
    /// Gelöschte Rohlogs.
    pub request_logs: u64,
    /// Gelöschte Payloads.
    pub payloads: u64,
    /// Gelöschte Rollups.
    pub rollups: u64,
}

fn cutoff(now_ms: i64, days: u32) -> Option<i64> {
    (days > 0).then(|| now_ms.saturating_sub(i64::from(days) * DAY_MS))
}

/// Ein Löschlauf zum Zeitpunkt `now_ms`. Payloads leben höchstens so lange wie Rohlogs.
pub async fn sweep(
    store: &Store,
    cfg: &RetentionConfig,
    now_ms: i64,
) -> Result<SweepResult, StoreError> {
    let log_cut = cutoff(now_ms, cfg.request_log_days);
    let payload_cut = match (cutoff(now_ms, cfg.payload_days), log_cut) {
        (Some(p), Some(l)) => Some(p.max(l)),
        (p, l) => p.or(l),
    };
    let mut res = SweepResult::default();
    if let Some(c) = payload_cut {
        res.payloads = store.purge_request_payloads(c).await?;
    }
    if let Some(c) = log_cut {
        res.request_logs = store.purge_request_logs(c).await?;
    }
    if let Some(c) = cutoff(now_ms, cfg.rollup_days) {
        res.rollups = store.purge_rollups(c).await?;
    }
    Ok(res)
}

/// Hintergrund-Task: erster Lauf sofort, danach alle `sweep_interval_secs`.
pub(crate) async fn run(
    store: Store,
    cfg: RetentionConfig,
    metrics: Arc<ObserveMetrics>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(cfg.sweep_interval_secs.max(1)));
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => return,
            _ = tick.tick() => {}
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        match sweep(&store, &cfg, now).await {
            Ok(r) => {
                ObserveMetrics::add(&metrics.purged, r.request_logs + r.payloads + r.rollups);
                if r != SweepResult::default() {
                    tracing::info!(
                        request_logs = r.request_logs,
                        payloads = r.payloads,
                        rollups = r.rollups,
                        "Aufbewahrung: abgelaufene Einträge gelöscht"
                    );
                }
            }
            Err(err) => tracing::warn!(error = %err, "Aufbewahrung: Löschlauf fehlgeschlagen"),
        }
    }
}
