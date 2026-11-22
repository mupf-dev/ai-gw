// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Dezimalzahlen aus fremden Quellen (JSON-Zahlen wie `2.5e-05`) exakt in
//! [`Amount`](aigw_core::resource::Amount) umrechnen – ohne Umweg über `f64`-Arithmetik.

use aigw_core::resource::Amount;

/// Wandelt die Dezimalzahl `s` (optional mit Exponent) mal `10^shift` in einen Betrag
/// (Nano-Einheiten, kaufmännisch gerundet). `None` bei ungültiger Eingabe, negativen Werten
/// oder Überlauf.
///
/// `shift` = 3 rechnet einen Preis je 1 000 Tokens in einen Preis je 1 Mio. Tokens um,
/// `shift` = 6 einen Preis je Token.
pub fn scaled_amount(s: &str, shift: i32) -> Option<Amount> {
    let s = s.trim();
    let (mantissa, exp) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], s[i + 1..].parse::<i32>().ok()?),
        None => (s, 0),
    };
    if mantissa.starts_with('-') {
        return None;
    }
    let mantissa = mantissa.strip_prefix('+').unwrap_or(mantissa);
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if int.is_empty() && frac.is_empty() {
        return None;
    }
    if !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Ziffern ohne führende Nullen (Begrenzung gegen Überlauf), Wert = digits × 10^e10.
    let digits = format!("{int}{frac}");
    let digits = digits.trim_start_matches('0');
    let frac_len = i32::try_from(frac.len()).ok()?;
    let e10 = exp
        .checked_sub(frac_len)?
        .checked_add(shift)?
        .checked_add(9)?;
    if digits.is_empty() {
        return Some(Amount::ZERO);
    }
    if digits.len() > 30 {
        return None;
    }
    let n: i128 = digits.parse().ok()?;
    let nanos = if e10 >= 0 {
        n.checked_mul(10i128.checked_pow(u32::try_from(e10).ok()?)?)?
    } else {
        let div = 10i128.checked_pow(u32::try_from(-e10).ok()?);
        match div {
            Some(d) => (n + d / 2) / d,
            None => 0,
        }
    };
    i64::try_from(nanos).ok().map(Amount::from_nanos)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str, shift: i32) -> Option<String> {
        scaled_amount(v, shift).map(|a| a.to_string())
    }

    #[test]
    fn converts_exactly() {
        assert_eq!(s("2.5e-05", 3).as_deref(), Some("0.025"));
        assert_eq!(s("0.000025", 3).as_deref(), Some("0.025"));
        assert_eq!(s("0.00066", 3).as_deref(), Some("0.66"));
        assert_eq!(s("4.5", 0).as_deref(), Some("4.5"));
        assert_eq!(s("7.5e-07", 6).as_deref(), Some("0.75"));
        assert_eq!(s("1.4e-05", 3).as_deref(), Some("0.014"));
        assert_eq!(s("0", 3).as_deref(), Some("0"));
        assert_eq!(s("3E+1", 0).as_deref(), Some("30"));
        // Rundung auf Nano-Einheiten.
        assert_eq!(s("0.0000000014", 0).as_deref(), Some("0.000000001"));
        assert_eq!(s("1e-30", 0).as_deref(), Some("0"));
    }

    #[test]
    fn rejects_invalid() {
        assert_eq!(s("-1", 0), None);
        assert_eq!(s("abc", 0), None);
        assert_eq!(s(".", 0), None);
        assert_eq!(s("1e400", 0), None);
        assert_eq!(s("99999999999999999999", 0), None);
    }
}
