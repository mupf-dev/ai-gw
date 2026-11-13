// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Routing-Einstellungen einer Modellgruppe (`ModelGroup.spec.settings`, camelCase).
//!
//! ```yaml
//! settings:
//!   retry:            # AIGW-25
//!     maxAttempts: 3          # Versuche je Modellgruppe inkl. des ersten (1 = keine Retries)
//!     initialBackoffMs: 100   # Backoff vor erneutem Versuch auf demselben Deployment
//!     maxBackoffMs: 2000
//!     budgetMs: 30000         # Gesamtbudget für neue Versuche inkl. Fallbacks
//!     maxRetryAfterMs: 10000  # längeres Retry-After → nicht warten, sondern aufgeben/Fallback
//!   circuitBreaker:   # AIGW-26
//!     enabled: true
//!     failureRate: 0.5        # Fehlerquote im Fenster, ab der der Breaker öffnet
//!     minRequests: 5          # Mindestanzahl Anfragen im Fenster
//!     windowSecs: 30          # gleitendes Fenster
//!     openSecs: 30            # Offen-Dauer, danach eine Probe (halboffen)
//!   healthCheck:      # optional, AIGW-26
//!     intervalSecs: 30
//!     timeoutMs: 5000
//!     unhealthyThreshold: 2   # aufeinanderfolgende Fehlschläge → Breaker offen
//! ```

use std::time::Duration;

use aigw_core::resource::JsonObject;
use serde::Deserialize;
use serde::de::DeserializeOwned;

/// Retries und Backoff (AIGW-25).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Versuche je Modellgruppe inkl. des ersten (≥ 1).
    pub max_attempts: u32,
    /// Basis des exponentiellen Backoffs.
    pub initial_backoff: Duration,
    /// Obergrenze des Backoffs.
    pub max_backoff: Duration,
    /// Gesamtbudget: nach Ablauf wird kein neuer Versuch (auch kein Fallback) gestartet.
    pub budget: Duration,
    /// Längstes `Retry-After`, auf das gewartet wird.
    pub max_retry_after: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(2),
            budget: Duration::from_secs(30),
            max_retry_after: Duration::from_secs(10),
        }
    }
}

impl RetryPolicy {
    /// Genau ein Versuch, kein Warten.
    pub fn none() -> Self {
        Self {
            max_attempts: 1,
            ..Self::default()
        }
    }
}

/// Circuit Breaker je Deployment (AIGW-26).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BreakerConfig {
    /// Aktiv.
    pub enabled: bool,
    /// Fehlerquote (0–1), ab der der Breaker öffnet.
    pub failure_rate: f64,
    /// Mindestanzahl Anfragen im Fenster.
    pub min_requests: u32,
    /// Länge des gleitenden Fensters (Sekunden).
    pub window_secs: u32,
    /// Offen-Dauer; danach lässt der Breaker eine Probe durch (halboffen).
    pub open_for: Duration,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            failure_rate: 0.5,
            min_requests: 5,
            window_secs: 30,
            open_for: Duration::from_secs(30),
        }
    }
}

impl BreakerConfig {
    /// Ausgeschaltet.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }
}

/// Aktive Health Checks (AIGW-26).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthCheckConfig {
    /// Abstand der Prüfungen je Deployment.
    pub interval: Duration,
    /// Zeitlimit einer Prüfung.
    pub timeout: Duration,
    /// Aufeinanderfolgende Fehlschläge, nach denen der Breaker geöffnet wird.
    pub unhealthy_threshold: u32,
}

/// Alle Routing-Einstellungen einer Modellgruppe.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct GroupPolicy {
    /// Retries.
    pub retry: RetryPolicy,
    /// Circuit Breaker.
    pub breaker: BreakerConfig,
    /// Health Checks (`None` = aus).
    pub health: Option<HealthCheckConfig>,
}

impl GroupPolicy {
    /// Ohne Retries, Breaker und Health Checks (feste Routen, Tests).
    pub fn passthrough() -> Self {
        Self {
            retry: RetryPolicy::none(),
            breaker: BreakerConfig::disabled(),
            health: None,
        }
    }

    /// Liest `retry`, `circuitBreaker` und `healthCheck` aus den Einstellungen einer
    /// Modellgruppe. Andere Schlüssel werden ignoriert, unbekannte Felder in diesen drei
    /// Abschnitten sind Fehler (Tippfehler sollen auffallen). Auch für die Prüfung in der
    /// Admin-API gedacht.
    pub fn from_settings(settings: &JsonObject) -> Result<Self, String> {
        let mut p = Self::default();
        if let Some(r) = section::<RawRetry>(settings, "retry")? {
            let d = RetryPolicy::default();
            p.retry = RetryPolicy {
                max_attempts: positive(r.max_attempts, "retry.maxAttempts")?
                    .unwrap_or(d.max_attempts),
                initial_backoff: r.initial_backoff_ms.map_or(d.initial_backoff, ms),
                max_backoff: r.max_backoff_ms.map_or(d.max_backoff, ms),
                budget: r.budget_ms.map_or(d.budget, ms),
                max_retry_after: r.max_retry_after_ms.map_or(d.max_retry_after, ms),
            };
            if p.retry.max_attempts > 20 {
                return Err("retry.maxAttempts darf höchstens 20 sein".into());
            }
        }
        if let Some(b) = section::<RawBreaker>(settings, "circuitBreaker")? {
            let d = BreakerConfig::default();
            let failure_rate = b.failure_rate.unwrap_or(d.failure_rate);
            if !(failure_rate > 0.0 && failure_rate <= 1.0) {
                return Err("circuitBreaker.failureRate muss in (0, 1] liegen".into());
            }
            let window_secs =
                positive(b.window_secs, "circuitBreaker.windowSecs")?.unwrap_or(d.window_secs);
            if window_secs > 3600 {
                return Err("circuitBreaker.windowSecs darf höchstens 3600 sein".into());
            }
            p.breaker = BreakerConfig {
                enabled: b.enabled.unwrap_or(true),
                failure_rate,
                min_requests: positive(b.min_requests, "circuitBreaker.minRequests")?
                    .unwrap_or(d.min_requests),
                window_secs,
                open_for: positive(b.open_secs, "circuitBreaker.openSecs")?
                    .map_or(d.open_for, |s| Duration::from_secs(u64::from(s))),
            };
        }
        if let Some(h) = section::<RawHealth>(settings, "healthCheck")?
            && h.enabled != Some(false)
        {
            let interval = positive(h.interval_secs, "healthCheck.intervalSecs")?.unwrap_or(30);
            p.health = Some(HealthCheckConfig {
                interval: Duration::from_secs(u64::from(interval)),
                timeout: h
                    .timeout_ms
                    .filter(|t| *t > 0)
                    .map_or(Duration::from_secs(5), ms),
                unhealthy_threshold: positive(
                    h.unhealthy_threshold,
                    "healthCheck.unhealthyThreshold",
                )?
                .unwrap_or(2),
            });
        }
        Ok(p)
    }
}

fn ms(v: u64) -> Duration {
    Duration::from_millis(v)
}

fn positive(v: Option<u32>, what: &str) -> Result<Option<u32>, String> {
    match v {
        Some(0) => Err(format!("{what} muss ≥ 1 sein")),
        v => Ok(v),
    }
}

fn section<T: DeserializeOwned>(settings: &JsonObject, key: &str) -> Result<Option<T>, String> {
    match settings.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => serde_json::from_value(v.clone())
            .map(Some)
            .map_err(|e| format!("settings.{key}: {e}")),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawRetry {
    max_attempts: Option<u32>,
    initial_backoff_ms: Option<u64>,
    max_backoff_ms: Option<u64>,
    budget_ms: Option<u64>,
    max_retry_after_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawBreaker {
    enabled: Option<bool>,
    failure_rate: Option<f64>,
    min_requests: Option<u32>,
    window_secs: Option<u32>,
    open_secs: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawHealth {
    enabled: Option<bool>,
    interval_secs: Option<u32>,
    timeout_ms: Option<u64>,
    unhealthy_threshold: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: serde_json::Value) -> JsonObject {
        match v {
            serde_json::Value::Object(o) => o,
            _ => JsonObject::new(),
        }
    }

    #[test]
    fn defaults_and_overrides() {
        let p = GroupPolicy::from_settings(&JsonObject::new()).unwrap();
        assert_eq!(p, GroupPolicy::default());
        assert_eq!(p.retry.max_attempts, 3);
        assert!(p.breaker.enabled);
        assert!(p.health.is_none());

        let p = GroupPolicy::from_settings(&obj(json!({
            "retry": {"maxAttempts": 5, "budgetMs": 1000},
            "circuitBreaker": {"failureRate": 0.25, "openSecs": 2},
            "healthCheck": {"intervalSecs": 10},
            "somethingElse": 1
        })))
        .unwrap();
        assert_eq!(p.retry.max_attempts, 5);
        assert_eq!(p.retry.budget, Duration::from_secs(1));
        assert_eq!(p.retry.initial_backoff, Duration::from_millis(100));
        assert!((p.breaker.failure_rate - 0.25).abs() < f64::EPSILON);
        assert_eq!(p.breaker.open_for, Duration::from_secs(2));
        assert_eq!(p.health.unwrap().interval, Duration::from_secs(10));
    }

    #[test]
    fn rejects_typos_and_bad_values() {
        for bad in [
            json!({"retry": {"maxAttempt": 2}}),
            json!({"retry": {"maxAttempts": 0}}),
            json!({"retry": {"maxAttempts": 99}}),
            json!({"circuitBreaker": {"failureRate": 1.5}}),
            json!({"healthCheck": {"intervalSecs": 0}}),
            json!({"retry": "viel"}),
        ] {
            assert!(
                GroupPolicy::from_settings(&obj(bad.clone())).is_err(),
                "{bad}"
            );
        }
        let p = GroupPolicy::from_settings(&obj(json!({"healthCheck": {"enabled": false}})));
        assert!(p.unwrap().health.is_none());
    }
}
