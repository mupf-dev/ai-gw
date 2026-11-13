// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Hot-Path-Seite: [`RequestLogger`] implementiert [`RequestObserver`].

use std::sync::Arc;
use std::sync::atomic::Ordering;

use aigw_core::observe::{PayloadMode, RequestObserver, RequestRecord, effective_payload_mode};
use aigw_core::snapshot::KeyCtx;
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
        if record.payload.as_ref().is_some_and(|p| !p.mode.captures()) {
            record.payload = None;
        }
        if self.tx.try_send(record).is_err() {
            self.metrics.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// `payloadMode` an Key, Team oder Organisation, sonst `observe.payload` (F30).
    fn payload_mode(&self, key: Option<&KeyCtx>) -> PayloadMode {
        effective_payload_mode(key, self.payload_mode)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aigw_core::resource::{
        self as r, ApiKeySpec, ApiKeyStatus, Object, ObjectMeta, OrganizationSpec, TeamSpec, new_id,
    };

    use super::*;

    fn ctx(
        key: Option<PayloadMode>,
        team: Option<PayloadMode>,
        org: Option<PayloadMode>,
    ) -> KeyCtx {
        KeyCtx {
            key: r::ApiKey {
                id: new_id(),
                metadata: ObjectMeta::named("k"),
                spec: ApiKeySpec {
                    payload_mode: key,
                    ..ApiKeySpec::default()
                },
                status: ApiKeyStatus {
                    prefix: "x".into(),
                    state: r::ApiKeyState::Active,
                    last_used_at: None,
                },
            },
            team: Some(Arc::new(Object {
                id: new_id(),
                metadata: ObjectMeta::named("t"),
                spec: TeamSpec {
                    payload_mode: team,
                    ..TeamSpec::default()
                },
            })),
            organization: Some(Arc::new(Object {
                id: new_id(),
                metadata: ObjectMeta::named("o"),
                spec: OrganizationSpec {
                    payload_mode: org,
                    ..OrganizationSpec::default()
                },
            })),
            expires_at: None,
        }
    }

    #[test]
    fn payload_mode_inherits_key_team_org_global() {
        let (tx, _rx) = mpsc::channel(1);
        let l = RequestLogger::new(tx, Arc::default(), Metadata);
        use PayloadMode::{Full, Masked, Metadata, Off};
        assert_eq!(l.payload_mode(None), Metadata, "ohne Key: global");
        assert_eq!(l.payload_mode(Some(&ctx(None, None, None))), Metadata);
        assert_eq!(l.payload_mode(Some(&ctx(None, None, Some(Full)))), Full);
        assert_eq!(
            l.payload_mode(Some(&ctx(None, Some(Masked), Some(Full)))),
            Masked
        );
        assert_eq!(
            l.payload_mode(Some(&ctx(Some(Off), Some(Masked), Some(Full)))),
            Off
        );
    }
}
