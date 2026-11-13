// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Aufbereitung eines Batches: [`RequestRecord`] → Zeilen, Payload (maskiert), Rollup-Deltas.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aigw_core::observe::{PayloadMode, RequestRecord};
use aigw_core::wire::WireFormat;
use aigw_store::observe::{PayloadRow, RequestLogRow, RollupCounts, RollupKey, latency_bucket};

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
    pub(crate) payload_mode: PayloadMode,
    pub(crate) payload_max_bytes: usize,
    pub(crate) masker: Arc<dyn PayloadMasker>,
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
    }
}

fn error_code(r: &RequestRecord) -> Option<String> {
    r.error_kind.and_then(|k| {
        serde_json::to_value(k)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
    })
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
            ..RollupCounts::default()
        };
        if let Some(slot) = c.latency_hist.get_mut(latency_bucket(log.latency_ms)) {
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
            let (key, delta) = Self::rollup(&r, &log);
            rollups.entry(key).or_default().merge(&delta);
            let payload = r.payload.as_deref().and_then(|raw| {
                build_payload(
                    raw,
                    self.payload_mode,
                    self.payload_max_bytes,
                    self.masker.as_ref(),
                )
                .map(|payload| PayloadRow {
                    id: log.id.clone(),
                    at: log.at,
                    mode: self.payload_mode.as_str().to_owned(),
                    payload,
                })
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
    use crate::mask::RegexMasker;

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
        let mut b = rec(119_999, 300, Some(team));
        b.status = 502;
        b.error_kind = Some(ErrorKind::BadGateway);
        b.payload = Some(Box::new(RawPayload {
            request_body: "mail: a@b.de".into(),
            ..RawPayload::default()
        }));
        let c = rec(120_000, 70_000, Some(team)); // nächste Minute
        let d = rec(60_500, 10, None); // anderer Schlüssel
        let p = Processor {
            payload_mode: PayloadMode::Masked,
            payload_max_bytes: 1024,
            masker: Arc::new(RegexMasker::new()),
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
    }
}
