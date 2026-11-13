// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Rate-Limit-Header im Stil des Eingangsformats.
//!
//! | OpenAI (`/v1/chat/completions`) | Anthropic (`/v1/messages`) |
//! |---|---|
//! | `x-ratelimit-limit-requests` | `anthropic-ratelimit-requests-limit` |
//! | `x-ratelimit-remaining-requests` | `anthropic-ratelimit-requests-remaining` |
//! | `x-ratelimit-reset-requests` (Dauer, z. B. `6m0s`, `250ms`) | `anthropic-ratelimit-requests-reset` (RFC 3339) |
//! | `x-ratelimit-*-tokens` (knappstes Token-Limit) | `anthropic-ratelimit-tokens-*`, `-input-tokens-*`, `-output-tokens-*` |
//!
//! Gemeldet wird je Metrik das knappste Limit (kleinster Rest) über alle Ebenen. Bei einer
//! Ablehnung nur die abgelehnte Metrik (die übrigen wurden nicht gebucht); dort ist der Reset
//! die Wartezeit (`Retry-After`).

use std::fmt::Write as _;
use std::time::{Duration, SystemTime};

use aigw_counters::{Quota, Rejection};
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::plan::{Metric, Planned};
use crate::error::ErrorFormat;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Q {
    limit: u64,
    remaining: u64,
    reset: Duration,
}

/// Knappstes Limit je Metrik.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RateHeaders {
    requests: Option<Q>,
    tokens: Option<Q>,
    input: Option<Q>,
    output: Option<Q>,
}

fn tighter(slot: &mut Option<Q>, q: Q) {
    if slot.is_none_or(|s| q.remaining < s.remaining) {
        *slot = Some(q);
    }
}

impl RateHeaders {
    fn slot(&mut self, m: Metric) -> &mut Option<Q> {
        match m {
            Metric::Requests => &mut self.requests,
            Metric::Tokens => &mut self.tokens,
            Metric::InputTokens => &mut self.input,
            Metric::OutputTokens => &mut self.output,
        }
    }

    /// Nach erfolgreicher Reservierung (`quotas` in der Reihenfolge von `planned`).
    pub(crate) fn from_quotas(planned: &[Planned], quotas: &[Quota]) -> Self {
        let mut h = Self::default();
        for (p, q) in planned.iter().zip(quotas) {
            tighter(
                h.slot(p.metric),
                Q {
                    limit: q.limit,
                    remaining: q.remaining,
                    reset: q.reset_after,
                },
            );
        }
        h
    }

    /// Nach einer Ablehnung: nur die abgelehnte Metrik.
    pub(crate) fn from_rejection(planned: &[Planned], rej: &Rejection) -> Self {
        let mut h = Self::default();
        if let Some(p) = planned.get(rej.index) {
            *h.slot(p.metric) = Some(Q {
                limit: rej.limit,
                remaining: rej.remaining(),
                reset: rej.retry_after.unwrap_or_else(|| rej.window.length()),
            });
        }
        h
    }

    /// `true`, wenn kein Limit gilt.
    pub(crate) fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Setzt die Header passend zum Eingangsformat.
    pub(crate) fn apply(&self, format: ErrorFormat, h: &mut HeaderMap, now: SystemTime) {
        match format {
            ErrorFormat::OpenAi => {
                let mut tokens = None;
                for q in [self.tokens, self.input, self.output].into_iter().flatten() {
                    tighter(&mut tokens, q);
                }
                for (name, q) in [("requests", self.requests), ("tokens", tokens)] {
                    let Some(q) = q else { continue };
                    set(h, &format!("x-ratelimit-limit-{name}"), q.limit.to_string());
                    set(
                        h,
                        &format!("x-ratelimit-remaining-{name}"),
                        q.remaining.to_string(),
                    );
                    set(
                        h,
                        &format!("x-ratelimit-reset-{name}"),
                        go_duration(q.reset),
                    );
                }
            }
            ErrorFormat::Anthropic => {
                for (name, q) in [
                    ("requests", self.requests),
                    ("tokens", self.tokens),
                    ("input-tokens", self.input),
                    ("output-tokens", self.output),
                ] {
                    let Some(q) = q else { continue };
                    let p = format!("anthropic-ratelimit-{name}");
                    set(h, &format!("{p}-limit"), q.limit.to_string());
                    set(h, &format!("{p}-remaining"), q.remaining.to_string());
                    if let Some(at) = rfc3339(now, q.reset) {
                        set(h, &format!("{p}-reset"), at);
                    }
                }
            }
        }
    }
}

fn set(h: &mut HeaderMap, name: &str, value: String) {
    if let (Ok(n), Ok(v)) = (
        HeaderName::from_bytes(name.as_bytes()),
        HeaderValue::from_str(&value),
    ) {
        h.insert(n, v);
    }
}

/// Dauer im Stil der OpenAI-Header (Go-`time.Duration`): `250ms`, `1.5s`, `6m0s`, `1h2m3s`.
pub(crate) fn go_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let (h, m) = (ms / 3_600_000, (ms / 60_000) % 60);
    let (s, frac) = ((ms % 60_000) / 1000, ms % 1000);
    let mut out = String::new();
    if h > 0 {
        let _ = write!(out, "{h}h");
    }
    if h > 0 || m > 0 {
        let _ = write!(out, "{m}m");
    }
    let _ = write!(out, "{s}");
    if frac > 0 {
        let f = format!("{frac:03}");
        let _ = write!(out, ".{}", f.trim_end_matches('0'));
    }
    out.push('s');
    out
}

/// Zeitpunkt `now + after` als RFC 3339 (ganze Sekunden, aufgerundet).
fn rfc3339(now: SystemTime, after: Duration) -> Option<String> {
    let at = OffsetDateTime::from(now + after);
    let at = if at.nanosecond() > 0 {
        at.replace_nanosecond(0).ok()? + time::Duration::SECOND
    } else {
        at
    };
    at.format(&Rfc3339).ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn go_durations() {
        let ms = Duration::from_millis;
        assert_eq!(go_duration(ms(0)), "0ms");
        assert_eq!(go_duration(ms(250)), "250ms");
        assert_eq!(go_duration(ms(1000)), "1s");
        assert_eq!(go_duration(ms(1500)), "1.5s");
        assert_eq!(go_duration(ms(59_320)), "59.32s");
        assert_eq!(go_duration(ms(360_000)), "6m0s");
        assert_eq!(go_duration(ms(3_723_000)), "1h2m3s");
    }

    #[test]
    fn rfc3339_rounds_up() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_250);
        assert_eq!(
            rfc3339(now, Duration::from_secs(10)).unwrap(),
            "2023-11-14T22:13:31Z"
        );
    }
}
