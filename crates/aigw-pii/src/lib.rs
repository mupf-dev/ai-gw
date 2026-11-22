// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! PII-Erkennung von ai-gw (AIGW-37, Doku `docs/konzept/13-guardrails.md`).
//!
//! Eigenentwicklung ohne Abhängigkeiten: ein Durchlauf über die Bytes des Texts, Erkenner
//! werden nur an Kandidatenstellen (Wortanfang, `@`, `://`, `+`, `(`) aufgerufen, Prüfziffern
//! und Plausibilitätsregeln senken Falsch-Positive. Ergebnis sind Byte-Bereiche
//! ([`Finding`]); Ersetzen ([`Engine::replace`]) allokiert nur, wenn etwas gefunden wurde.
//!
//! Genutzt von den Guardrail-Plugins (`aigw-plugins`, `pii`) und der Log-Maskierung
//! (`aigw-observe`, `PayloadMasker`) – deshalb eine eigene, schlanke Crate.
//!
//! Namen und Postadressen werden bewusst **nicht** erkannt (ohne NER-Modell zu
//! fehleranfällig); dafür ist ein externer Dienst über das `content-safety`- bzw.
//! Webhook-Plugin vorgesehen.

mod checks;
mod scan;

use std::borrow::Cow;
use std::fmt;

pub use checks::{
    card_brand_ok, iban_length, iban_mod97, icao_check, luhn, shannon_bits, svnr_de, tax_id_de,
};

/// Kategorie personenbezogener bzw. geheimer Daten.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Category {
    /// E-Mail-Adresse.
    Email,
    /// Telefonnummer (international mit `+`/`00`, deutsch national mit `0`).
    Phone,
    /// IBAN (Länderlänge + mod 97).
    Iban,
    /// Kreditkartennummer (BIN-Bereich + Luhn).
    CreditCard,
    /// Deutsche Steuer-Identifikationsnummer (Prüfziffer).
    TaxIdDe,
    /// Deutsche Sozialversicherungsnummer (Prüfziffer).
    SocialSecurityDe,
    /// Deutscher Personalausweis (Seriennummer + Prüfziffer).
    IdCardDe,
    /// Deutscher Reisepass (Seriennummer + Prüfziffer).
    PassportDe,
    /// IPv4- oder IPv6-Adresse.
    IpAddress,
    /// Zugangsdaten in einer URL (`scheme://user:pass@host`).
    UrlCredentials,
    /// API-Key/Token (bekannte Präfixe, JWT, Entropie-Heuristik).
    ApiKey,
}

impl Category {
    /// Alle Kategorien.
    pub const ALL: [Self; 11] = [
        Self::Email,
        Self::Phone,
        Self::Iban,
        Self::CreditCard,
        Self::TaxIdDe,
        Self::SocialSecurityDe,
        Self::IdCardDe,
        Self::PassportDe,
        Self::IpAddress,
        Self::UrlCredentials,
        Self::ApiKey,
    ];

    /// Maschinenname (Metrik-Label, Fehlermeldung), z. B. `credit_card`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Email => "email",
            Self::Phone => "phone",
            Self::Iban => "iban",
            Self::CreditCard => "credit_card",
            Self::TaxIdDe => "tax_id_de",
            Self::SocialSecurityDe => "social_security_de",
            Self::IdCardDe => "id_card_de",
            Self::PassportDe => "passport_de",
            Self::IpAddress => "ip_address",
            Self::UrlCredentials => "url_credentials",
            Self::ApiKey => "api_key",
        }
    }

    /// Typ im Platzhalter (`[EMAIL]`, `[EMAIL_1]`).
    pub fn label(self) -> &'static str {
        match self {
            Self::Email => "EMAIL",
            Self::Phone => "PHONE",
            Self::Iban => "IBAN",
            Self::CreditCard => "CARD",
            Self::TaxIdDe => "TAX_ID",
            Self::SocialSecurityDe => "SVNR",
            Self::IdCardDe => "ID_CARD",
            Self::PassportDe => "PASSPORT",
            Self::IpAddress => "IP",
            Self::UrlCredentials => "URL_CREDENTIALS",
            Self::ApiKey => "API_KEY",
        }
    }

    /// Liest einen Namen: `credit_card`, `creditCard`, `credit-card` (Groß-/Kleinschreibung
    /// egal).
    pub fn parse(s: &str) -> Option<Self> {
        let norm = |x: &str| -> String {
            x.chars()
                .filter(|c| !matches!(c, '_' | '-'))
                .map(|c| c.to_ascii_lowercase())
                .collect()
        };
        let want = norm(s);
        Self::ALL.into_iter().find(|c| norm(c.as_str()) == want)
    }

    fn bit(self) -> u16 {
        1 << (self as u8)
    }
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Menge von Kategorien (Bitmaske).
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct CategorySet(u16);

impl CategorySet {
    /// Leere Menge.
    pub const EMPTY: Self = Self(0);

    /// Alle Kategorien.
    pub fn all() -> Self {
        Category::ALL.into_iter().collect()
    }

    /// Enthält `c`?
    pub fn contains(self, c: Category) -> bool {
        self.0 & c.bit() != 0
    }

    /// Fügt `c` hinzu.
    pub fn insert(&mut self, c: Category) {
        self.0 |= c.bit();
    }

    /// Leer?
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Kategorien in fester Reihenfolge.
    pub fn iter(self) -> impl Iterator<Item = Category> {
        Category::ALL.into_iter().filter(move |c| self.contains(*c))
    }
}

impl FromIterator<Category> for CategorySet {
    fn from_iter<I: IntoIterator<Item = Category>>(iter: I) -> Self {
        let mut s = Self::EMPTY;
        for c in iter {
            s.insert(c);
        }
        s
    }
}

impl fmt::Debug for CategorySet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

/// Fundstelle: Kategorie und Byte-Bereich im untersuchten Text (auf Zeichengrenzen).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Finding {
    /// Kategorie.
    pub category: Category,
    /// Anfang (Byte-Index).
    pub start: usize,
    /// Ende (exklusiv).
    pub end: usize,
}

impl Finding {
    /// Text der Fundstelle in `text`.
    pub fn text<'a>(&self, text: &'a str) -> &'a str {
        text.get(self.start..self.end).unwrap_or("")
    }
}

/// Entropie-Heuristik für unbekannte Token (Teil der Kategorie [`Category::ApiKey`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EntropyConfig {
    /// Mindestlänge des Tokens.
    pub min_len: usize,
    /// Höchstlänge (längere Folgen sind eher eingebettete Daten als Schlüssel).
    pub max_len: usize,
    /// Mindest-Entropie in Bit je Zeichen.
    pub min_bits: f64,
}

impl Default for EntropyConfig {
    fn default() -> Self {
        Self {
            min_len: 32,
            max_len: 256,
            min_bits: 4.0,
        }
    }
}

/// Konfiguration der Erkennung.
#[derive(Debug, Clone, Default)]
pub struct EngineConfig {
    /// Gesuchte Kategorien.
    pub categories: CategorySet,
    /// Werte, die nie als Fund gelten. Vergleich ohne Groß-/Kleinschreibung und ohne
    /// Leerzeichen, `-`, `/`, `(`, `)`; ein führendes `*` vergleicht nur das Ende
    /// (`*@example.com`).
    pub allow: Vec<String>,
    /// Entropie-Heuristik für API-Keys (`None` = nur bekannte Präfixe/JWT).
    pub entropy: Option<EntropyConfig>,
}

impl EngineConfig {
    /// Alle Kategorien, Entropie-Heuristik mit Standardwerten, keine Allowlist.
    pub fn all() -> Self {
        Self {
            categories: CategorySet::all(),
            allow: Vec::new(),
            entropy: Some(EntropyConfig::default()),
        }
    }
}

#[derive(Debug, Clone)]
struct AllowEntry {
    /// Normalisiert (klein, ohne Trenner).
    value: Box<[u8]>,
    suffix: bool,
}

/// PII-Erkennung (unveränderlich, `Send + Sync`, günstig zu teilen).
#[derive(Debug, Clone)]
pub struct Engine {
    categories: CategorySet,
    allow: Box<[AllowEntry]>,
    entropy: Option<EntropyConfig>,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new(EngineConfig::all())
    }
}

fn norm_bytes(s: &str) -> impl Iterator<Item = u8> + '_ {
    s.bytes()
        .filter(|b| !matches!(b, b' ' | b'-' | b'/' | b'(' | b')' | b'\t'))
        .map(|b| b.to_ascii_lowercase())
}

impl Engine {
    /// Baut die Erkennung.
    pub fn new(cfg: EngineConfig) -> Self {
        let allow = cfg
            .allow
            .iter()
            .filter(|a| !a.trim().is_empty())
            .map(|a| {
                let (suffix, v) = match a.trim().strip_prefix('*') {
                    Some(rest) => (true, rest),
                    None => (false, a.trim()),
                };
                AllowEntry {
                    value: norm_bytes(v).collect(),
                    suffix,
                }
            })
            .collect();
        Self {
            categories: cfg.categories,
            allow,
            entropy: cfg.entropy,
        }
    }

    /// Gesuchte Kategorien.
    pub fn categories(&self) -> CategorySet {
        self.categories
    }

    fn allowed(&self, s: &str) -> bool {
        if self.allow.is_empty() {
            return false;
        }
        self.allow.iter().any(|a| {
            if a.suffix {
                let n: Vec<u8> = norm_bytes(s).collect();
                n.ends_with(&a.value)
            } else {
                norm_bytes(s).eq(a.value.iter().copied())
            }
        })
    }

    /// Hängt die Fundstellen in `text` an `out` an (aufsteigend, nicht überlappend).
    pub fn scan(&self, text: &str, out: &mut Vec<Finding>) {
        if self.categories.is_empty() {
            return;
        }
        let base = out.len();
        scan::Scanner {
            engine: self,
            text,
            b: text.as_bytes(),
            out,
            base,
        }
        .run();
    }

    /// Fundstellen in `text`.
    pub fn find(&self, text: &str) -> Vec<Finding> {
        let mut v = Vec::new();
        self.scan(text, &mut v);
        v
    }

    /// Ersetzt jede Fundstelle durch das, was `f` an den Puffer anhängt. Ohne Fund wird
    /// `text` unverändert ausgeliehen (keine Allokation).
    pub fn replace<'a>(
        &self,
        text: &'a str,
        mut f: impl FnMut(&Finding, &str, &mut String),
    ) -> Cow<'a, str> {
        let mut found = Vec::new();
        self.scan(text, &mut found);
        if found.is_empty() {
            return Cow::Borrowed(text);
        }
        let mut out = String::with_capacity(text.len() + 16);
        let mut last = 0;
        for fd in &found {
            out.push_str(&text[last..fd.start]);
            f(fd, &text[fd.start..fd.end], &mut out);
            last = fd.end;
        }
        out.push_str(&text[last..]);
        Cow::Owned(out)
    }

    /// Ersetzt jede Fundstelle durch `[LABEL]` (Log-Maskierung).
    pub fn mask_labels<'a>(&self, text: &'a str) -> Cow<'a, str> {
        self.replace(text, |fd, _, out| {
            out.push('[');
            out.push_str(fd.category.label());
            out.push(']');
        })
    }
}

#[cfg(test)]
mod tests;
