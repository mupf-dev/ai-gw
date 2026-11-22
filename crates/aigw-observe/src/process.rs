// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Aufbereitung eines Batches: [`RequestRecord`] → Zeilen, Payload (maskiert), Rollup-Deltas.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aigw_core::observe::RequestRecord;
use aigw_core::wire::WireFormat;
use aigw_store::observe::{
    AttemptRow, GuardrailRow, PayloadRow, RequestLogRow, RollupCounts, RollupKey, latency_bucket,
};

use crate::mask::{PayloadMasker, build_payload};

/// Ein aufbereiteter Eintrag (Rohlog + optionaler Payload); Eingabe für Store und Exporter.
#[derive(Debug, Clone, PartialEq)]
pub struct LogEntry {
    /// Rohlog-Zeile.
    pub log: RequestLogRow,
    /// Payload (bereits gefiltert/maskiert), sofern konfiguriert.
    pub payload: Option<PayloadRow>,
}

/// Aufbereiteter Batch.
#[derive(Debug, Default)]
pub(crate) struct Batch {
    pub(crate) entries: Vec<LogEntry>,
    pub(crate) rollups: Vec<(RollupKey, RollupCounts)>,
}

/// Bereitet Batches auf (läuft im Writer, außerhalb des Hot Paths).
#[derive(Debug, Clone)]
pub(crate) struct Processor {
    pub(crate) payload_max_bytes: usize,
    pub(crate) masker: Arc<dyn PayloadMasker>,
    /// Rollups berechnen (`observe.rollups`, F33).
    pub(crate) rollups: bool,
}

const MINUTE_MS: i64 = 60_000;

fn unix_ms(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

fn tokens(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// Bezeichner des Eingangsformats (Spalte `request_log.endpoint`).
pub fn endpoint_name(f: WireFormat) -> &'static str {
    match f {
        WireFormat::OpenAiChat => "openai-chat",
        WireFormat::AnthropicMessages => "anthropic-messages",
        WireFormat::BedrockConverse => "bedrock-converse",
        WireFormat::Gemini => "gemini",
        WireFormat::OpenAiResponses => "openai-responses",
        WireFormat::OpenAiEmbeddings => "openai-embeddings",
        WireFormat::OpenAiImages => "openai-images",
        WireFormat::OpenAiAudio => "openai-audio",
        WireFormat::Mcp => "mcp",
    }
}

fn error_code(r: &RequestRecord) -> Option<String> {
    r.error_kind.and_then(|k| {
        serde_json::to_value(k)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
    })
}

/// Höchstlänge einer gespeicherten Client-Request-ID.
const CLIENT_REQUEST_ID_MAX: usize = 128;

/// Client-Request-ID für das Rohlog: nur sichtbares ASCII, gekürzt; leer → `None`.
fn client_request_id(v: &str) -> Option<String> {
    let s: String = v
        .trim()
        .chars()
        .filter(char::is_ascii_graphic)
        .take(CLIENT_REQUEST_ID_MAX)
        .collect();
    (!s.is_empty()).then_some(s)
}

impl Processor {
    /// Zeile für `request_log`.
    pub(crate) fn log_row(r: &RequestRecord) -> RequestLogRow {
        let u = &r.usage;
        RequestLogRow {
            id: r.request_id.clone(),
            at: unix_ms(r.started_at),
            endpoint: endpoint_name(r.ingress).to_owned(),
            api_key_id: r.api_key_id.map(|i| i.to_string()),
            team_id: r.team_id.map(|i| i.to_string()),
            organization_id: r.organization_id.map(|i| i.to_string()),
            model_requested: r.model_alias.clone(),
            model_group: r.model_group.clone(),
            deployment_id: r.deployment_id.map(|i| i.to_string()),
            provider: r.provider.map(|p| p.as_str().to_owned()),
            provider_model: r.provider_model.clone(),
            status: i64::from(r.status),
            error_code: error_code(r),
            input_tokens: tokens(u.input_tokens),
            output_tokens: tokens(u.output_tokens),
            cached_tokens: tokens(u.cache_read_tokens.unwrap_or(0)),
            cache_write_tokens: tokens(u.cache_write_tokens.unwrap_or(0)),
            reasoning_tokens: tokens(u.reasoning_tokens.unwrap_or(0)),
            usage_estimated: r.usage_estimated,
            cost: r.cost.map_or(0, |c| c.nanos()),
            latency_ms: ms(r.duration),
            ttft_ms: r.ttft.map(ms),
            cache: r.cache.map(|c| c.as_str().to_owned()),
            stream: r.stream,
            client_aborted: r.client_aborted,
            client_ip: r.client_ip.map(|ip| ip.to_string()),
            client_request_id: r.client_request_id.as_deref().and_then(client_request_id),
            attempts: r
                .attempts
                .iter()
                .map(|a| AttemptRow {
                    model_group: a.model_group.clone(),
                    deployment: a.deployment.clone(),
                    deployment_id: a.deployment_id.map(|i| i.to_string()),
                    error: a.error.map(|e| e.as_str().to_owned()),
                    upstream_status: a.upstream_status,
                    duration_ms: ms(a.duration),
                })
                .collect(),
            parent_request_id: r.parent_request_id.clone(),
            error_class: r.error_class.map(|c| c.as_str().to_owned()),
            trace_id: r.trace_id.clone(),
            guardrails: r
                .guardrails
                .iter()
                .map(|g| GuardrailRow {
                    plugin: g.plugin.to_owned(),
                    category: g.category.clone(),
                    action: g.action.to_owned(),
                    count: g.count,
                })
                .collect(),
            workload: r.workload.as_deref().cloned(),
        }
    }

    /// Rollup-Schlüssel und -Delta eines Eintrags.
    pub(crate) fn rollup(r: &RequestRecord, log: &RequestLogRow) -> (RollupKey, RollupCounts) {
        let key = RollupKey {
            bucket_start: log.at - log.at.rem_euclid(MINUTE_MS),
            api_key_id: log.api_key_id.clone().unwrap_or_default(),
            team_id: log.team_id.clone().unwrap_or_default(),
            organization_id: log.organization_id.clone().unwrap_or_default(),
            model_group: log.model_group.clone().unwrap_or_default(),
            deployment_id: log.deployment_id.clone().unwrap_or_default(),
            error_class: log.error_class.clone().unwrap_or_default(),
        };
        let mut c = RollupCounts {
            requests: 1,
            errors: i64::from(r.is_error()),
            cache_hits: i64::from(r.cache.is_some_and(|c| c.is_hit())),
            input_tokens: log.input_tokens,
            output_tokens: log.output_tokens,
            cached_tokens: log.cached_tokens,
            cache_write_tokens: log.cache_write_tokens,
            reasoning_tokens: log.reasoning_tokens,
            cost: log.cost,
            latency_ms_sum: log.latency_ms,
            estimated: i64::from(log.usage_estimated),
            aborted: i64::from(log.client_aborted),
            ttft_ms_sum: log.ttft_ms.unwrap_or(0),
            ttft_count: i64::from(log.ttft_ms.is_some()),
            // AIGW-117: Kosten nach Art, Plugin-Anteil und Ersparnis durch Prompt-Caching.
            cost_input: r.cost_breakdown.input.nanos(),
            cost_cache_read: r.cost_breakdown.cache_read.nanos(),
            cost_cache_write: r.cost_breakdown.cache_write.nanos(),
            cost_output: r.cost_breakdown.output.nanos(),
            cache_read_saved: r.cost_breakdown.cache_read_saved.nanos(),
            plugin_cost: r.plugin_cost.map_or(0, |c| c.nanos()),
            plugin_input_tokens: i64::try_from(r.plugin_usage.input_tokens).unwrap_or(i64::MAX),
            plugin_output_tokens: i64::try_from(r.plugin_usage.output_tokens).unwrap_or(i64::MAX),
            ..RollupCounts::default()
        };
        if let Some(slot) = c.latency_hist.get_mut(latency_bucket(log.latency_ms)) {
            *slot = 1;
        }
        if let Some(t) = log.ttft_ms
            && let Some(slot) = c.ttft_hist.get_mut(latency_bucket(t))
        {
            *slot = 1;
        }
        (key, c)
    }

    /// Bereitet einen Batch auf: Zeilen, Payloads, je Schlüssel aggregierte Rollups.
    pub(crate) fn process(&self, records: Vec<RequestRecord>) -> Batch {
        let mut rollups: HashMap<RollupKey, RollupCounts> = HashMap::new();
        let mut entries = Vec::with_capacity(records.len());
        for r in records {
            let log = Self::log_row(&r);
            if self.rollups {
                let (key, delta) = Self::rollup(&r, &log);
                rollups.entry(key).or_default().merge(&delta);
            }
            // Modus je Anfrage (F30: Key/Team/Organisation, sonst global).
            let payload = r.payload.as_deref().and_then(|raw| {
                build_payload(raw, raw.mode, self.payload_max_bytes, self.masker.as_ref()).map(
                    |payload| PayloadRow {
                        id: log.id.clone(),
                        at: log.at,
                        mode: raw.mode.as_str().to_owned(),
                        payload,
                    },
                )
            });
            entries.push(LogEntry { log, payload });
        }
        let mut rollups: Vec<_> = rollups.into_iter().collect();
        rollups.sort_by(|a, b| a.0.cmp(&b.0)); // feste Reihenfolge: weniger Deadlock-Risiko
        Batch { entries, rollups }
    }
}

#[cfg(test)]
mod tests {
    use aigw_core::ir::{ErrorKind, Usage};
    use aigw_core::observe::{CacheOutcome, RawPayload};
    use aigw_core::resource::new_id;

    use super::*;
    use crate::mask::PiiMasker;

    fn rec(at_ms: u64, latency: u64, team: Option<aigw_core::resource::Id>) -> RequestRecord {
        let mut r = RequestRecord::new(new_id().to_string(), WireFormat::OpenAiChat, 200);
        r.started_at = UNIX_EPOCH + Duration::from_millis(at_ms);
        r.duration = Duration::from_millis(latency);
        r.team_id = team;
        r.model_group = Some("gpt-4o".into());
        r.usage = Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: Some(2),
            ..Usage::default()
        };
        r
    }

    #[test]
    fn aggregates_per_minute_and_key() {
        let team = new_id();
        let mut a = rec(60_000, 10, Some(team));
        a.ttft = Some(Duration::from_millis(4));
        a.cache = Some(CacheOutcome::HitExact);
        // AIGW-117: Kosten-Aufschlüsselung und Plugin-Anteil landen in den Rollups.
        a.cost = Some(aigw_core::resource::Amount::from_nanos(100));
        a.cost_breakdown = aigw_core::observe::CostBreakdown {
            input: aigw_core::resource::Amount::from_nanos(30),
            cache_read: aigw_core::resource::Amount::from_nanos(5),
            cache_write: aigw_core::resource::Amount::ZERO,
            output: aigw_core::resource::Amount::from_nanos(50),
            cache_read_saved: aigw_core::resource::Amount::from_nanos(9),
        };
        a.plugin_cost = Some(aigw_core::resource::Amount::from_nanos(15));
        a.plugin_usage = Usage {
            input_tokens: 40,
            output_tokens: 3,
            ..Usage::default()
        };
        let mut b = rec(119_999, 300, Some(team));
        b.status = 502;
        b.error_kind = Some(ErrorKind::BadGateway);
        b.payload = Some(Box::new(RawPayload {
            request_body: "mail: a@b.de".into(),
            mode: aigw_core::observe::PayloadMode::Masked,
            ..RawPayload::default()
        }));
        let c = rec(120_000, 70_000, Some(team)); // nächste Minute
        let d = rec(60_500, 10, None); // anderer Schlüssel
        let p = Processor {
            payload_max_bytes: 1024,
            masker: Arc::new(PiiMasker::new()),
            rollups: true,
        };
        let batch = p.process(vec![a, b, c, d]);
        assert_eq!(batch.entries.len(), 4);
        assert_eq!(batch.rollups.len(), 3);
        let (k, v) = batch
            .rollups
            .iter()
            .find(|(k, _)| k.bucket_start == 60_000 && !k.team_id.is_empty())
            .unwrap();
        assert_eq!(k.model_group, "gpt-4o");
        assert_eq!(v.requests, 2);
        assert_eq!(v.errors, 1);
        assert_eq!(v.cache_hits, 1);
        assert_eq!(v.input_tokens, 20);
        assert_eq!(v.cached_tokens, 4);
        assert_eq!(v.latency_ms_sum, 310);
        assert_eq!(v.ttft_count, 1);
        assert_eq!(v.latency_hist[0], 1);
        assert_eq!(v.latency_hist[4], 1);
        assert_eq!(v.latency_hist.iter().sum::<i64>(), 2);
        assert_eq!(
            (v.cost, v.cost_input, v.cost_cache_read, v.cost_output),
            (100, 30, 5, 50)
        );
        assert_eq!((v.cache_read_saved, v.plugin_cost), (9, 15));
        assert_eq!((v.plugin_input_tokens, v.plugin_output_tokens), (40, 3));
        let late = batch
            .rollups
            .iter()
            .find(|(k, _)| k.bucket_start == 120_000)
            .unwrap();
        assert_eq!(late.1.latency_hist[11], 1);

        let e = &batch.entries[1];
        assert_eq!(e.log.error_code.as_deref(), Some("bad_gateway"));
        assert_eq!(e.log.cached_tokens, 2);
        let pl = e.payload.as_ref().unwrap();
        assert_eq!(pl.mode, "masked");
        assert_eq!(pl.payload["request"]["body"], "mail: [EMAIL]");
        assert!(batch.entries[0].payload.is_none());
        assert_eq!(e.log.error_class, None);
    }

    #[test]
    fn new_columns_and_rollup_switch() {
        use aigw_core::ir::ErrorClass;
        use aigw_core::observe::{GuardrailHit, PayloadMode};
        let mut a = rec(60_000, 10, None);
        a.status = 504;
        a.error_kind = Some(ErrorKind::BadGateway);
        a.error_class = Some(ErrorClass::Timeout);
        a.trace_id = Some("4bf92f3577b34da6a3ce929d0e0e4736".into());
        a.parent_request_id = Some(new_id().to_string());
        a.guardrails = vec![GuardrailHit {
            plugin: "pii",
            category: "email".into(),
            action: "mask",
            count: 2,
        }];
        // je Anfrage eigener Modus (F30): full ohne Maskierung
        a.payload = Some(Box::new(RawPayload {
            request_body: "mail: a@b.de".into(),
            mode: PayloadMode::Full,
            ..RawPayload::default()
        }));
        let p = Processor {
            payload_max_bytes: 1024,
            masker: Arc::new(PiiMasker::new()),
            rollups: true,
        };
        let b = p.process(vec![a.clone()]);
        let log = &b.entries[0].log;
        assert_eq!(log.error_class.as_deref(), Some("timeout"));
        assert_eq!(log.trace_id, a.trace_id);
        assert_eq!(log.parent_request_id, a.parent_request_id);
        assert_eq!(log.guardrails.len(), 1);
        assert_eq!(log.guardrails[0].count, 2);
        let pl = b.entries[0].payload.as_ref().unwrap();
        assert_eq!(pl.mode, "full");
        assert_eq!(pl.payload["request"]["body"], "mail: a@b.de");
        assert_eq!(b.rollups[0].0.error_class, "timeout");
        // F33: ohne Rollups
        let p = Processor {
            rollups: false,
            ..p
        };
        let b = p.process(vec![a]);
        assert!(b.rollups.is_empty());
        assert_eq!(b.entries.len(), 1);
    }
}
