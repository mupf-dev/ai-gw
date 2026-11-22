// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Aktive Health Checks (AIGW-26, optional je Modellgruppe über `settings.healthCheck`).
//!
//! Je Deployment läuft im eingestellten Abstand ein leichter Chat-Request (`ping`,
//! `max_tokens: 1`, nicht gestreamt) über denselben Adapter wie echte Anfragen. Er zählt
//! weder als offene Anfrage noch für RPM/TPM.
//!
//! - Fehlschlag (Fehlerklasse, die auf das Deployment zeigt, oder Zeitlimit): nach
//!   `unhealthyThreshold` aufeinanderfolgenden Fehlschlägen wird der Breaker geöffnet.
//! - Erfolg: Zähler zurücksetzen; ist der Breaker offen/halboffen, wird er geschlossen (der
//!   Health Check ersetzt dann die Probe).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use aigw_core::ir::{ChatRequest, Message, Role};

use super::exec::breaker_failure;
use super::state::{duration_ms, now_ms};
use super::{BreakerConfig, BreakerState, HealthCheckConfig, Member, RouteTable};

/// Startet fällige Health Checks (nicht blockierend, je Prüfung eine Task).
pub(crate) fn run_due(table: &RouteTable) {
    let now = now_ms();
    for group in table.unique_groups() {
        let Some(cfg) = group.policy().health else {
            continue;
        };
        // Der Health Check ist ein Chat-Request; reine Embedding-/Bild-/Audio-Gruppen
        // werden nicht geprüft (AIGW-21).
        if !group
            .policy()
            .capabilities
            .contains(aigw_providers::Capability::Chat)
        {
            continue;
        }
        let breaker = group.policy().breaker;
        for m in group.members() {
            let last = m.state.last_health_ms.load(Ordering::Acquire);
            if last != 0 && now.saturating_sub(last) < duration_ms(cfg.interval) {
                continue;
            }
            if m.state
                .last_health_ms
                .compare_exchange(last, now.max(1), Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                continue;
            }
            tokio::spawn(probe(Arc::clone(m), group.name().to_owned(), cfg, breaker));
        }
    }
}

async fn probe(m: Arc<Member>, group: String, cfg: HealthCheckConfig, breaker: BreakerConfig) {
    let req = ChatRequest {
        model: group,
        messages: vec![Message::text(Role::User, "ping")],
        max_tokens: Some(1),
        stream: Some(false),
        ..ChatRequest::default()
    };
    let deployment = m.provider.describe().deployment.to_owned();
    let failure = match tokio::time::timeout(cfg.timeout, Arc::clone(&m.provider).chat(req)).await {
        Err(_) => Some("Zeitlimit".to_owned()),
        Ok(Ok(_)) => None,
        Ok(Err(e)) => (breaker_failure(e.class) == Some(true)).then(|| e.to_string()),
    };
    match failure {
        Some(reason) => {
            let n = m.state.health_failures.fetch_add(1, Ordering::AcqRel) + 1;
            tracing::warn!(deployment = %deployment, failures = n, reason = %reason, "Health Check fehlgeschlagen");
            if n >= cfg.unhealthy_threshold
                && breaker.enabled
                && m.state.breaker.state() == BreakerState::Closed
            {
                m.state.breaker.open(&breaker, now_ms());
            }
        }
        None => {
            m.state.health_failures.store(0, Ordering::Release);
            if m.state.breaker.state() != BreakerState::Closed {
                tracing::info!(deployment = %deployment, "Health Check erfolgreich – Breaker geschlossen");
                m.state.breaker.close();
            }
        }
    }
}
