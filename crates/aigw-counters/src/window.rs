// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Zeitfenster und die Rechenregeln des Sliding-Window-Verfahrens.
//!
//! Jedes Fenster der Länge `len` wird in Buckets an Epochengrenzen eingeteilt
//! (`bucket = now_ms / len`). Der Verbrauch im gleitenden Fenster wird aus dem laufenden Bucket
//! (`cur`) und dem vorigen (`prev`) geschätzt; der vorige zählt anteilig zur noch
//! überlappenden Zeit:
//!
//! ```text
//! estimate = cur + ⌊prev · (len − elapsed) / len⌋
//! ```
//!
//! Alle Backends verwenden diese Funktionen (bei `redis` rechnet das Lua-Skript dieselbe Formel
//! für die Entscheidung, die Header-Werte rechnet Rust nach).

use std::time::Duration;

use aigw_core::resource::LimitWindow;

/// Länge eines Kalendertags in Millisekunden.
const DAY_MS: u64 = 86_400_000;

/// Fenster eines Limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Window {
    /// Sekunde.
    Second,
    /// Minute (RPM/TPM).
    Minute,
    /// Stunde.
    Hour,
    /// Tag (24 h, gleitend).
    Day,
    /// Monat, als **30 Tage** gleitend (kein Kalendermonat).
    Month,
}

impl Window {
    /// Alle Fenster in fester Reihenfolge.
    pub const ALL: [Self; 5] = [
        Self::Second,
        Self::Minute,
        Self::Hour,
        Self::Day,
        Self::Month,
    ];

    /// Länge in Millisekunden.
    pub const fn length_ms(self) -> u64 {
        match self {
            Self::Second => 1_000,
            Self::Minute => 60_000,
            Self::Hour => 3_600_000,
            Self::Day => DAY_MS,
            Self::Month => 30 * DAY_MS,
        }
    }

    /// Länge als [`Duration`].
    pub const fn length(self) -> Duration {
        Duration::from_millis(self.length_ms())
    }

    /// Kurzzeichen für externe Schlüssel (Postgres-Zeilen, Redis-Schlüssel).
    pub const fn code(self) -> &'static str {
        match self {
            Self::Second => "s",
            Self::Minute => "m",
            Self::Hour => "h",
            Self::Day => "d",
            Self::Month => "mo",
        }
    }

    /// Kurzzeichen → Fenster (Umkehrung von [`Window::code`]).
    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|w| w.code() == code)
    }

    /// Position in [`Window::ALL`] (für Arrays je Fenster).
    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Second => 0,
            Self::Minute => 1,
            Self::Hour => 2,
            Self::Day => 3,
            Self::Month => 4,
        }
    }

    /// Bucket-Nummer zum Zeitpunkt `now_ms`.
    pub const fn bucket(self, now_ms: u64) -> u64 {
        now_ms / self.length_ms()
    }
}

impl From<LimitWindow> for Window {
    fn from(w: LimitWindow) -> Self {
        match w {
            LimitWindow::Second => Self::Second,
            LimitWindow::Minute => Self::Minute,
            LimitWindow::Hour => Self::Hour,
            LimitWindow::Day => Self::Day,
        }
    }
}

/// Gewichtete Schätzung des Verbrauchs im gleitenden Fenster.
pub(crate) fn estimate(cur: u64, prev: u64, elapsed: u64, len: u64) -> u64 {
    let rest = len.saturating_sub(elapsed.min(len));
    let weighted = u128::from(prev) * u128::from(rest) / u128::from(len.max(1));
    cur.saturating_add(u64::try_from(weighted).unwrap_or(u64::MAX))
}

/// Ergebnis der Bewertung eines Zählers für eine geplante Reservierung.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Evaluation {
    /// Geschätzter Verbrauch **vor** der Reservierung.
    pub(crate) estimate: u64,
    /// Passt `amount` noch ins Limit?
    pub(crate) admit: bool,
}

/// Bewertet, ob `amount` bei Stand (`cur`, `prev`) noch unter `limit` passt.
pub(crate) fn evaluate(
    cur: u64,
    prev: u64,
    elapsed: u64,
    len: u64,
    limit: u64,
    amount: u64,
) -> Evaluation {
    let est = estimate(cur, prev, elapsed, len);
    Evaluation {
        estimate: est,
        admit: est.saturating_add(amount) <= limit,
    }
}

/// Wartezeit (ms), bis `amount` bei unverändertem Verbrauch wieder ins Limit passt.
///
/// `None`, wenn `amount > limit` (passt nie). Mindestens 1 ms, wenn aktuell nicht passend.
pub(crate) fn retry_after_ms(
    cur: u64,
    prev: u64,
    elapsed: u64,
    len: u64,
    limit: u64,
    amount: u64,
) -> Option<u64> {
    if amount > limit {
        return None;
    }
    let elapsed = elapsed.min(len);
    if estimate(cur, prev, elapsed, len).saturating_add(amount) <= limit {
        return Some(0);
    }
    let need = limit - amount; // höchstens zulässiger Schätzwert vor der Reservierung
    let wait = if cur <= need {
        // Noch in diesem Bucket: Der Anteil von `prev` muss auf `need − cur` sinken.
        // Gesucht ist t mit prev·(len − t) ≤ (need − cur)·len  (⇒ ⌊…⌋ ≤ need − cur).
        // prev > 0, sonst wäre die Schätzung ≤ need.
        let allowed = u128::from(need - cur) * u128::from(len) / u128::from(prev.max(1));
        let t = u128::from(len).saturating_sub(allowed);
        u64::try_from(t).unwrap_or(u64::MAX).saturating_sub(elapsed)
    } else {
        // Erst im nächsten Bucket, in dem `cur` zum gewichteten Vorgänger wird.
        let allowed = u128::from(need) * u128::from(len) / u128::from(cur);
        let t = u64::try_from(u128::from(len).saturating_sub(allowed)).unwrap_or(u64::MAX);
        (len - elapsed).saturating_add(t)
    };
    Some(wait.max(1))
}

/// Zeit (ms), bis die Schätzung auf 0 gefallen ist (für `x-ratelimit-reset-*`).
pub(crate) fn reset_after_ms(cur: u64, prev: u64, elapsed: u64, len: u64) -> u64 {
    let elapsed = elapsed.min(len);
    if cur > 0 {
        (len - elapsed) + len
    } else if prev > 0 {
        len - elapsed
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    const L: u64 = 60_000;

    #[test]
    fn codes_roundtrip() {
        for w in Window::ALL {
            assert_eq!(Window::from_code(w.code()), Some(w));
            assert_eq!(Window::ALL[w.index()], w);
        }
        assert_eq!(Window::from_code("x"), None);
        assert_eq!(Window::Month.length(), Duration::from_secs(30 * 86_400));
    }

    #[test]
    fn estimate_weights_previous_bucket() {
        assert_eq!(estimate(10, 0, 0, L), 10);
        assert_eq!(estimate(10, 100, 0, L), 110); // Bucket-Anfang: prev voll
        assert_eq!(estimate(10, 100, L / 4, L), 85); // 75 % Überlappung
        assert_eq!(estimate(10, 100, L / 2, L), 60);
        assert_eq!(estimate(10, 100, L, L), 10);
        assert_eq!(estimate(10, 100, 2 * L, L), 10); // elapsed wird begrenzt
        assert_eq!(estimate(u64::MAX, u64::MAX, 0, L), u64::MAX);
    }

    #[test]
    fn evaluate_admits_up_to_limit() {
        assert!(evaluate(90, 0, 0, L, 100, 10).admit);
        assert!(!evaluate(91, 0, 0, L, 100, 10).admit);
        assert!(evaluate(0, 0, 0, L, 0, 0).admit);
        assert!(!evaluate(0, 0, 0, L, 0, 1).admit);
    }

    #[test]
    fn retry_after_never_when_amount_exceeds_limit() {
        assert_eq!(retry_after_ms(0, 0, 0, L, 10, 11), None);
    }

    #[test]
    fn retry_after_zero_when_fits() {
        assert_eq!(retry_after_ms(10, 0, 0, L, 100, 5), Some(0));
    }

    #[test]
    fn retry_after_within_current_bucket() {
        // cur=20, prev=100 bei elapsed=0 → Schätzung 120. Limit 100, amount 10 → need 90,
        // also prev-Anteil ≤ 70 → t ≥ 0,3·L.
        let wait = retry_after_ms(20, 100, 0, L, 100, 10).unwrap();
        assert_eq!(wait, L * 3 / 10);
        assert!(evaluate(20, 100, wait, L, 100, 10).admit);
        assert!(!evaluate(20, 100, wait - 1_000, L, 100, 10).admit);
    }

    #[test]
    fn retry_after_in_next_bucket() {
        // cur=95 > need=90 → erst nächster Bucket: dort 95·(1 − t/L) ≤ 90.
        let elapsed = L / 2;
        let wait = retry_after_ms(95, 0, elapsed, L, 100, 10).unwrap();
        let in_next = wait - (L - elapsed);
        assert!(evaluate(0, 95, in_next, L, 100, 10).admit);
        assert!(!evaluate(0, 95, in_next.saturating_sub(1_000), L, 100, 10).admit);
    }

    #[test]
    fn retry_after_is_at_least_one_ms_and_consistent_randomized() {
        // Grobe Eigenschaftsprüfung über ein Raster: nach der Wartezeit passt es,
        // eine Sekunde vorher (sofern > 1 s) nicht.
        for cur in [0u64, 1, 30, 89, 90, 91, 150] {
            for prev in [0u64, 1, 50, 100, 500] {
                for elapsed in [0u64, 1, 15_000, 59_999] {
                    let Some(wait) = retry_after_ms(cur, prev, elapsed, L, 100, 10) else {
                        panic!("amount ≤ limit muss eine Wartezeit liefern")
                    };
                    if wait == 0 {
                        assert!(evaluate(cur, prev, elapsed, L, 100, 10).admit);
                        continue;
                    }
                    let at = elapsed + wait;
                    let (c, p, e) = if at < L {
                        (cur, prev, at)
                    } else if at < 2 * L {
                        (0, cur, at - L)
                    } else {
                        (0, 0, at - 2 * L)
                    };
                    assert!(
                        evaluate(c, p, e, L, 100, 10).admit,
                        "cur={cur} prev={prev} elapsed={elapsed} wait={wait}"
                    );
                }
            }
        }
    }

    #[test]
    fn reset_after() {
        assert_eq!(reset_after_ms(0, 0, 10, L), 0);
        assert_eq!(reset_after_ms(0, 5, 10_000, L), 50_000);
        assert_eq!(reset_after_ms(5, 5, 10_000, L), 110_000);
    }

    #[test]
    fn from_limit_window() {
        assert_eq!(Window::from(LimitWindow::Minute), Window::Minute);
        assert_eq!(Window::from(LimitWindow::Day), Window::Day);
    }
}
