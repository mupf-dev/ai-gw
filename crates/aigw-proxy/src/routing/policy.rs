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
//!   capabilities: [chat]      # AIGW-21: chat, embeddings, images, audio-transcription,
//!                             # audio-speech; ohne Angabe nur chat
//!   fallbackIgnoresGrants: false  # F80: Fallbacks auch auf für Key/Team nicht freigegebene Gruppen
//!   responseModel: upstream   # F49: upstream | requested (Modellname im Antwort-Body)
//!   clientBetas: allow-all    # F60: allow-all | allowlist | deny (anthropic-beta der Clients)
//!   betaAllowlist: []         # F60: erlaubte Kennungen bei clientBetas: allowlist
//!   exposeReasoning: true     # F10: Reasoning an OpenAI-Clients (reasoning_content)
//!   # Von den Deployments geerbt (dort überschreibbar), geprüft wie bei Deployments:
//!   timeouts: {connectMs: 10000, firstByteMs: 120000, idleMs: 120000, totalMs: 600000}  # F44
//!   thinkingBudgetPolicy: raise-max-tokens   # F59: raise-max-tokens | cap-budget
//!   propagateTraceContext: false             # F98: Standard je Anbieterart
//! ```

use std::sync::Arc;
use std::time::Duration;

use aigw_core::resource::JsonObject;
use aigw_providers::{Capability, CapabilitySet};
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

/// Modellname im Antwort-Body (`settings.responseModel`, F49).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResponseModel {
    /// Wie vom Anbieter gemeldet (Standard).
    #[default]
    Upstream,
    /// Der vom Client angefragte Name (Modellgruppe bzw. Alias).
    Requested,
}

/// Umgang mit `anthropic-beta` der Clients (`settings.clientBetas`, F60).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ClientBetas {
    /// Alle Kennungen weitergeben (Standard).
    #[default]
    AllowAll,
    /// Nur Kennungen aus `settings.betaAllowlist`; andere werden verworfen.
    Allowlist(Arc<[String]>),
    /// Keine Kennungen des Clients (nur die des Deployments).
    Deny,
}

impl ClientBetas {
    /// Filtert die Kennungen des Clients; liefert die verworfenen.
    pub fn filter(&self, betas: &mut Vec<String>) -> Vec<String> {
        match self {
            Self::AllowAll => Vec::new(),
            Self::Deny => std::mem::take(betas),
            Self::Allowlist(allowed) => {
                let (keep, drop): (Vec<String>, Vec<String>) = std::mem::take(betas)
                    .into_iter()
                    .partition(|b| allowed.contains(b));
                *betas = keep;
                drop
            }
        }
    }
}

/// Alle Routing-Einstellungen einer Modellgruppe.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupPolicy {
    /// Retries.
    pub retry: RetryPolicy,
    /// Circuit Breaker.
    pub breaker: BreakerConfig,
    /// Health Checks (`None` = aus).
    pub health: Option<HealthCheckConfig>,
    /// Arten von Anfragen, die die Gruppe annimmt (`settings.capabilities`, AIGW-21;
    /// Standard nur Chat). Anfragen anderer Art → `400`.
    pub capabilities: CapabilitySet,
    /// Fallback-Kette dieser Gruppe auch auf Gruppen, die für Key/Team nicht freigegeben sind
    /// (`settings.fallbackIgnoresGrants`, F80; Standard: überspringen).
    pub fallback_ignores_grants: bool,
    /// Modellname im Antwort-Body (F49).
    pub response_model: ResponseModel,
    /// `anthropic-beta` der Clients (F60).
    pub client_betas: ClientBetas,
    /// Reasoning an OpenAI-Clients ausgeben (`settings.exposeReasoning`, F10; Standard an).
    pub expose_reasoning: bool,
}

impl Default for GroupPolicy {
    fn default() -> Self {
        Self {
            retry: RetryPolicy::default(),
            breaker: BreakerConfig::default(),
            health: None,
            capabilities: CapabilitySet::CHAT,
            fallback_ignores_grants: false,
            response_model: ResponseModel::Upstream,
            client_betas: ClientBetas::AllowAll,
            expose_reasoning: true,
        }
    }
}

impl GroupPolicy {
    /// Ohne Retries, Breaker und Health Checks, alle Arten von Anfragen (feste Routen,
    /// Tests, eingebauter Mock).
    pub fn passthrough() -> Self {
        Self {
            retry: RetryPolicy::none(),
            breaker: BreakerConfig::disabled(),
            health: None,
            capabilities: CapabilitySet::ALL,
            ..Self::default()
        }
    }

    /// Liest `retry`, `circuitBreaker`, `healthCheck` und `capabilities` aus den
    /// Einstellungen einer Modellgruppe. Andere Schlüssel werden ignoriert, unbekannte Felder
    /// in diesen Abschnitten sind Fehler (Tippfehler sollen auffallen). Auch für die Prüfung
    /// in der Admin-API gedacht.
    pub fn from_settings(settings: &JsonObject) -> Result<Self, String> {
        let mut p = Self::default();
        // Von den Deployments geerbte Einstellungen (F44/F59/F98) wie bei Deployments prüfen.
        aigw_core::validate::deployment_settings(settings)?;
        if let Some(b) = section::<bool>(settings, "fallbackIgnoresGrants")? {
            p.fallback_ignores_grants = b;
        }
        if let Some(b) = section::<bool>(settings, "exposeReasoning")? {
            p.expose_reasoning = b;
        }
        // F192: `settings.tokenizer` schon beim Speichern prüfen (sonst erst zur Laufzeit mit
        // Rückfall auf die automatische Zuordnung).
        if let Some(t) = section::<String>(settings, "tokenizer")? {
            aigw_tokenizer::TokenizerSetting::parse(&t)?;
        }
        if let Some(m) = section::<String>(settings, "responseModel")? {
            p.response_model = match m.as_str() {
                "upstream" => ResponseModel::Upstream,
                "requested" => ResponseModel::Requested,
                other => {
                    return Err(format!(
                        "settings.responseModel {other:?} unbekannt (upstream, requested)"
                    ));
                }
            };
        }
        let allowlist = section::<Vec<String>>(settings, "betaAllowlist")?;
        if let Some(list) = &allowlist
            && let Some(bad) = list.iter().find(|b| !valid_beta(b))
        {
            return Err(format!("settings.betaAllowlist: {bad:?} ungültig"));
        }
        match section::<String>(settings, "clientBetas")?.as_deref() {
            None | Some("allow-all") => {
                if allowlist.is_some() {
                    return Err(
                        "settings.betaAllowlist wirkt nur mit clientBetas: allowlist".into(),
                    );
                }
            }
            Some("deny") => {
                if allowlist.is_some() {
                    return Err(
                        "settings.betaAllowlist wirkt nur mit clientBetas: allowlist".into(),
                    );
                }
                p.client_betas = ClientBetas::Deny;
            }
            Some("allowlist") => {
                p.client_betas = ClientBetas::Allowlist(allowlist.unwrap_or_default().into());
            }
            Some(other) => {
                return Err(format!(
                    "settings.clientBetas {other:?} unbekannt (allow-all, allowlist, deny)"
                ));
            }
        }
        if let Some(list) = section::<Vec<String>>(settings, "capabilities")? {
            p.capabilities = list
                .iter()
                .map(|c| {
                    Capability::parse(c).ok_or_else(|| {
                        format!(
                            "settings.capabilities: {c:?} unbekannt (chat, embeddings, images, \
                             audio-transcription, audio-speech)"
                        )
                    })
                })
                .collect::<Result<_, _>>()?;
            if p.capabilities == CapabilitySet::EMPTY {
                return Err("settings.capabilities darf nicht leer sein".into());
            }
        }
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

/// Zeichen einer `anthropic-beta`-Kennung.
fn valid_beta(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
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
        for bad in [
            json!({"capabilities": ["chat", "video"]}),
            json!({"capabilities": []}),
            json!({"capabilities": "chat"}),
        ] {
            assert!(
                GroupPolicy::from_settings(&obj(bad.clone())).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn data_plane_settings() {
        let p = GroupPolicy::from_settings(&obj(json!({
            "fallbackIgnoresGrants": true,
            "responseModel": "requested",
            "clientBetas": "allowlist",
            "betaAllowlist": ["prompt-caching-2024-07-31"],
            "exposeReasoning": false,
            "timeouts": {"connectMs": 2000, "totalMs": 30000},
            "thinkingBudgetPolicy": "cap-budget",
            "propagateTraceContext": true
        })))
        .unwrap();
        assert!(p.fallback_ignores_grants);
        assert_eq!(p.response_model, ResponseModel::Requested);
        assert!(!p.expose_reasoning);
        let mut betas = vec!["prompt-caching-2024-07-31".to_owned(), "x-1".to_owned()];
        assert_eq!(p.client_betas.filter(&mut betas), ["x-1"]);
        assert_eq!(betas, ["prompt-caching-2024-07-31"]);
        let mut betas = vec!["a".to_owned()];
        let deny = GroupPolicy::from_settings(&obj(json!({"clientBetas": "deny"}))).unwrap();
        assert_eq!(deny.client_betas.filter(&mut betas), ["a"]);
        assert!(betas.is_empty());
        let d = GroupPolicy::default();
        assert!(d.expose_reasoning && !d.fallback_ignores_grants);
        assert_eq!(d.response_model, ResponseModel::Upstream);
        for bad in [
            json!({"responseModel": "alias"}),
            json!({"clientBetas": "some"}),
            json!({"betaAllowlist": ["a"]}),
            json!({"clientBetas": "allowlist", "betaAllowlist": ["a b"]}),
            json!({"exposeReasoning": "nein"}),
            json!({"tokenizer": "o300k"}),
            json!({"tokenizer": "hf:../etc/passwd"}),
            json!({"tokenizer": 5}),
            json!({"fallbackIgnoresGrants": 1}),
            json!({"timeouts": {"connectMs": 0}}),
            json!({"timeouts": {"readMs": 10}}),
            json!({"timeouts": 5}),
            json!({"thinkingBudgetPolicy": "max"}),
            json!({"propagateTraceContext": "ja"}),
        ] {
            assert!(
                GroupPolicy::from_settings(&obj(bad.clone())).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn capabilities() {
        let p = GroupPolicy::from_settings(&JsonObject::new()).unwrap();
        assert!(p.capabilities.contains(Capability::Chat));
        assert!(!p.capabilities.contains(Capability::Embeddings));
        let p = GroupPolicy::from_settings(&obj(json!({
            "capabilities": ["embeddings", "audio-speech"]
        })))
        .unwrap();
        assert!(!p.capabilities.contains(Capability::Chat));
        assert!(p.capabilities.contains(Capability::Embeddings));
        assert!(p.capabilities.contains(Capability::AudioSpeech));
        assert!(
            GroupPolicy::passthrough()
                .capabilities
                .contains(Capability::Images)
        );
    }
}
