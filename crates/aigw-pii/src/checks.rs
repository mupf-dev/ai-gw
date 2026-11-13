// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Prüfziffern und Plausibilitätsregeln (ohne Allokation, auf kompakten Ziffern-/Zeichenfolgen).

/// Luhn-Prüfung über Ziffernwerte (0–9).
pub fn luhn(digits: &[u8]) -> bool {
    let mut sum = 0u32;
    for (i, &d) in digits.iter().rev().enumerate() {
        let d = u32::from(d);
        sum += if i % 2 == 1 {
            let x = d * 2;
            if x > 9 { x - 9 } else { x }
        } else {
            d
        };
    }
    sum.is_multiple_of(10)
}

fn prefix_num(digits: &[u8], n: usize) -> u32 {
    digits
        .iter()
        .take(n)
        .fold(0u32, |acc, &d| acc * 10 + u32::from(d))
}

/// Gehört die Ziffernfolge (Länge eingeschlossen) zu einem bekannten Kartennetz (BIN-Bereich)?
///
/// Visa, Mastercard (51–55, 2221–2720), Amex, Discover, Diners, JCB, UnionPay, Maestro, Mir.
pub fn card_brand_ok(digits: &[u8]) -> bool {
    let len = digits.len();
    if !(13..=19).contains(&len) {
        return false;
    }
    let p1 = prefix_num(digits, 1);
    let p2 = prefix_num(digits, 2);
    let p3 = prefix_num(digits, 3);
    let p4 = prefix_num(digits, 4);
    match p1 {
        // Visa
        4 => matches!(len, 13 | 16 | 19),
        // Mastercard 51–55; Maestro 50, 56–58
        5 => match p2 {
            51..=55 => len == 16,
            50 | 56..=58 => len >= 13,
            _ => false,
        },
        // Mastercard 2221–2720, Mir 2200–2204
        2 => (2221..=2720).contains(&p4) && len == 16 || (2200..=2204).contains(&p4) && len >= 16,
        3 => match p2 {
            // Amex
            34 | 37 => len == 15,
            // Diners 36, 38, 39
            36 | 38 | 39 => len >= 14,
            // Diners 300–305, JCB 3528–3589
            30 => (300..=305).contains(&p3) && len >= 14,
            35 => (3528..=3589).contains(&p4) && len >= 16,
            _ => false,
        },
        // Discover 6011, 644–649, 65; UnionPay 62; Maestro 6x
        6 => match p2 {
            60 => p4 == 6011 && len >= 16,
            62 | 65 => len >= 16,
            64 => (644..=649).contains(&p3) && len >= 16,
            63 | 67 => len >= 13, // Maestro
            _ => false,
        },
        _ => false,
    }
}

/// Länge der IBAN je Land (ISO 13616, SWIFT-Register); `None` = kein IBAN-Land.
pub fn iban_length(cc: &[u8]) -> Option<usize> {
    let len = match cc {
        b"NO" => 15,
        b"BE" => 16,
        b"DK" | b"FI" | b"FO" | b"GL" | b"NL" => 18,
        b"MK" | b"SI" => 19,
        b"AT" | b"BA" | b"EE" | b"KZ" | b"LT" | b"LU" | b"XK" => 20,
        b"CH" | b"HR" | b"LI" | b"LV" => 21,
        b"BG" | b"BH" | b"CR" | b"DE" | b"GB" | b"GE" | b"IE" | b"ME" | b"RS" | b"VA" => 22,
        b"AE" | b"GI" | b"IL" | b"IQ" | b"TL" => 23,
        b"AD" | b"CZ" | b"ES" | b"MD" | b"PK" | b"RO" | b"SA" | b"SE" | b"SK" | b"TN" | b"VG" => 24,
        b"PT" | b"ST" => 25,
        b"IS" | b"TR" => 26,
        b"FR" | b"GR" | b"IT" | b"MC" | b"MR" | b"SM" => 27,
        b"AL" | b"AZ" | b"BY" | b"CY" | b"DO" | b"GT" | b"HU" | b"LB" | b"PL" | b"SV" => 28,
        b"BR" | b"EG" | b"PS" | b"QA" | b"UA" => 29,
        b"JO" | b"KW" | b"MU" => 30,
        b"MT" | b"SC" => 31,
        b"LC" => 32,
        _ => return None,
    };
    Some(len)
}

/// IBAN-Prüfung (mod 97 = 1) über die kompakte Form (Großbuchstaben/Ziffern, ohne Leerzeichen).
pub fn iban_mod97(compact: &[u8]) -> bool {
    if compact.len() < 5 {
        return false;
    }
    let mut rem: u32 = 0;
    for &b in compact[4..].iter().chain(&compact[..4]) {
        let v = match b {
            b'0'..=b'9' => u32::from(b - b'0'),
            b'A'..=b'Z' => u32::from(b - b'A') + 10,
            _ => return false,
        };
        rem = if v >= 10 {
            (rem * 100 + v) % 97
        } else {
            (rem * 10 + v) % 97
        };
    }
    rem == 1
}

/// Deutsche Steuer-Identifikationsnummer (11 Ziffern): erste Ziffer ≠ 0, in den ersten zehn
/// Ziffern kommt genau eine Ziffer zwei- oder dreimal vor (dreimal nicht direkt
/// hintereinander), Prüfziffer nach ISO 7064 MOD 11,10.
pub fn tax_id_de(d: &[u8]) -> bool {
    if d.len() != 11 || d[0] == 0 {
        return false;
    }
    let mut counts = [0u8; 10];
    for &x in &d[..10] {
        counts[usize::from(x)] += 1;
    }
    let repeated: Vec<usize> = (0..10).filter(|&i| counts[i] > 1).collect();
    let [rep] = repeated.as_slice() else {
        return false;
    };
    match counts[*rep] {
        2 => {}
        3 => {
            if d[..10]
                .windows(3)
                .any(|w| w.iter().all(|&x| usize::from(x) == *rep))
            {
                return false;
            }
        }
        _ => return false,
    }
    let mut product = 10u32;
    for &x in &d[..10] {
        let mut sum = (u32::from(x) + product) % 10;
        if sum == 0 {
            sum = 10;
        }
        product = (sum * 2) % 11;
    }
    let check = match 11 - product {
        10 => 0,
        c => c,
    };
    check == u32::from(d[10])
}

/// Deutsche Rentenversicherungs-/Sozialversicherungsnummer (12 Zeichen
/// `BB TTMMJJ A SS P`): Bereichsnummer, Geburtsdatum (plausibel), Anfangsbuchstabe,
/// Seriennummer, Prüfziffer (Gewichte 2 1 2 5 7 1 2 1 2 1 2 1, Quersummen, mod 10).
pub fn svnr_de(s: &[u8; 12]) -> bool {
    let digit = |i: usize| s[i].is_ascii_digit();
    if !(0..8).all(digit) || !s[8].is_ascii_uppercase() || !(9..12).all(digit) {
        return false;
    }
    let v = |i: usize| u32::from(s[i] - b'0');
    let day = v(2) * 10 + v(3);
    let month = v(4) * 10 + v(5);
    if !(1..=31).contains(&day) || !(1..=12).contains(&month) {
        return false;
    }
    let letter = u32::from(s[8] - b'A') + 1;
    let digits = [
        v(0),
        v(1),
        v(2),
        v(3),
        v(4),
        v(5),
        v(6),
        v(7),
        letter / 10,
        letter % 10,
        v(9),
        v(10),
    ];
    const W: [u32; 12] = [2, 1, 2, 5, 7, 1, 2, 1, 2, 1, 2, 1];
    let sum: u32 = digits
        .iter()
        .zip(W)
        .map(|(d, w)| {
            let p = d * w;
            p / 10 + p % 10
        })
        .sum();
    sum % 10 == v(11)
}

/// Prüfziffer nach ICAO 9303 (Gewichte 7 3 1; Ziffern 0–9, Buchstaben A=10 … Z=35).
pub fn icao_check(s: &[u8]) -> Option<u8> {
    const W: [u32; 3] = [7, 3, 1];
    let mut sum = 0u32;
    for (i, &c) in s.iter().enumerate() {
        let v = match c {
            b'0'..=b'9' => u32::from(c - b'0'),
            b'A'..=b'Z' => u32::from(c - b'A') + 10,
            _ => return None,
        };
        sum += v * W[i % 3];
    }
    u8::try_from(sum % 10).ok()
}

/// Zeichen der Seriennummern deutscher Ausweisdokumente (seit 2010): Ziffern und
/// C F G H J K L M N P R T V W X Y Z (keine Vokale und verwechselbaren Buchstaben).
pub(crate) fn de_doc_char(c: u8) -> bool {
    c.is_ascii_digit() || b"CFGHJKLMNPRTVWXYZ".contains(&c)
}

/// Shannon-Entropie in Bit je Zeichen (ASCII).
pub fn shannon_bits(s: &[u8]) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let mut counts = [0u32; 128];
    for &c in s {
        counts[usize::from(c & 0x7f)] += 1;
    }
    #[allow(clippy::cast_precision_loss)] // Längen weit unter 2^52
    let n = s.len() as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = f64::from(c) / n;
            -p * p.log2()
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digits(s: &str) -> Vec<u8> {
        s.bytes()
            .filter(u8::is_ascii_digit)
            .map(|b| b - b'0')
            .collect()
    }

    #[test]
    fn luhn_and_brands() {
        for ok in [
            "4111111111111111",
            "5555555555554444",
            "2223003122003222",
            "378282246310005",
            "6011111111111117",
            "3530111333300000",
            "30569309025904",
            "6200000000000005",
        ] {
            let d = digits(ok);
            assert!(luhn(&d) && card_brand_ok(&d), "{ok}");
        }
        assert!(!luhn(&digits("4111111111111112")));
        // Luhn stimmt, aber kein Kartennetz.
        assert!(luhn(&digits("1111111111111117")));
        assert!(!card_brand_ok(&digits("1111111111111117")));
        // Visa mit 15 Stellen gibt es nicht.
        assert!(!card_brand_ok(&digits("411111111111111")));
    }

    #[test]
    fn iban() {
        assert!(iban_mod97(b"DE89370400440532013000"));
        assert!(iban_mod97(b"GB82WEST12345698765432"));
        assert!(!iban_mod97(b"DE89370400440532013001"));
        assert_eq!(iban_length(b"DE"), Some(22));
        assert_eq!(iban_length(b"US"), None);
    }

    #[test]
    fn tax_id() {
        for ok in ["86095742719", "47036892816", "65929970489", "57549285017"] {
            assert!(tax_id_de(&digits(ok)), "{ok}");
        }
        assert!(!tax_id_de(&digits("86095742718"))); // Prüfziffer
        assert!(!tax_id_de(&digits("01234567890"))); // führende 0
        assert!(!tax_id_de(&digits("12345678903"))); // keine Wiederholung
    }

    #[test]
    fn svnr() {
        assert!(svnr_de(b"15070649C103"));
        assert!(svnr_de(b"65170839J003"));
        assert!(!svnr_de(b"15070649C104"));
        assert!(!svnr_de(b"15320649C103")); // Tag 32
    }

    #[test]
    fn icao() {
        assert_eq!(icao_check(b"T22000129"), Some(3));
        assert_eq!(icao_check(b"C01X00T47"), Some(8));
        assert_eq!(icao_check(b"T2200012a"), None);
    }

    #[test]
    fn entropy() {
        assert!(shannon_bits(b"aaaa") < 0.01);
        assert!((shannon_bits(b"abcd") - 2.0).abs() < 1e-9);
    }
}
