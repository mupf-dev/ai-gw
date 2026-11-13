// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Auswahl eines Deployments innerhalb einer Prioritätsgruppe (AIGW-24).
//!
//! - `weighted`: gewichtet zufällig (Gewicht 0 = nur, wenn alle 0 sind).
//! - `least-latency`: kleinste EWMA (TTFT bei Streams, sonst Antwortzeit; fehlt der eine
//!   Wert, gilt der andere). Deployments ohne Messwert gelten als 0 (werden zuerst probiert).
//!   Alles innerhalb von +10 % des Besten gilt als gleich gut → gewichtet zufällig, damit die
//!   Messwerte aller Kandidaten frisch bleiben.
//! - `least-busy`: wenigste offene Anfragen, Gleichstand gewichtet zufällig.
//! - `cost`: günstigster Preis (Eingabe + Ausgabe je 1 Mio. Tokens laut Preisliste);
//!   Deployments ohne Preis zuletzt; Gleichstand gewichtet zufällig.
//!
//! Zufall: wyrand je Thread, Startwert aus `RandomState` (kein Lock, keine Abhängigkeit).

use std::cell::Cell;
use std::hash::{BuildHasher as _, Hasher as _};

use aigw_core::resource::RoutingStrategy;

use super::Member;

thread_local! {
    static RNG: Cell<u64> = Cell::new(seed());
}

fn seed() -> u64 {
    let mut h = std::hash::RandomState::new().build_hasher();
    h.write_u64(0x9e37_79b9_7f4a_7c15);
    h.finish() | 1
}

/// Nächste Zufallszahl (wyrand).
pub(crate) fn rand_u64() -> u64 {
    RNG.with(|s| {
        let x = s.get().wrapping_add(0xa076_1d64_78bd_642f);
        s.set(x);
        let t = u128::from(x) * u128::from(x ^ 0xe703_7ed1_a0b4_28db);
        ((t >> 64) as u64) ^ (t as u64)
    })
}

/// Gleichverteilt in `0..n` (`n > 0`).
pub(crate) fn rand_below(n: u64) -> u64 {
    ((u128::from(rand_u64()) * u128::from(n)) >> 64) as u64
}

/// Gewichtet zufällige Wahl unter `cands` (Indizes in `members`), Ergebnis = Position in
/// `cands`.
fn weighted(members: &[std::sync::Arc<Member>], cands: &[usize]) -> usize {
    let total: u64 = cands.iter().map(|&i| u64::from(members[i].weight)).sum();
    if total == 0 {
        return rand_below(cands.len() as u64) as usize;
    }
    let mut r = rand_below(total);
    for (pos, &i) in cands.iter().enumerate() {
        let w = u64::from(members[i].weight);
        if r < w {
            return pos;
        }
        r -= w;
    }
    cands.len() - 1
}

/// Wählt aus `cands` (nicht leer) nach Strategie; Ergebnis = Position in `cands`.
pub(crate) fn choose(
    strategy: RoutingStrategy,
    members: &[std::sync::Arc<Member>],
    cands: &[usize],
    stream: bool,
    scratch: &mut Vec<usize>,
) -> usize {
    if cands.len() == 1 {
        return 0;
    }
    let key = |i: usize| -> u64 {
        let m = &members[i];
        match strategy {
            RoutingStrategy::Weighted => 0,
            RoutingStrategy::LeastLatency => {
                let (a, b) = if stream {
                    (&m.state.ttft, &m.state.latency)
                } else {
                    (&m.state.latency, &m.state.ttft)
                };
                a.get().or_else(|| b.get()).unwrap_or(0)
            }
            RoutingStrategy::LeastBusy => u64::from(m.state.in_flight()),
            RoutingStrategy::Cost => m.cost.unwrap_or(u64::MAX),
        }
    };
    if strategy == RoutingStrategy::Weighted {
        return weighted(members, cands);
    }
    let best = cands.iter().map(|&i| key(i)).min().unwrap_or(0);
    let limit = if strategy == RoutingStrategy::LeastLatency {
        best.saturating_add(best / 10)
    } else {
        best
    };
    scratch.clear();
    scratch.extend(cands.iter().copied().filter(|&i| key(i) <= limit));
    let pos_in_scratch = weighted(members, scratch);
    let chosen = scratch[pos_in_scratch];
    cands.iter().position(|&i| i == chosen).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rand_below_is_in_range_and_spread() {
        let mut hist = [0u32; 4];
        for _ in 0..40_000 {
            let v = rand_below(4) as usize;
            hist[v] += 1;
        }
        for h in hist {
            assert!((9_000..11_000).contains(&h), "{hist:?}");
        }
    }
}
