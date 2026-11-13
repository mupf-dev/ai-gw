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
//!
//! Daneben gibt es **feste Zeiträume** ([`Window::Period`], F25: Kalendertag/-woche/-monat):
//! ein einziger Bucket `[start, end)` ohne Gleiten; am Ende beginnt der Zähler bei 0. Den
//! Zeitraum (Zeitzone, Monatslänge, Sommerzeit) berechnet der Aufrufer; Bucket-Nummer ist der
//! Beginn in Millisekunden.

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
    /// 30 × 24 h, gleitend (kein Kalendermonat; Konfiguration `30d`).
    Days30,
    /// Fester Zeitraum `[start_ms, end_ms)` ohne Gleiten (Kalendertag/-woche/-monat, F25).
    /// Der Zähler gilt bis `end_ms` und beginnt mit dem nächsten Zeitraum bei 0.
    Period {
        /// Beginn (inklusive, Unix-ms) – zugleich die Bucket-Nummer.
        start_ms: u64,
        /// Ende (exklusive, Unix-ms).
        end_ms: u64,
    },
}

/// Index der Map für feste Zeiträume in der lokalen Engine (nach den gleitenden Fenstern).
pub(crate) const PERIOD_INDEX: usize = 5;

impl Window {
    /// Alle gleitenden Fenster in fester Reihenfolge.
    pub const SLIDING: [Self; 5] = [
        Self::Second,
        Self::Minute,
        Self::Hour,
        Self::Day,
        Self::Days30,
    ];

    /// Fester Zeitraum `[start_ms, end_ms)` (mindestens 1 ms lang).
    pub const fn period(start_ms: u64, end_ms: u64) -> Self {
        let end_ms = if end_ms > start_ms {
            end_ms
        } else {
            start_ms + 1
        };
        Self::Period { start_ms, end_ms }
    }

    /// `true` für gleitende Fenster, `false` für feste Zeiträume.
    pub const fn is_sliding(self) -> bool {
        !matches!(self, Self::Period { .. })
    }

    /// Länge in Millisekunden.
    pub const fn length_ms(self) -> u64 {
        match self {
            Self::Second => 1_000,
            Self::Minute => 60_000,
            Self::Hour => 3_600_000,
            Self::Day => DAY_MS,
            Self::Days30 => 30 * DAY_MS,
            Self::Period { start_ms, end_ms } => {
                let l = end_ms.saturating_sub(start_ms);
                if l == 0 { 1 } else { l }
            }
        }
    }

    /// Länge als [`Duration`].
    pub const fn length(self) -> Duration {
        Duration::from_millis(self.length_ms())
    }

    /// Kurzzeichen für externe Schlüssel (Postgres-Zeilen, Redis-Schlüssel); `p` für feste
    /// Zeiträume (Bucket = Beginn, unterscheidet Zeiträume desselben Schlüssels).
    pub const fn code(self) -> &'static str {
        match self {
            Self::Second => "s",
            Self::Minute => "m",
            Self::Hour => "h",
            Self::Day => "d",
            Self::Days30 => "30d",
            Self::Period { .. } => "p",
        }
    }

    /// Kurzzeichen → gleitendes Fenster (Umkehrung von [`Window::code`]; feste Zeiträume
    /// brauchen zusätzlich Beginn und Ende).
    pub fn from_code(code: &str) -> Option<Self> {
        Self::SLIDING.into_iter().find(|w| w.code() == code)
    }

    /// Position der Map je Fenster in der lokalen Engine.
    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Second => 0,
            Self::Minute => 1,
            Self::Hour => 2,
            Self::Day => 3,
            Self::Days30 => 4,
            Self::Period { .. } => PERIOD_INDEX,
        }
    }

    /// Bucket-Nummer zum Zeitpunkt `now_ms` (feste Zeiträume: immer der Beginn).
    pub const fn bucket(self, now_ms: u64) -> u64 {
        match self {
            Self::Period { start_ms, .. } => start_ms,
            _ => now_ms / self.length_ms(),
        }
    }

    /// Beginn (Unix-ms) des Buckets `idx`.
    pub const fn bucket_start(self, idx: u64) -> u64 {
        match self {
            Self::Period { .. } => idx,
            _ => idx.saturating_mul(self.length_ms()),
        }
    }

    /// Zählt Bucket `idx` zum Zeitpunkt `now_ms` noch zum Fenster (laufender oder – gleitend –
    /// voriger Bucket)?
    pub const fn counts(self, idx: u64, now_ms: u64) -> bool {
        match self {
            Self::Period { start_ms, end_ms } => idx == start_ms && now_ms < end_ms,
            _ => idx.saturating_add(1) >= self.bucket(now_ms),
        }
    }

    /// Ende (Unix-ms) eines festen Zeitraums, `u64::MAX` bei gleitenden Fenstern.
    pub(crate) const fn until(self) -> u64 {
        match self {
            Self::Period { end_ms, .. } => end_ms,
            _ => u64::MAX,
        }
    }

    /// Geschätzter Verbrauch (feste Zeiträume: der Zählerstand selbst).
    pub(crate) fn estimate(self, cur: u64, prev: u64, elapsed: u64) -> u64 {
        if self.is_sliding() {
            estimate(cur, prev, elapsed, self.length_ms())
        } else {
            cur
        }
    }

    /// Wie [`evaluate`] für dieses Fenster.
    pub(crate) fn evaluate(
        self,
        cur: u64,
        prev: u64,
        elapsed: u64,
        limit: u64,
        amount: u64,
    ) -> Evaluation {
        let prev = if self.is_sliding() { prev } else { 0 };
        evaluate(cur, prev, elapsed, self.length_ms(), limit, amount)
    }

    /// Wie [`retry_after_ms`]; bei festen Zeiträumen bis zum Ende des Zeitraums.
    pub(crate) fn retry_after_ms(
        self,
        cur: u64,
        prev: u64,
        elapsed: u64,
        limit: u64,
        amount: u64,
    ) -> Option<u64> {
        let len = self.length_ms();
        if self.is_sliding() {
            return retry_after_ms(cur, prev, elapsed, len, limit, amount);
        }
        if amount > limit {
            return None;
        }
        if cur.saturating_add(amount) <= limit {
            return Some(0);
        }
        Some(len.saturating_sub(elapsed).max(1))
    }

    /// Wie [`reset_after_ms`]; bei festen Zeiträumen das Ende des Zeitraums (sofern etwas
    /// gebucht ist).
    pub(crate) fn reset_after_ms(self, cur: u64, prev: u64, elapsed: u64) -> u64 {
        let len = self.length_ms();
        if self.is_sliding() {
            reset_after_ms(cur, prev, elapsed, len)
        } else if cur > 0 {
            len.saturating_sub(elapsed)
        } else {
            0
        }
    }
}

impl Window {
    /// Gleitendes Fenster eines Limits; `None` für Kalenderzeiträume (die braucht den
    /// Zeitraum aus Zeitzone und Zeitpunkt, siehe [`Window::period`]).
    pub fn sliding(w: LimitWindow) -> Option<Self> {
        Some(match w {
            LimitWindow::Second => Self::Second,
            LimitWindow::Minute => Self::Minute,
            LimitWindow::Hour => Self::Hour,
            LimitWindow::Day => Self::Day,
            LimitWindow::Days30 => Self::Days30,
            LimitWindow::CalendarDay | LimitWindow::CalendarWeek | LimitWindow::CalendarMonth => {
                return None;
            }
        })
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
        for w in Window::SLIDING {
            assert_eq!(Window::from_code(w.code()), Some(w));
            assert_eq!(Window::SLIDING[w.index()], w);
            assert!(w.is_sliding());
        }
        assert_eq!(Window::from_code("x"), None);
        assert_eq!(Window::from_code("p"), None);
        assert_eq!(Window::Days30.length(), Duration::from_secs(30 * 86_400));
        assert_eq!(Window::Days30.code(), "30d");
    }

    #[test]
    fn period_window_is_fixed() {
        let w = Window::period(1_000, 61_000);
        assert!(!w.is_sliding());
        assert_eq!(w.length_ms(), 60_000);
        assert_eq!(w.bucket(30_000), 1_000);
        assert_eq!(w.bucket_start(1_000), 1_000);
        assert_eq!(w.index(), PERIOD_INDEX);
        assert!(w.counts(1_000, 60_999));
        assert!(!w.counts(1_000, 61_000));
        assert!(!w.counts(999, 2_000));
        // kein Gleiten: der „vorige“ Bucket zählt nie
        assert_eq!(w.estimate(10, 500, 0), 10);
        assert!(w.evaluate(90, 500, 0, 100, 10).admit);
        // Wartezeit/Reset = Ende des Zeitraums
        assert_eq!(w.retry_after_ms(95, 0, 20_000, 100, 10), Some(40_000));
        assert_eq!(w.retry_after_ms(0, 0, 0, 100, 101), None);
        assert_eq!(w.retry_after_ms(5, 0, 0, 100, 10), Some(0));
        assert_eq!(w.reset_after_ms(5, 0, 20_000), 40_000);
        assert_eq!(w.reset_after_ms(0, 0, 20_000), 0);
        assert_eq!(Window::period(5, 5).length_ms(), 1);
        // gleitend: wie die freien Funktionen
        assert_eq!(Window::Minute.bucket_start(3), 180_000);
        assert!(Window::Minute.counts(9, 10 * 60_000 + 5));
        assert!(!Window::Minute.counts(8, 10 * 60_000 + 5));
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
        assert_eq!(Window::sliding(LimitWindow::Minute), Some(Window::Minute));
        assert_eq!(Window::sliding(LimitWindow::Day), Some(Window::Day));
        assert_eq!(Window::sliding(LimitWindow::Days30), Some(Window::Days30));
        assert_eq!(Window::sliding(LimitWindow::CalendarMonth), None);
    }
}
