// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Geldbeträge als Festkommazahl.
//!
//! Beträge werden intern als ganze **Nano-Einheiten** (10⁻⁹ der Abrechnungswährung) in `i64`
//! geführt: exakt summierbar (kein Float-Rundungsfehler in Rollups und Budgets), bis rund
//! 9,2 Mrd. Währungseinheiten. Nach außen (API, YAML) erscheinen sie als Dezimal-Zeichenkette,
//! z. B. `"2.50"`, damit keine Präzision durch JSON-Zahlen verloren geht.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

const SCALE: i64 = 1_000_000_000;
const FRACTION_DIGITS: usize = 9;

/// Geldbetrag in Nano-Einheiten der Abrechnungswährung.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, utoipa::ToSchema)]
#[schema(value_type = String, example = "2.50")]
pub struct Amount(i64);

impl Amount {
    /// Null.
    pub const ZERO: Self = Self(0);

    /// Erzeugt einen Betrag aus Nano-Einheiten.
    pub const fn from_nanos(nanos: i64) -> Self {
        Self(nanos)
    }

    /// Betrag in Nano-Einheiten (für Datenbank und Zähler).
    pub const fn nanos(self) -> i64 {
        self.0
    }

    /// Kosten für `tokens` Tokens, wenn `self` der Preis je 1 Mio. Tokens ist (abgerundet).
    pub fn per_million(self, tokens: u64) -> Self {
        let v = i128::from(self.0) * i128::from(tokens) / 1_000_000;
        Self(i64::try_from(v).unwrap_or(i64::MAX))
    }

    /// Sättigende Addition.
    #[must_use]
    pub const fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }
}

/// Fehler beim Parsen eines Betrags.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("ungültiger Betrag {0:?} (erwartet Dezimalzahl mit höchstens 9 Nachkommastellen)")]
pub struct ParseAmountError(String);

impl FromStr for Amount {
    type Err = ParseAmountError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ParseAmountError(s.to_owned());
        let t = s.trim();
        let (neg, t) = match t.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, t),
        };
        let (int, frac) = t.split_once('.').unwrap_or((t, ""));
        if (int.is_empty() && frac.is_empty())
            || frac.len() > FRACTION_DIGITS
            || !int.bytes().all(|b| b.is_ascii_digit())
            || !frac.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(err());
        }
        let int: i64 = if int.is_empty() {
            0
        } else {
            int.parse().map_err(|_| err())?
        };
        let mut frac_val: i64 = if frac.is_empty() {
            0
        } else {
            frac.parse().map_err(|_| err())?
        };
        for _ in frac.len()..FRACTION_DIGITS {
            frac_val *= 10;
        }
        let v = int
            .checked_mul(SCALE)
            .and_then(|v| v.checked_add(frac_val))
            .ok_or_else(err)?;
        Ok(Self(if neg { -v } else { v }))
    }
}

impl fmt::Display for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "" };
        let abs = self.0.unsigned_abs();
        let scale = SCALE.unsigned_abs();
        let int = abs / scale;
        let frac = abs % scale;
        if frac == 0 {
            return write!(f, "{sign}{int}");
        }
        let frac = format!("{frac:09}");
        write!(f, "{sign}{int}.{}", frac.trim_end_matches('0'))
    }
}

impl Serialize for Amount {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Amount {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        /// Akzeptiert Zeichenketten und – für bequemes YAML – auch Zahlen.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Str(String),
            Int(i64),
            Float(f64),
        }
        let s = match Raw::deserialize(d)? {
            Raw::Str(s) => s,
            Raw::Int(i) => i.to_string(),
            // Gleitkommazahlen über die kürzeste Darstellung, die zurück zur Zahl führt.
            Raw::Float(f) => f.to_string(),
        };
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_format_roundtrip() {
        for (s, nanos, back) in [
            ("2.50", 2_500_000_000, "2.5"),
            ("0.000000001", 1, "0.000000001"),
            ("15", 15 * SCALE, "15"),
            ("-1.25", -1_250_000_000, "-1.25"),
            (".5", 500_000_000, "0.5"),
        ] {
            let a: Amount = s.parse().unwrap();
            assert_eq!(a.nanos(), nanos, "{s}");
            assert_eq!(a.to_string(), back);
        }
        for bad in ["", "1.0000000001", "abc", "1.2.3", "-", "1e3"] {
            assert!(bad.parse::<Amount>().is_err(), "{bad}");
        }
    }

    #[test]
    fn per_million_tokens() {
        // 2,50 je 1 Mio. Tokens → 1000 Tokens kosten 0,0025.
        let price: Amount = "2.50".parse().unwrap();
        assert_eq!(price.per_million(1000).to_string(), "0.0025");
    }

    #[test]
    fn serde_accepts_numbers_and_strings() {
        let a: Amount = serde_json::from_str("\"0.15\"").unwrap();
        let b: Amount = serde_json::from_str("0.15").unwrap();
        let c: Amount = serde_json::from_str("3").unwrap();
        assert_eq!(a, b);
        assert_eq!(c.nanos(), 3 * SCALE);
        assert_eq!(serde_json::to_string(&a).unwrap(), "\"0.15\"");
    }
}
