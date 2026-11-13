// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Budget-Zeiträume als Kalendertag, -woche (ab Montag) und -monat in einer konfigurierbaren
//! Zeitzone (`billing.timezone`, Standard UTC). Sommerzeit wird über die eingebettete
//! IANA-Zeitzonendatenbank (jiff) berücksichtigt: Ein Tag kann 23 oder 25 Stunden haben.

use aigw_core::resource::BudgetPeriod;
use jiff::tz::TimeZone;
use jiff::{Span, Timestamp, Zoned};

/// Fehler: unbekannte Zeitzone.
#[derive(Debug, thiserror::Error)]
#[error("unbekannte Zeitzone {name:?}: {reason}")]
pub struct CalendarError {
    name: String,
    reason: String,
}

/// Ein Zeitraum `[start, end)` in Millisekunden seit der Unix-Epoche.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeriodRange {
    /// Beginn (inklusive).
    pub start_ms: i64,
    /// Ende (exklusive); `None` bei [`BudgetPeriod::Total`].
    pub end_ms: Option<i64>,
}

/// Kalender für Budget-Zeiträume.
#[derive(Debug, Clone)]
pub struct Calendar {
    tz: TimeZone,
    name: String,
}

impl Default for Calendar {
    fn default() -> Self {
        Self::utc()
    }
}

impl Calendar {
    /// UTC.
    pub fn utc() -> Self {
        Self {
            tz: TimeZone::UTC,
            name: "UTC".to_owned(),
        }
    }

    /// Kalender in der IANA-Zeitzone `name` (z. B. `Europe/Berlin`).
    pub fn new(name: &str) -> Result<Self, CalendarError> {
        let name = name.trim();
        if name.is_empty() || name.eq_ignore_ascii_case("utc") {
            return Ok(Self::utc());
        }
        let tz = TimeZone::get(name).map_err(|e| CalendarError {
            name: name.to_owned(),
            reason: e.to_string(),
        })?;
        Ok(Self {
            tz,
            name: name.to_owned(),
        })
    }

    /// Name der Zeitzone.
    pub fn timezone(&self) -> &str {
        &self.name
    }

    /// Zeitraum `period`, in den der Zeitpunkt `at_ms` fällt.
    pub fn period(&self, period: BudgetPeriod, at_ms: i64) -> PeriodRange {
        if period == BudgetPeriod::Total {
            return PeriodRange {
                start_ms: 0,
                end_ms: None,
            };
        }
        match self.calendar_period(period, at_ms) {
            Some(r) => r,
            // Nur bei Zeitpunkten außerhalb des darstellbaren Bereichs: fester UTC-Tag.
            None => {
                const DAY: i64 = 86_400_000;
                let start = at_ms.div_euclid(DAY) * DAY;
                PeriodRange {
                    start_ms: start,
                    end_ms: Some(start + DAY),
                }
            }
        }
    }

    fn calendar_period(&self, period: BudgetPeriod, at_ms: i64) -> Option<PeriodRange> {
        let ts = Timestamp::from_millisecond(at_ms).ok()?;
        let zoned = ts.to_zoned(self.tz.clone());
        let date = zoned.date();
        let (start, next) = match period {
            BudgetPeriod::Day => (date, date.tomorrow().ok()?),
            BudgetPeriod::Week => {
                let back = i64::from(date.weekday().to_monday_zero_offset());
                let monday = date.checked_sub(Span::new().days(back)).ok()?;
                (monday, monday.checked_add(Span::new().days(7)).ok()?)
            }
            BudgetPeriod::Month => {
                let first = date.first_of_month();
                (first, date.last_of_month().tomorrow().ok()?)
            }
            BudgetPeriod::Total => return None,
        };
        let start_ms = self.day_start(start)?;
        let end_ms = self.day_start(next)?;
        Some(PeriodRange {
            start_ms,
            end_ms: Some(end_ms),
        })
    }

    /// Erster Zeitpunkt des Kalendertags `date` in dieser Zeitzone (in Millisekunden).
    fn day_start(&self, date: jiff::civil::Date) -> Option<i64> {
        let z: Zoned = date.to_zoned(self.tz.clone()).ok()?.start_of_day().ok()?;
        Some(z.timestamp().as_millisecond())
    }
}

/// Bezeichner eines Zeitraums wie in der Spalte `budget_usage.period`.
pub fn period_code(period: BudgetPeriod) -> &'static str {
    match period {
        BudgetPeriod::Day => "day",
        BudgetPeriod::Week => "week",
        BudgetPeriod::Month => "month",
        BudgetPeriod::Total => "total",
    }
}

/// Gegenstück zu [`period_code`].
pub fn period_from_code(code: &str) -> Option<BudgetPeriod> {
    Some(match code {
        "day" => BudgetPeriod::Day,
        "week" => BudgetPeriod::Week,
        "month" => BudgetPeriod::Month,
        "total" => BudgetPeriod::Total,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use time::macros::datetime;

    use super::*;

    fn ms(t: time::OffsetDateTime) -> i64 {
        i64::try_from(t.unix_timestamp_nanos() / 1_000_000).unwrap()
    }

    #[test]
    fn utc_periods() {
        let c = Calendar::utc();
        let at = ms(datetime!(2026-11-13 15:30 UTC)); // Freitag
        let day = c.period(BudgetPeriod::Day, at);
        assert_eq!(day.start_ms, ms(datetime!(2026-11-13 0:00 UTC)));
        assert_eq!(day.end_ms, Some(ms(datetime!(2026-11-14 0:00 UTC))));
        let week = c.period(BudgetPeriod::Week, at);
        assert_eq!(week.start_ms, ms(datetime!(2026-11-09 0:00 UTC)));
        assert_eq!(week.end_ms, Some(ms(datetime!(2026-11-16 0:00 UTC))));
        let month = c.period(BudgetPeriod::Month, at);
        assert_eq!(month.start_ms, ms(datetime!(2026-11-01 0:00 UTC)));
        assert_eq!(month.end_ms, Some(ms(datetime!(2026-12-01 0:00 UTC))));
        let total = c.period(BudgetPeriod::Total, at);
        assert_eq!((total.start_ms, total.end_ms), (0, None));
        // Montag 00:00 gehört zur neuen Woche, Sonntag 23:59 zur alten.
        let mon = ms(datetime!(2026-11-16 0:00 UTC));
        assert_eq!(c.period(BudgetPeriod::Week, mon).start_ms, mon);
        let sun = ms(datetime!(2026-11-15 23:59:59 UTC));
        assert_eq!(c.period(BudgetPeriod::Week, sun), week);
        // Dezember → Januar.
        let dec = c.period(BudgetPeriod::Month, ms(datetime!(2026-12-31 23:00 UTC)));
        assert_eq!(dec.end_ms, Some(ms(datetime!(2027-01-01 0:00 UTC))));
    }

    #[test]
    fn berlin_with_daylight_saving() {
        let c = Calendar::new("Europe/Berlin").unwrap();
        assert_eq!(c.timezone(), "Europe/Berlin");
        // 23:30 UTC am 31.10. ist in Berlin schon der 1.11. (UTC+1).
        let at = ms(datetime!(2026-10-31 23:30 UTC));
        let month = c.period(BudgetPeriod::Month, at);
        assert_eq!(month.start_ms, ms(datetime!(2026-10-31 23:00 UTC)));
        assert_eq!(month.end_ms, Some(ms(datetime!(2026-11-30 23:00 UTC))));
        // Umstellung am 25.10.2026: der Tag hat 25 Stunden.
        let day = c.period(BudgetPeriod::Day, ms(datetime!(2026-10-25 12:00 UTC)));
        assert_eq!(day.start_ms, ms(datetime!(2026-10-24 22:00 UTC)));
        assert_eq!(day.end_ms, Some(ms(datetime!(2026-10-25 23:00 UTC))));
        // Oktober beginnt in Sommerzeit (UTC+2).
        let oct = c.period(BudgetPeriod::Month, ms(datetime!(2026-10-15 12:00 UTC)));
        assert_eq!(oct.start_ms, ms(datetime!(2026-09-30 22:00 UTC)));
        assert_eq!(oct.end_ms, Some(month.start_ms));
    }

    #[test]
    fn unknown_timezone_is_an_error() {
        assert!(Calendar::new("Mars/Olympus").is_err());
        assert_eq!(Calendar::new("utc").unwrap().timezone(), "UTC");
        assert_eq!(Calendar::new("").unwrap().timezone(), "UTC");
    }

    #[test]
    fn period_codes_roundtrip() {
        for p in [
            BudgetPeriod::Day,
            BudgetPeriod::Week,
            BudgetPeriod::Month,
            BudgetPeriod::Total,
        ] {
            assert_eq!(period_from_code(period_code(p)), Some(p));
        }
        assert_eq!(period_from_code("year"), None);
    }
}
