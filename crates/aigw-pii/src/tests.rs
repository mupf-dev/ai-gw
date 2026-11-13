// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Positive und negative Korpora je Kategorie (inkl. typischer Falsch-Positive).

use super::*;

fn engine() -> Engine {
    Engine::default()
}

/// Fundstellen als (Kategorie, Text).
fn found(text: &str) -> Vec<(Category, String)> {
    engine()
        .find(text)
        .iter()
        .map(|f| (f.category, f.text(text).to_owned()))
        .collect()
}

#[track_caller]
fn hit(cat: Category, text: &str, expect: &str) {
    let f = found(text);
    assert!(
        f.iter().any(|(c, t)| *c == cat && t == expect),
        "{cat:?} {expect:?} nicht gefunden in {text:?}: {f:?}"
    );
}

#[track_caller]
fn clean(text: &str) {
    let f = found(text);
    assert!(f.is_empty(), "Falsch-Positiv in {text:?}: {f:?}");
}

#[test]
fn email() {
    hit(
        Category::Email,
        "Mail an alice.b+x@mail.example.org bitte",
        "alice.b+x@mail.example.org",
    );
    hit(Category::Email, "<bob@example.de>", "bob@example.de");
    hit(
        Category::Email,
        "{\"c\":\"\\nbob@example.com\"}",
        "bob@example.com",
    );
    clean("kein@ treffer");
    clean("@handle und foo@bar ohne TLD");
    clean("user@localhost");
    clean("Version 1.2@3.4");
}

#[test]
fn phone() {
    for (t, p) in [
        ("Ruf an: +49 30 1234567.", "+49 30 1234567"),
        ("Tel. +49 (0)30 12345678", "+49 (0)30 12345678"),
        ("mobil 0171 1234567", "0171 1234567"),
        ("Tel (030) 1234567", "(030) 1234567"),
        ("Fax 030/12345678", "030/12345678"),
        ("0049 30 1234567", "0049 30 1234567"),
        ("+1-212-555-0198", "+1-212-555-0198"),
        ("Nummer 01711234567", "01711234567"),
    ] {
        hit(Category::Phone, t, p);
    }
    for t in [
        "am 01.02.2024 um 10:30",
        "PLZ 01067 Dresden",
        "Seite 0 von 12",
        "Wert -0.123456789",
        "+5 Punkte",
        "Datum 01/02/2024",
        "Bestellnr. 1234567890",
        "Preis 0,99 €",
    ] {
        clean(t);
    }
}

#[test]
fn iban() {
    hit(
        Category::Iban,
        "IBAN DE89 3704 0044 0532 0130 00, danke",
        "DE89 3704 0044 0532 0130 00",
    );
    hit(
        Category::Iban,
        "DE89370400440532013000 EUR",
        "DE89370400440532013000",
    );
    hit(
        Category::Iban,
        "GB82WEST12345698765432",
        "GB82WEST12345698765432",
    );
    hit(
        Category::Iban,
        "AT611904300234573201",
        "AT611904300234573201",
    );
    clean("DE89370400440532013001"); // Prüfziffer
    clean("DE8937040044053201300"); // zu kurz für DE
    clean("US12345678901234567890"); // kein IBAN-Land
    clean("ISO 8601 und DE12 Abschnitt");
}

#[test]
fn credit_card() {
    hit(
        Category::CreditCard,
        "Karte 4111 1111 1111 1111.",
        "4111 1111 1111 1111",
    );
    hit(
        Category::CreditCard,
        "Karte 4111-1111-1111-1111",
        "4111-1111-1111-1111",
    );
    hit(
        Category::CreditCard,
        "{\"cc\":5555555555554444}",
        "5555555555554444",
    );
    hit(
        Category::CreditCard,
        "Amex 3782 822463 10005",
        "3782 822463 10005",
    );
    hit(
        Category::CreditCard,
        "4111 1111 1111 1111 22",
        "4111 1111 1111 1111",
    );
    clean("Nr 4111 1111 1111 1112"); // Luhn
    clean("ts 1731489600000"); // Zeitstempel
    clean("1111111111111117"); // Luhn ok, kein Kartennetz
    clean("Konto 4111111111111111999"); // 19 Ziffern, Luhn falsch
}

#[test]
fn tax_id_de() {
    hit(Category::TaxIdDe, "Steuer-ID: 86095742719", "86095742719");
    hit(Category::TaxIdDe, "IdNr 86 095 742 719", "86 095 742 719");
    clean("86095742718");
    clean("Rechnung 12345678903");
}

#[test]
fn social_security_de() {
    hit(
        Category::SocialSecurityDe,
        "SVNR 15070649C103.",
        "15070649C103",
    );
    hit(
        Category::SocialSecurityDe,
        "RV-Nr 15 070649 C 103",
        "15 070649 C 103",
    );
    clean("15070649C104");
    clean("Artikel 12345678A123");
}

#[test]
fn id_documents_de() {
    hit(Category::IdCardDe, "Ausweis T220001293", "T220001293");
    hit(Category::PassportDe, "Pass-Nr. C01X00T478", "C01X00T478");
    clean("Ausweis T220001294"); // Prüfziffer
    clean("HTTPSERVER1"); // Buchstaben außerhalb des Alphabets
    clean("TRXPWZKMN5"); // zu wenige Ziffern
}

#[test]
fn ip_addresses() {
    hit(
        Category::IpAddress,
        "Client 192.168.1.20 meldet",
        "192.168.1.20",
    );
    hit(
        Category::IpAddress,
        "von 2001:db8:85a3::8a2e:370:7334.",
        "2001:db8:85a3::8a2e:370:7334",
    );
    hit(
        Category::IpAddress,
        "link fe80::1ff:fe23:4567:890a",
        "fe80::1ff:fe23:4567:890a",
    );
    clean("Version 1.2.3 und 10.20");
    clean("1.2.3.4.5");
    clean("Zeit 12:30:45");
    clean("MAC 00:1A:2B:3C:4D:5E");
    clean("std::vec und foo::bar");
    clean("999.1.1.1");
}

#[test]
fn url_credentials() {
    hit(
        Category::UrlCredentials,
        "postgres://admin:s3cret@db.internal:5432/app",
        "admin:s3cret",
    );
    hit(Category::UrlCredentials, "https://u:p@example.com", "u:p");
    // Keine E-Mail aus dem Passwort-Teil.
    assert!(
        !found("https://user:pw@example.com/x")
            .iter()
            .any(|(c, _)| *c == Category::Email)
    );
    clean("ssh://git@github.com/org/repo");
    clean("https://example.com/path?a=b@c");
}

#[test]
fn api_keys() {
    for k in [
        "sk-proj-AbCdEf0123456789GhIjKl0123456789",
        "sk-ant-api03-AbCdEf0123456789GhIjKl012345",
        "ghp_1234567890abcdefghijABCDEFGHIJ123456",
        "AKIAIOSFODNN7EXAMPLE",
        "xoxb-123456789012-abcdefABCDEF",
        "glpat-AbCd1234EfGh5678IjKl",
        "AIzaSyA1234567890abcdefghijklmnopqrstuv",
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
    ] {
        hit(Category::ApiKey, &format!("key={k} ok"), k);
    }
    // Entropie-Heuristik.
    hit(
        Category::ApiKey,
        "token: Zx8Qm2Lp9RtV4Kw7Hy3Nc6Bd1Fg5Js0A",
        "Zx8Qm2Lp9RtV4Kw7Hy3Nc6Bd1Fg5Js0A",
    );
    for t in [
        "sk-learn ist eine Bibliothek",
        "sk-this-is-a-very-long-kebab-case-name",
        "ThisIsAVeryLongIdentifierName1234",
        "getUserAccountSettingsForTenant2024",
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "550e8400-e29b-41d4-a716-446655440000",
        "/usr/local/lib/python3.12/site-packages/numpy",
    ] {
        clean(t);
    }
}

#[test]
fn prose_has_no_false_positives() {
    clean(
        "Am 13.11.2026 um 14:05 Uhr trafen sich 12 Teilnehmer in Raum 3.14. Die Agenda \
         umfasste Punkt 1.2.3, Budget 1.250.000 EUR (Version v2.4.1), ISO-8601 und RFC 9457. \
         Bitte Ticket #4711 sowie PR 1234 prüfen; Preis 19,99 € bzw. 0,5 % Rabatt.",
    );
    clean("The quick brown fox jumps over the lazy dog. Call me maybe? See section 4.2.");
    clean("fn main() { let x: Vec<u8> = Vec::with_capacity(4096); println!(\"{x:?}\"); }");
}

#[test]
fn categories_and_allowlist() {
    let e = Engine::new(EngineConfig {
        categories: [Category::Email].into_iter().collect(),
        allow: vec!["support@acme.de".into(), "*@example.com".into()],
        entropy: None,
    });
    let t = "a@acme.de SUPPORT@acme.de x@example.com 4111111111111111";
    let f: Vec<&str> = e.find(t).iter().map(|f| f.text(t)).collect();
    assert_eq!(f, ["a@acme.de"]);

    let e = Engine::new(EngineConfig {
        categories: CategorySet::all(),
        allow: vec!["+49 30 1234567".into()],
        entropy: None,
    });
    assert!(e.find("Tel +49-30-1234567").is_empty());
}

#[test]
fn replace_and_labels() {
    let e = engine();
    assert!(matches!(e.mask_labels("nichts zu tun"), Cow::Borrowed(_)));
    assert_eq!(
        e.mask_labels("Ich bin bob@example.com, Karte 4111111111111111"),
        "Ich bin [EMAIL], Karte [CARD]"
    );
    let mut n = 0;
    let out = e.replace("a@b.de und c@d.de", |f, _, out| {
        n += 1;
        out.push_str(&format!("[{}_{n}]", f.category.label()));
    });
    assert_eq!(out, "[EMAIL_1] und [EMAIL_2]");
}

#[test]
fn findings_are_sorted_and_disjoint_on_mixed_text() {
    let t = "Mail max@firma.de, Tel +49 171 2345678, IBAN DE89370400440532013000, \
             IP 10.0.0.1, Key ghp_1234567890abcdefghijABCDEFGHIJ123456 und Karte \
             5555 5555 5555 4444.";
    let f = engine().find(t);
    let cats: Vec<Category> = f.iter().map(|f| f.category).collect();
    assert_eq!(
        cats,
        [
            Category::Email,
            Category::Phone,
            Category::Iban,
            Category::IpAddress,
            Category::ApiKey,
            Category::CreditCard
        ]
    );
    assert!(f.windows(2).all(|w| w[0].end <= w[1].start));
}

#[test]
fn category_names() {
    assert_eq!(Category::parse("creditCard"), Some(Category::CreditCard));
    assert_eq!(Category::parse("credit_card"), Some(Category::CreditCard));
    assert_eq!(Category::parse("TAX-ID-DE"), Some(Category::TaxIdDe));
    assert_eq!(Category::parse("name"), None);
}

#[test]
fn non_ascii_text_is_safe() {
    let t = "Grüße an jürgen@example.de – Tel. +49 30 1234567 ✓ äöü";
    let f = found(t);
    assert!(f.iter().any(|(c, _)| *c == Category::Phone));
    // Internationalisierter Local-Part.
    assert!(
        f.iter()
            .any(|(c, s)| *c == Category::Email && s == "jürgen@example.de")
    );
    let _ = engine().mask_labels(t);
}
