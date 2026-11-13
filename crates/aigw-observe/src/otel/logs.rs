// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Request-Log als OTLP-Logs: ein `LogRecord` je Anfrage (`event_name = "aigw.request"`).
//!
//! Attribute nach GenAI- bzw. HTTP-Semantic-Conventions, wo es passende gibt
//! (`gen_ai.request.model`, `gen_ai.system`, `gen_ai.provider.name`, `gen_ai.usage.*`, `http.response.status_code`,
//! `error.type`, `client.address`), sonst unter `aigw.*`. Der Body ist der (bereits maskierte)
//! Payload als JSON-Text, wenn `observe.otlp.logs.include_payload` gesetzt ist.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use aigw_core::config::OtlpConfig;
use aigw_core::observe::{gen_ai_provider_name, gen_ai_system};
use aigw_core::types::ProviderKind;

use super::OtlpMetrics;
use super::http::{OtlpClient, OtlpConfigError, Signal};
use super::proto::{AnyValue, KeyValue, LogData, Origin, encode_logs};
use super::trace::now_unix_nano;
use crate::export::{ExportFuture, LogExporter};
use crate::process::LogEntry;

/// Exporter für OTLP-Logs.
#[derive(Debug, Clone)]
pub struct OtlpLogExporter {
    client: OtlpClient,
    origin: Origin,
    include_payload: bool,
    metrics: Arc<OtlpMetrics>,
}

impl OtlpLogExporter {
    /// Exporter nach `cfg` (`observe.otlp`). Benötigt eine Tokio-Runtime beim Export.
    pub fn new(cfg: &OtlpConfig, metrics: Arc<OtlpMetrics>) -> Result<Self, OtlpConfigError> {
        Ok(Self {
            client: OtlpClient::new(cfg, Arc::clone(&metrics))?,
            origin: super::origin(cfg),
            include_payload: cfg.logs.include_payload,
            metrics,
        })
    }
}

fn severity(status: i64) -> (i32, &'static str) {
    match status {
        500.. => (17, "ERROR"),
        400..=499 => (13, "WARN"),
        _ => (9, "INFO"),
    }
}

fn to_log(e: &LogEntry, include_payload: bool, observed: u64) -> LogData {
    let l = &e.log;
    let mut a = Vec::with_capacity(28);
    a.push(KeyValue::str("aigw.request_id", l.id.clone()));
    a.push(KeyValue::str("aigw.endpoint", l.endpoint.clone()));
    a.push(KeyValue::str("gen_ai.operation.name", "chat"));
    let opt = |a: &mut Vec<KeyValue>, k: &str, v: &Option<String>| {
        if let Some(v) = v {
            a.push(KeyValue::str(k, v.clone()));
        }
    };
    opt(&mut a, "gen_ai.request.model", &l.model_requested);
    opt(&mut a, "aigw.model_group", &l.model_group);
    opt(&mut a, "aigw.deployment_id", &l.deployment_id);
    if let Some(p) = &l.provider {
        a.push(KeyValue::str("aigw.provider", p.clone()));
        if let Some(kind) = ProviderKind::ALL.iter().find(|k| k.as_str() == p) {
            a.push(KeyValue::str("gen_ai.system", gen_ai_system(*kind)));
            a.push(KeyValue::str(
                "gen_ai.provider.name",
                gen_ai_provider_name(*kind),
            ));
        }
    }
    opt(&mut a, "aigw.provider_model", &l.provider_model);
    a.push(KeyValue::int("http.response.status_code", l.status));
    opt(&mut a, "error.type", &l.error_code);
    opt(&mut a, "aigw.error_class", &l.error_class);
    opt(&mut a, "aigw.parent_request_id", &l.parent_request_id);
    if !l.guardrails.is_empty()
        && let Ok(g) = serde_json::to_string(&l.guardrails)
    {
        a.push(KeyValue::str("aigw.guardrails", g));
    }
    a.push(KeyValue::int("gen_ai.usage.input_tokens", l.input_tokens));
    a.push(KeyValue::int("gen_ai.usage.output_tokens", l.output_tokens));
    for (k, v) in [
        ("aigw.usage.cache_read_tokens", l.cached_tokens),
        ("aigw.usage.cache_write_tokens", l.cache_write_tokens),
        ("aigw.usage.reasoning_tokens", l.reasoning_tokens),
        ("aigw.cost_nanos", l.cost),
    ] {
        if v != 0 {
            a.push(KeyValue::int(k, v));
        }
    }
    a.push(KeyValue::bool("aigw.usage_estimated", l.usage_estimated));
    a.push(KeyValue::int("aigw.latency_ms", l.latency_ms));
    if let Some(t) = l.ttft_ms {
        a.push(KeyValue::int("aigw.ttft_ms", t));
    }
    opt(&mut a, "aigw.cache", &l.cache);
    a.push(KeyValue::bool("aigw.stream", l.stream));
    a.push(KeyValue::bool("aigw.client_aborted", l.client_aborted));
    opt(&mut a, "aigw.api_key_id", &l.api_key_id);
    opt(&mut a, "aigw.team_id", &l.team_id);
    opt(&mut a, "aigw.organization_id", &l.organization_id);
    opt(&mut a, "client.address", &l.client_ip);
    let (severity_number, severity_text) = severity(l.status);
    LogData {
        time_unix_nano: u64::try_from(l.at).unwrap_or(0).saturating_mul(1_000_000),
        observed_unix_nano: observed,
        severity_number,
        severity_text,
        body: include_payload
            .then_some(e.payload.as_ref())
            .flatten()
            .map(|p| AnyValue::Str(p.payload.to_string())),
        attributes: a,
        event_name: "aigw.request",
        trace_id: l.trace_id.as_deref().and_then(super::trace::unhex::<16>),
    }
}

impl LogExporter for OtlpLogExporter {
    fn name(&self) -> &str {
        "otlp"
    }

    fn export<'a>(&'a self, batch: &'a [LogEntry]) -> ExportFuture<'a> {
        Box::pin(async move {
            if batch.is_empty() {
                return Ok(());
            }
            let observed = now_unix_nano();
            let logs: Vec<LogData> = batch
                .iter()
                .map(|e| to_log(e, self.include_payload, observed))
                .collect();
            let body = encode_logs(&self.origin, &logs);
            match self.client.post(Signal::Logs, body).await {
                Ok(()) => {
                    self.metrics
                        .logs_exported
                        .fetch_add(logs.len() as u64, Ordering::Relaxed);
                    Ok(())
                }
                Err(e) => {
                    self.metrics
                        .log_export_errors
                        .fetch_add(1, Ordering::Relaxed);
                    Err(e)
                }
            }
        })
    }
}
