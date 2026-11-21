// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Kosten je Anfrage aus der Preistabelle (Architektur §8).
//!
//! Die IR-Usage zählt **inklusive**: `input_tokens` enthält gecachte und in den Cache
//! geschriebene Tokens, `output_tokens` enthält Reasoning-Tokens (03 §2). Abgerechnet wird
//! daher getrennt:
//!
//! | Token-Art | Menge | Preis (fehlt → Ersatz) |
//! |---|---|---|
//! | Eingabe | `input − cache_read − cache_write` | `input` (→ 0) |
//! | Cache lesen | `cache_read` | `cachedInput` (→ `input`) |
//! | Cache schreiben | `cache_write` | `cacheWrite` (→ `input`) |
//! | Ausgabe | `output − reasoning` | `output` (→ 0) |
//! | Reasoning | `reasoning` | `reasoning` (→ `output`) |
//!
//! dazu `perRequest` sowie für die weiteren Endpunkte (AIGW-21-Folge) `perImage` × erzeugte
//! Bilder, `perAudioMinute` × Audio-Dauer (anteilig je Millisekunde) und
//! `perMillionCharacters` × Zeichen einer Sprachausgabe. Gerechnet wird exakt in `i128`
//! (Preis in Nano-Einheiten × Menge) und erst die Summe auf ganze Nano-Einheiten abgerundet.

use aigw_core::ir::Usage;
use aigw_core::observe::CostBreakdown;
use aigw_core::resource::{Amount, PriceSpec};
use aigw_core::snapshot::Snapshot;
use aigw_core::types::ProviderKind;
use time::OffsetDateTime;

/// Kosten von `usage` zum Preis `price`.
pub fn cost(price: &PriceSpec, usage: &Usage) -> Amount {
    cost_with_breakdown(price, usage).0
}

/// Kosten von `usage` zum Preis `price` samt Aufschlüsselung nach Token-Art (AIGW-117).
///
/// Die Summe rechnet exakt wie [`cost`] (erst die Summe wird abgerundet); die Teile sind je Art
/// abgerundet. Die Ersparnis durch Prompt-Caching ist `cache_read × (input − cachedInput)`,
/// nur wenn ein eigener, niedrigerer Cache-Lesepreis hinterlegt ist.
pub fn cost_with_breakdown(price: &PriceSpec, usage: &Usage) -> (Amount, CostBreakdown) {
    let p = &price.per_million_tokens;
    let nanos = |a: Option<Amount>| i128::from(a.map_or(0, Amount::nanos));
    let input = nanos(p.input);
    let cached = p.cached_input.map_or(input, |a| i128::from(a.nanos()));
    let cache_write = p.cache_write.map_or(input, |a| i128::from(a.nanos()));
    let output = nanos(p.output);
    let reasoning = p.reasoning.map_or(output, |a| i128::from(a.nanos()));

    let read_tokens = usage.cache_read_tokens.unwrap_or(0);
    let write_tokens = usage.cache_write_tokens.unwrap_or(0);
    let plain_input = usage
        .input_tokens
        .saturating_sub(read_tokens)
        .saturating_sub(write_tokens);
    let reasoning_tokens = usage.reasoning_tokens.unwrap_or(0).min(usage.output_tokens);
    let plain_output = usage.output_tokens - reasoning_tokens;

    let t = i128::from;
    let part = |v: i128| Amount::from_nanos(i64::try_from(v / 1_000_000).unwrap_or(i64::MAX));
    let input_part = input * t(plain_input);
    let read_part = cached * t(read_tokens);
    let write_part = cache_write * t(write_tokens);
    let output_part = output * t(plain_output) + reasoning * t(reasoning_tokens);
    let sum = input_part + read_part + write_part + output_part;
    let tokens = i64::try_from(sum / 1_000_000).unwrap_or(i64::MAX);
    // Bilder (ganzzahlig), Audio-Minuten (je ms) und Zeichen (je 1 Mio.).
    let images = nanos(price.per_image) * t(usage.images.unwrap_or(0));
    let audio = nanos(price.per_audio_minute) * t(usage.audio_ms.unwrap_or(0)) / 60_000;
    let chars = nanos(price.per_million_characters) * t(usage.characters.unwrap_or(0)) / 1_000_000;
    let media = i64::try_from(images + audio + chars).unwrap_or(i64::MAX);
    let total = Amount::from_nanos(tokens)
        .saturating_add(Amount::from_nanos(media))
        .saturating_add(price.per_request.unwrap_or(Amount::ZERO));
    let breakdown = CostBreakdown {
        input: part(input_part),
        cache_read: part(read_part),
        cache_write: part(write_part),
        output: part(output_part),
        cache_read_saved: part((input - cached).max(0) * t(read_tokens)),
    };
    (total, breakdown)
}

/// Kosten einer Anfrage an `provider`/`model` mit dem zum Zeitpunkt `at` gültigen Preis
/// ([`Snapshot::price_at`]); `None`, wenn für das Modell kein Preis hinterlegt ist.
pub fn cost_at(
    snapshot: &Snapshot,
    provider: ProviderKind,
    model: &str,
    at: OffsetDateTime,
    usage: &Usage,
) -> Option<Amount> {
    snapshot
        .price_at(provider, model, at)
        .map(|p| cost(&p.spec, usage))
}

/// Wie [`cost_at`], zusätzlich mit Aufschlüsselung nach Token-Art (AIGW-117).
pub fn cost_breakdown_at(
    snapshot: &Snapshot,
    provider: ProviderKind,
    model: &str,
    at: OffsetDateTime,
    usage: &Usage,
) -> Option<(Amount, CostBreakdown)> {
    snapshot
        .price_at(provider, model, at)
        .map(|p| cost_with_breakdown(&p.spec, usage))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;

    use aigw_core::resource::{Object, ObjectMeta, TokenPrices, new_id};
    use time::macros::datetime;

    use super::*;

    fn amount(s: &str) -> Amount {
        s.parse().unwrap()
    }

    fn price(valid_from: OffsetDateTime, input: &str, output: &str) -> PriceSpec {
        PriceSpec {
            provider: None,
            model: "m".into(),
            valid_from,
            per_million_tokens: TokenPrices {
                input: Some(amount(input)),
                output: Some(amount(output)),
                ..TokenPrices::default()
            },
            per_request: None,
            per_image: None,
            per_audio_minute: None,
            per_million_characters: None,
        }
    }

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            ..Usage::default()
        }
    }

    #[test]
    fn plain_input_and_output() {
        // 2,50 / 10,00 je 1 Mio.: 1000 Eingabe + 500 Ausgabe = 0,0025 + 0,005.
        let p = price(datetime!(2026-01-01 0:00 UTC), "2.50", "10");
        assert_eq!(cost(&p, &usage(1000, 500)).to_string(), "0.0075");
        assert_eq!(cost(&p, &usage(0, 0)), Amount::ZERO);
    }

    #[test]
    fn token_kinds_with_and_without_fallback() {
        let mut p = price(datetime!(2026-01-01 0:00 UTC), "3", "15");
        let u = Usage {
            input_tokens: 10_000, // davon 6000 aus dem Cache, 1000 in den Cache geschrieben
            output_tokens: 2_000, // davon 500 Reasoning
            cache_read_tokens: Some(6_000),
            cache_write_tokens: Some(1_000),
            reasoning_tokens: Some(500),
            ..Usage::default()
        };
        // Ohne eigene Preise: alles zum Ein-/Ausgabepreis.
        // 10 000 × 3 + 2000 × 15 = 30 000 + 30 000 → 0,06
        assert_eq!(cost(&p, &u).to_string(), "0.06");
        p.per_million_tokens.cached_input = Some(amount("0.30"));
        p.per_million_tokens.cache_write = Some(amount("3.75"));
        p.per_million_tokens.reasoning = Some(amount("20"));
        p.per_request = Some(amount("0.001"));
        // 3000×3 + 6000×0,3 + 1000×3,75 + 1500×15 + 500×20 = 9000 + 1800 + 3750 + 22500 + 10000
        // = 47 050 / 1 Mio. = 0,04705 (+ 0,001 je Anfrage)
        assert_eq!(cost(&p, &u).to_string(), "0.04805");
        // AIGW-117: Aufschlüsselung je Art, Ersparnis = 6000 × (3 − 0,3) / 1 Mio. = 0,0162.
        let (total, b) = cost_with_breakdown(&p, &u);
        assert_eq!(total.to_string(), "0.04805");
        assert_eq!(b.input.to_string(), "0.009");
        assert_eq!(b.cache_read.to_string(), "0.0018");
        assert_eq!(b.cache_write.to_string(), "0.00375");
        assert_eq!(b.output.to_string(), "0.0325");
        assert_eq!(b.cache_read_saved.to_string(), "0.0162");
        // Ohne eigenen Cache-Lesepreis keine Ersparnis.
        p.per_million_tokens.cached_input = None;
        assert_eq!(cost_with_breakdown(&p, &u).1.cache_read_saved, Amount::ZERO);
    }

    #[test]
    fn images_audio_and_characters() {
        let mut p = price(datetime!(2026-01-01 0:00 UTC), "0", "0");
        p.per_image = Some(amount("0.04"));
        p.per_audio_minute = Some(amount("0.006"));
        p.per_million_characters = Some(amount("15"));
        let u = |images, audio_ms, characters| Usage {
            images,
            audio_ms,
            characters,
            ..Usage::default()
        };
        assert_eq!(cost(&p, &u(Some(3), None, None)).to_string(), "0.12");
        // 90 s Audio = 1,5 min × 0,006 = 0,009; anteilig je Millisekunde.
        assert_eq!(cost(&p, &u(None, Some(90_000), None)).to_string(), "0.009");
        assert_eq!(cost(&p, &u(None, Some(1), None)).nanos(), 100);
        // 2000 Zeichen × 15 / 1 Mio. = 0,03.
        assert_eq!(cost(&p, &u(None, None, Some(2_000))).to_string(), "0.03");
        // Ohne Preise: 0.
        let free = price(datetime!(2026-01-01 0:00 UTC), "0", "0");
        assert_eq!(
            cost(&free, &u(Some(3), Some(60_000), Some(10))),
            Amount::ZERO
        );
    }

    #[test]
    fn rounding_is_on_the_sum() {
        // 0,000001 je 1 Mio. = 1 Nano-Einheit je 1000 Tokens: einzeln abgerundet wären 999
        // Tokens zweimal 0, zusammen 1998 → 1 Nano-Einheit.
        let p = price(datetime!(2026-01-01 0:00 UTC), "0.000001", "0.000001");
        assert_eq!(cost(&p, &usage(999, 999)).nanos(), 1);
    }

    #[test]
    fn inconsistent_usage_does_not_underflow() {
        let p = price(datetime!(2026-01-01 0:00 UTC), "1", "1");
        let u = Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: Some(50),
            reasoning_tokens: Some(50),
            ..Usage::default()
        };
        assert!(cost(&p, &u) > Amount::ZERO);
    }

    #[test]
    fn price_change_uses_price_at_request_time() {
        let mut snap = Snapshot::default();
        let old = price(datetime!(2026-01-01 0:00 UTC), "1", "2");
        let new = price(datetime!(2026-06-01 0:00 UTC), "2", "4");
        snap.prices.insert(
            (None, "m".into()),
            [old, new]
                .into_iter()
                .map(|spec| {
                    Arc::new(Object {
                        id: new_id(),
                        metadata: ObjectMeta::named("p"),
                        spec,
                    })
                })
                .collect(),
        );
        let u = usage(1_000_000, 1_000_000);
        let at = |t| cost_at(&snap, ProviderKind::OpenAi, "m", t, &u).map(|a| a.to_string());
        assert_eq!(at(datetime!(2025-12-31 23:59 UTC)), None);
        assert_eq!(at(datetime!(2026-05-31 23:59:59 UTC)).as_deref(), Some("3"));
        assert_eq!(at(datetime!(2026-06-01 0:00 UTC)).as_deref(), Some("6"));
        assert_eq!(
            cost_at(
                &snap,
                ProviderKind::OpenAi,
                "other",
                datetime!(2026-07-01 0:00 UTC),
                &u
            ),
            None
        );
    }
}
