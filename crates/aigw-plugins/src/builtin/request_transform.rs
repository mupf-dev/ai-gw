// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Plugin `request-transform`: Parameter der Anfrage entfernen, mit Standardwerten
//! belegen, überschreiben und deckeln.
//!
//! ```yaml
//! plugin: request-transform
//! config:
//!   remove: [seed]                     # 1. entfernen
//!   defaults: {temperature: 0.3}       # 2. setzen, falls der Client nichts angibt
//!   overrides: {topP: 1.0}             # 3. immer setzen (erzwingen)
//!   caps: {maxTokens: 1024}            # 4. deckeln; fehlendes maxTokens = Deckel
//!   rejectOverCap: false               # true: Überschreitung → 400 statt Deckeln
//! ```
//!
//! Felder: `maxTokens`, `temperature`, `topP`, `topK`, `frequencyPenalty`,
//! `presencePenalty`, `seed`, `parallelToolCalls`, `user`; Deckel für `maxTokens`,
//! `temperature`, `topP`. Das Modell lässt sich bewusst **nicht** setzen: Modellfreigabe und
//! Routing sind vor den Plugins entschieden (die Kette sperrt Modell-Umleitungen generell).

use std::sync::Arc;

use aigw_core::ir::{ApiError, ChatRequest};
use serde::Deserialize;
use serde_json::Value;

use crate::binding::FailMode;
use crate::plugin::{Flow, Hook, Phases, Plugin, ReqCtx};
use crate::registry::PluginFactory;

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Params {
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    top_p: Option<f64>,
    #[serde(default)]
    top_k: Option<u64>,
    #[serde(default)]
    frequency_penalty: Option<f64>,
    #[serde(default)]
    presence_penalty: Option<f64>,
    #[serde(default)]
    seed: Option<i64>,
    #[serde(default)]
    parallel_tool_calls: Option<bool>,
    #[serde(default)]
    user: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Caps {
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    top_p: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
enum Field {
    MaxTokens,
    Temperature,
    TopP,
    TopK,
    FrequencyPenalty,
    PresencePenalty,
    Seed,
    ParallelToolCalls,
    User,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Config {
    #[serde(default)]
    remove: Vec<Field>,
    #[serde(default)]
    defaults: Params,
    #[serde(default)]
    overrides: Params,
    #[serde(default)]
    caps: Caps,
    #[serde(default)]
    reject_over_cap: bool,
}

/// Plugin `request-transform`.
#[derive(Debug, Clone, Default)]
pub struct RequestTransform {
    config: Config,
}

fn check_float(what: &str, v: Option<f64>, max: f64) -> Result<(), String> {
    match v {
        Some(x) if !x.is_finite() || !(0.0..=max).contains(&x) => {
            Err(format!("{what}: Wert muss zwischen 0 und {max} liegen"))
        }
        _ => Ok(()),
    }
}

fn check_penalty(what: &str, v: Option<f64>) -> Result<(), String> {
    match v {
        Some(x) if !x.is_finite() || !(-2.0..=2.0).contains(&x) => {
            Err(format!("{what}: Wert muss zwischen -2 und 2 liegen"))
        }
        _ => Ok(()),
    }
}

fn check_params(prefix: &str, p: &Params) -> Result<(), String> {
    check_float(&format!("{prefix}.temperature"), p.temperature, 2.0)?;
    check_float(&format!("{prefix}.topP"), p.top_p, 1.0)?;
    check_penalty(&format!("{prefix}.frequencyPenalty"), p.frequency_penalty)?;
    check_penalty(&format!("{prefix}.presencePenalty"), p.presence_penalty)?;
    if p.max_tokens == Some(0) {
        return Err(format!("{prefix}.maxTokens muss > 0 sein"));
    }
    Ok(())
}

impl RequestTransform {
    fn from_config(config: Config) -> Result<Self, String> {
        check_params("defaults", &config.defaults)?;
        check_params("overrides", &config.overrides)?;
        check_float("caps.temperature", config.caps.temperature, 2.0)?;
        check_float("caps.topP", config.caps.top_p, 1.0)?;
        if config.caps.max_tokens == Some(0) {
            return Err("caps.maxTokens muss > 0 sein".to_owned());
        }
        Ok(Self { config })
    }

    /// Wendet die Konfiguration auf `req` an.
    fn apply(&self, req: &mut ChatRequest) -> Result<(), ApiError> {
        let c = &self.config;
        for f in &c.remove {
            match f {
                Field::MaxTokens => req.max_tokens = None,
                Field::Temperature => req.sampling.temperature = None,
                Field::TopP => req.sampling.top_p = None,
                Field::TopK => req.sampling.top_k = None,
                Field::FrequencyPenalty => req.sampling.frequency_penalty = None,
                Field::PresencePenalty => req.sampling.presence_penalty = None,
                Field::Seed => req.sampling.seed = None,
                Field::ParallelToolCalls => req.parallel_tool_calls = None,
                Field::User => req.user = None,
            }
        }
        set(req, &c.defaults, false);
        set(req, &c.overrides, true);
        if let Some(cap) = c.caps.max_tokens {
            match req.max_tokens {
                Some(v) if v > cap && c.reject_over_cap => {
                    return Err(over_cap("max_tokens", &cap.to_string()));
                }
                Some(v) if v <= cap => {}
                _ => req.max_tokens = Some(cap),
            }
        }
        cap_float(
            &mut req.sampling.temperature,
            c.caps.temperature,
            c.reject_over_cap,
            "temperature",
        )?;
        cap_float(
            &mut req.sampling.top_p,
            c.caps.top_p,
            c.reject_over_cap,
            "top_p",
        )?;
        Ok(())
    }
}

fn over_cap(param: &str, cap: &str) -> ApiError {
    ApiError::invalid_request(format!("{param} überschreitet das erlaubte Maximum {cap}"))
        .with_param(param)
        .with_code("parameter_over_limit")
}

fn cap_float(
    slot: &mut Option<f64>,
    cap: Option<f64>,
    reject: bool,
    param: &str,
) -> Result<(), ApiError> {
    if let (Some(cap), Some(v)) = (cap, *slot)
        && v > cap
    {
        if reject {
            return Err(over_cap(param, &cap.to_string()));
        }
        *slot = Some(cap);
    }
    Ok(())
}

fn set(req: &mut ChatRequest, p: &Params, force: bool) {
    fn put<T: Clone>(slot: &mut Option<T>, v: Option<&T>, force: bool) {
        if let Some(v) = v
            && (force || slot.is_none())
        {
            *slot = Some(v.clone());
        }
    }
    put(&mut req.max_tokens, p.max_tokens.as_ref(), force);
    put(&mut req.sampling.temperature, p.temperature.as_ref(), force);
    put(&mut req.sampling.top_p, p.top_p.as_ref(), force);
    put(&mut req.sampling.top_k, p.top_k.as_ref(), force);
    put(
        &mut req.sampling.frequency_penalty,
        p.frequency_penalty.as_ref(),
        force,
    );
    put(
        &mut req.sampling.presence_penalty,
        p.presence_penalty.as_ref(),
        force,
    );
    put(&mut req.sampling.seed, p.seed.as_ref(), force);
    put(
        &mut req.parallel_tool_calls,
        p.parallel_tool_calls.as_ref(),
        force,
    );
    put(&mut req.user, p.user.as_ref(), force);
}

impl Plugin for RequestTransform {
    fn name(&self) -> &'static str {
        "request-transform"
    }

    fn phases(&self) -> Phases {
        Phases::REQUEST
    }

    fn on_request<'a>(&'a self, ctx: &'a mut ReqCtx<'_>) -> Hook<'a, Flow> {
        Hook::ok(match self.apply(&mut ctx.request) {
            Ok(()) => Flow::Continue,
            Err(e) => Flow::Reject(e),
        })
    }
}

/// Fabrik von [`RequestTransform`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Factory;

impl PluginFactory for Factory {
    fn name(&self) -> &'static str {
        "request-transform"
    }

    fn build(&self, config: &Value) -> Result<Arc<dyn Plugin>, String> {
        let c: Config = serde_json::from_value(config.clone()).map_err(|e| e.to_string())?;
        Ok(Arc::new(RequestTransform::from_config(c)?))
    }

    fn default_fail_mode(&self) -> FailMode {
        FailMode::Closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plugin(v: Value) -> RequestTransform {
        let c: Config = serde_json::from_value(v).unwrap_or_default();
        RequestTransform::from_config(c).unwrap_or_default()
    }

    fn req() -> ChatRequest {
        let mut r = ChatRequest {
            model: "m".into(),
            ..ChatRequest::default()
        };
        r.max_tokens = Some(5000);
        r.sampling.temperature = Some(1.5);
        r.sampling.seed = Some(7);
        r
    }

    #[test]
    fn remove_defaults_overrides_caps() {
        let p = plugin(json!({
            "remove": ["seed"],
            "defaults": {"topP": 0.9, "temperature": 0.1},
            "overrides": {"user": "gateway"},
            "caps": {"maxTokens": 1024, "temperature": 1.0}
        }));
        let mut r = req();
        assert!(p.apply(&mut r).is_ok());
        assert_eq!(r.sampling.seed, None);
        assert_eq!(r.sampling.top_p, Some(0.9));
        // Default greift nicht, wenn der Client einen Wert hat; der Deckel schon.
        assert_eq!(r.sampling.temperature, Some(1.0));
        assert_eq!(r.max_tokens, Some(1024));
        assert_eq!(r.user.as_deref(), Some("gateway"));

        // Fehlendes max_tokens wird auf den Deckel gesetzt.
        let mut r = req();
        r.max_tokens = None;
        assert!(p.apply(&mut r).is_ok());
        assert_eq!(r.max_tokens, Some(1024));
    }

    #[test]
    fn reject_over_cap() {
        let p = plugin(json!({"caps": {"maxTokens": 100}, "rejectOverCap": true}));
        let mut r = req();
        let err = p.apply(&mut r).err();
        assert_eq!(
            err.and_then(|e| e.code),
            Some("parameter_over_limit".to_owned())
        );
    }

    #[test]
    fn invalid_config_is_rejected() {
        let f = Factory;
        assert!(f.build(&json!({"overrides": {"model": "x"}})).is_err());
        assert!(f.build(&json!({"defaults": {"temperature": 7}})).is_err());
        assert!(f.build(&json!({"caps": {"maxTokens": 0}})).is_err());
        assert!(f.build(&json!({"remove": ["model"]})).is_err());
        assert!(f.build(&json!({"unknown": 1})).is_err());
        assert!(f.build(&json!({})).is_ok());
    }
}
