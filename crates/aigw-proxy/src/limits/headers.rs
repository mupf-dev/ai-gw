// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

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

    #[cfg(test)]
    pub(crate) fn set_requests_for_test(&mut self, limit: u64, remaining: u64, reset: Duration) {
        self.requests = Some(Q {
            limit,
            remaining,
            reset,
        });
    }

    /// Setzt die Header passend zum Eingangsformat.
    pub(crate) fn apply(&self, format: ErrorFormat, h: &mut HeaderMap, now: SystemTime) {
        // Header-Namen als statische Konstanten, Zahlen ohne Zwischen-String (Hot Path,
        // AIGW-57).
        match format {
            ErrorFormat::OpenAi => {
                let mut tokens = None;
                for q in [self.tokens, self.input, self.output].into_iter().flatten() {
                    tighter(&mut tokens, q);
                }
                for (names, q) in [
                    (&OPENAI_NAMES[0], self.requests),
                    (&OPENAI_NAMES[1], tokens),
                ] {
                    let Some(q) = q else { continue };
                    h.insert(
                        HeaderName::from_static(names[0]),
                        HeaderValue::from(q.limit),
                    );
                    h.insert(
                        HeaderName::from_static(names[1]),
                        HeaderValue::from(q.remaining),
                    );
                    set(h, names[2], go_duration(q.reset));
                }
            }
            ErrorFormat::Anthropic => {
                for (names, q) in [
                    (&ANTHROPIC_NAMES[0], self.requests),
                    (&ANTHROPIC_NAMES[1], self.tokens),
                    (&ANTHROPIC_NAMES[2], self.input),
                    (&ANTHROPIC_NAMES[3], self.output),
                ] {
                    let Some(q) = q else { continue };
                    h.insert(
                        HeaderName::from_static(names[0]),
                        HeaderValue::from(q.limit),
                    );
                    h.insert(
                        HeaderName::from_static(names[1]),
                        HeaderValue::from(q.remaining),
                    );
                    if let Some(at) = rfc3339(now, q.reset) {
                        set(h, names[2], at);
                    }
                }
            }
        }
    }
}

/// OpenAI-Header je Größe: Limit, Rest, Rücksetzung (Anfragen, Tokens).
const OPENAI_NAMES: [[&str; 3]; 2] = [
    [
        "x-ratelimit-limit-requests",
        "x-ratelimit-remaining-requests",
        "x-ratelimit-reset-requests",
    ],
    [
        "x-ratelimit-limit-tokens",
        "x-ratelimit-remaining-tokens",
        "x-ratelimit-reset-tokens",
    ],
];

/// Anthropic-Header je Größe: Limit, Rest, Rücksetzung (Anfragen, Tokens, Eingabe, Ausgabe).
const ANTHROPIC_NAMES: [[&str; 3]; 4] = [
    [
        "anthropic-ratelimit-requests-limit",
        "anthropic-ratelimit-requests-remaining",
        "anthropic-ratelimit-requests-reset",
    ],
    [
        "anthropic-ratelimit-tokens-limit",
        "anthropic-ratelimit-tokens-remaining",
        "anthropic-ratelimit-tokens-reset",
    ],
    [
        "anthropic-ratelimit-input-tokens-limit",
        "anthropic-ratelimit-input-tokens-remaining",
        "anthropic-ratelimit-input-tokens-reset",
    ],
    [
        "anthropic-ratelimit-output-tokens-limit",
        "anthropic-ratelimit-output-tokens-remaining",
        "anthropic-ratelimit-output-tokens-reset",
    ],
];

fn set(h: &mut HeaderMap, name: &'static str, value: String) {
    if let Ok(v) = HeaderValue::try_from(value) {
        h.insert(HeaderName::from_static(name), v);
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
