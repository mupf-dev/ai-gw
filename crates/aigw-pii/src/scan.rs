// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Ein Durchlauf über die Bytes; Erkenner nur an Kandidatenstellen.
//!
//! - Wortanfang (alphanumerisch, davor kein Wortzeichen): je nach erstem Zeichen IPv4/IPv6,
//!   SVNR, Kreditkarte, Steuer-ID, Telefon (`0…`), IBAN, API-Key-Präfixe, Ausweis/Pass,
//!   Entropie-Token. Ohne Fund springt der Scanner ans Wortende.
//! - `@`: E-Mail (Local-Part rückwärts, Domain vorwärts).
//! - `://`: Zugangsdaten in URLs (`user:pass@`).
//! - `+`, `(`: Telefonnummern.
//!
//! Nach einem Fund geht es hinter dessen Ende weiter (keine Überlappungen); eine E-Mail
//! verdrängt frühere Funde in ihrem Local-Part.

use std::net::Ipv6Addr;

use crate::checks::{
    card_brand_ok, de_doc_char, iban_length, iban_mod97, icao_check, luhn, shannon_bits, svnr_de,
    tax_id_de,
};
use crate::{Category, Engine, Finding};

#[inline]
fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

#[inline]
fn is_token(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
}

/// JSON-Escape `\n`, `\t`, `\r` vor `i` (im Request-Log stehen rohe JSON-Bodies).
#[inline]
fn escape_before(b: &[u8], i: usize) -> bool {
    i >= 2 && b[i - 2] == b'\\' && matches!(b[i - 1], b'n' | b't' | b'r')
}

#[inline]
fn boundary_before(b: &[u8], i: usize) -> bool {
    i == 0 || !is_word(b[i - 1]) || escape_before(b, i)
}

#[inline]
fn boundary_after(b: &[u8], e: usize) -> bool {
    e >= b.len() || !is_word(b[e])
}

#[inline]
fn run_end(b: &[u8], mut i: usize, f: impl Fn(u8) -> bool) -> usize {
    while i < b.len() && f(b[i]) {
        i += 1;
    }
    i
}

/// Bekannte Token-Präfixe (Kleinbuchstaben-Anfang) und Mindestlänge des ganzen Tokens.
const LOWER_KEYS: &[(&[u8], usize)] = &[
    (b"sk-ant-", 32),
    (b"sk-proj-", 32),
    (b"sk-", 23),
    (b"sk_live_", 24),
    (b"sk_test_", 24),
    (b"rk_live_", 24),
    (b"rk_test_", 24),
    (b"whsec_", 30),
    (b"ghp_", 40),
    (b"gho_", 40),
    (b"ghu_", 40),
    (b"ghs_", 40),
    (b"ghr_", 40),
    (b"github_pat_", 50),
    (b"xoxb-", 20),
    (b"xoxp-", 20),
    (b"xoxa-", 20),
    (b"xoxe-", 20),
    (b"xoxs-", 20),
    (b"xapp-", 20),
    (b"glpat-", 26),
    (b"hf_", 34),
    (b"npm_", 40),
    (b"pypi-", 50),
];

/// Maximal gemerkte Zifferngruppen (Karte/Steuer-ID).
const MAX_GROUPS: usize = 12;

struct Digits {
    d: [u8; 20],
    /// (Ziffern bis hier, Byte-Ende der Gruppe)
    ends: [(usize, usize); MAX_GROUPS],
    groups: usize,
}

pub(crate) struct Scanner<'e, 't, 'o> {
    pub(crate) engine: &'e Engine,
    pub(crate) text: &'t str,
    pub(crate) b: &'t [u8],
    pub(crate) out: &'o mut Vec<Finding>,
    /// Funde vor `base` gehören nicht zu diesem Lauf.
    pub(crate) base: usize,
}

impl Scanner<'_, '_, '_> {
    #[inline]
    fn on(&self, c: Category) -> bool {
        self.engine.categories.contains(c)
    }

    fn push(&mut self, category: Category, start: usize, end: usize) {
        if self.engine.allowed(&self.text[start..end]) {
            return;
        }
        self.out.push(Finding {
            category,
            start,
            end,
        });
    }

    pub(crate) fn run(mut self) {
        let b = self.b;
        let n = b.len();
        let mut i = 0;
        while i < n {
            let c = b[i];
            match c {
                b'@' => {
                    if self.on(Category::Email)
                        && let Some(e) = self.email(i)
                    {
                        i = e;
                        continue;
                    }
                    i += 1;
                }
                b':' => {
                    if self.on(Category::UrlCredentials)
                        && b[i + 1..].starts_with(b"//")
                        && let Some(e) = self.url_credentials(i)
                    {
                        i = e;
                        continue;
                    }
                    i += 1;
                }
                b'+' | b'(' => {
                    if self.on(Category::Phone)
                        && boundary_before(b, i)
                        && let Some(e) = self.phone(i)
                    {
                        self.push(Category::Phone, i, e);
                        i = e;
                        continue;
                    }
                    i += 1;
                }
                _ if c.is_ascii_alphanumeric() => {
                    if matches!(c, b'n' | b't' | b'r') && i > 0 && b[i - 1] == b'\\' {
                        // JSON-Escape: der Buchstabe gehört nicht zum folgenden Wort.
                        i += 1;
                        continue;
                    }
                    let w_end = run_end(b, i, |c| c.is_ascii_alphanumeric());
                    if boundary_before(b, i)
                        && let Some((cat, e)) = self.word(i)
                    {
                        self.push(cat, i, e);
                        i = e.max(i + 1);
                        continue;
                    }
                    i = w_end;
                }
                _ => i += 1,
            }
        }
    }

    /// Erkenner am Wortanfang `i`.
    fn word(&self, i: usize) -> Option<(Category, usize)> {
        let c = self.b[i];
        if c.is_ascii_digit() {
            if self.on(Category::IpAddress) {
                if let Some(e) = self.ipv4(i) {
                    return Some((Category::IpAddress, e));
                }
                if let Some(e) = self.ipv6(i) {
                    return Some((Category::IpAddress, e));
                }
            }
            if self.on(Category::SocialSecurityDe)
                && let Some(e) = self.svnr(i)
            {
                return Some((Category::SocialSecurityDe, e));
            }
            if self.on(Category::CreditCard) || self.on(Category::TaxIdDe) {
                let g = self.digit_groups(i, b" -");
                if self.on(Category::CreditCard)
                    && let Some(e) = card(&g, self.b)
                {
                    return Some((Category::CreditCard, e));
                }
                if self.on(Category::TaxIdDe) {
                    let g = self.digit_groups(i, b" ");
                    if let Some(e) = tax_id(&g, self.b) {
                        return Some((Category::TaxIdDe, e));
                    }
                }
            }
            if self.on(Category::Phone)
                && c == b'0'
                && let Some(e) = self.phone(i)
            {
                return Some((Category::Phone, e));
            }
        } else if c.is_ascii_uppercase() {
            if self.on(Category::Iban)
                && let Some(e) = self.iban(i)
            {
                return Some((Category::Iban, e));
            }
            if self.on(Category::ApiKey)
                && let Some(e) = self.upper_key(i)
            {
                return Some((Category::ApiKey, e));
            }
            if let Some(r) = self.id_document(i) {
                return Some(r);
            }
            if self.on(Category::IpAddress)
                && let Some(e) = self.ipv6(i)
            {
                return Some((Category::IpAddress, e));
            }
        } else {
            if self.on(Category::ApiKey)
                && let Some(e) = self.lower_key(i)
            {
                return Some((Category::ApiKey, e));
            }
            if self.on(Category::IpAddress)
                && let Some(e) = self.ipv6(i)
            {
                return Some((Category::IpAddress, e));
            }
        }
        if self.on(Category::ApiKey)
            && let Some(e) = self.entropy_token(i)
        {
            return Some((Category::ApiKey, e));
        }
        None
    }

    fn email(&mut self, at: usize) -> Option<usize> {
        let b = self.b;
        let n = b.len();
        // Nicht-ASCII-Bytes zählen mit (internationalisierte Local-Parts, RFC 6531); der
        // Anfang liegt so immer auf einer Zeichengrenze.
        let is_local = |c: u8| {
            c.is_ascii_alphanumeric() || c >= 0x80 || matches!(c, b'.' | b'_' | b'%' | b'+' | b'-')
        };
        let mut s = at;
        while s > 0 && is_local(b[s - 1]) && at - s < 64 {
            s -= 1;
        }
        if s > 0 && is_local(b[s - 1]) {
            return None; // Local-Part zu lang
        }
        if s < at && escape_before(b, s + 1) {
            s += 1;
        }
        while s < at && b[s] == b'.' {
            s += 1;
        }
        // `scheme://user@host` ohne Passwort ist keine E-Mail-Adresse.
        if s == at || (s >= 2 && b[s - 1] == b'/' && b[s - 2] == b'/') {
            return None;
        }
        let mut j = at + 1;
        let mut labels = 0;
        let mut tld = (j, j);
        loop {
            let ls = j;
            while j < n && (b[j].is_ascii_alphanumeric() || b[j] == b'-') && j - ls < 63 {
                j += 1;
            }
            if j == ls {
                break;
            }
            labels += 1;
            tld = (ls, j);
            if j + 1 < n && b[j] == b'.' && b[j + 1].is_ascii_alphanumeric() {
                j += 1;
            } else {
                break;
            }
        }
        let end = tld.1;
        let t = &b[tld.0..tld.1];
        if labels < 2 || !(2..=24).contains(&t.len()) || !t.iter().all(u8::is_ascii_alphabetic) {
            return None;
        }
        while self.out.len() > self.base && self.out.last().is_some_and(|f| f.end > s) {
            self.out.pop();
        }
        self.push(Category::Email, s, end);
        Some(end)
    }

    /// `i` zeigt auf `:` von `://`.
    fn url_credentials(&mut self, i: usize) -> Option<usize> {
        let b = self.b;
        if i == 0 || !b[i - 1].is_ascii_alphabetic() {
            return None;
        }
        let st = i + 3;
        let mut j = st;
        let mut at = None;
        while j < b.len() {
            match b[j] {
                b'/' | b'?' | b'#' | b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'' | b'<' | b'>'
                | b'\\' | b'`' => break,
                b'@' => at = Some(j),
                _ => {}
            }
            j += 1;
        }
        let at = at?;
        let colon = b[st..at].iter().position(|&c| c == b':')?;
        if colon == 0 || st + colon + 1 == at {
            return None;
        }
        self.push(Category::UrlCredentials, st, at);
        Some(at + 1)
    }

    fn ipv4(&self, i: usize) -> Option<usize> {
        let b = self.b;
        let n = b.len();
        if i >= 2 && b[i - 1] == b'.' && b[i - 2].is_ascii_digit() {
            return None;
        }
        let mut j = i;
        for k in 0..4 {
            let s = j;
            let mut v = 0u32;
            while j < n && b[j].is_ascii_digit() && j - s < 3 {
                v = v * 10 + u32::from(b[j] - b'0');
                j += 1;
            }
            if j == s || v > 255 || (j < n && b[j].is_ascii_digit()) {
                return None;
            }
            if k < 3 {
                if j < n && b[j] == b'.' {
                    j += 1;
                } else {
                    return None;
                }
            }
        }
        if j < n && (is_word(b[j]) || (b[j] == b'.' && j + 1 < n && b[j + 1].is_ascii_digit())) {
            return None;
        }
        Some(j)
    }

    fn ipv6(&self, i: usize) -> Option<usize> {
        let b = self.b;
        // Schnell: der erste Block muss aus 1–4 Hex-Ziffern bestehen, gefolgt von `:`.
        let h = run_end(b, i, |c| c.is_ascii_hexdigit());
        if h - i > 4 || b.get(h) != Some(&b':') {
            return None;
        }
        let mut j = i;
        while j < b.len()
            && j - i < 45
            && (b[j].is_ascii_hexdigit() || b[j] == b':' || b[j] == b'.')
        {
            j += 1;
        }
        while j > i && b[j - 1] == b'.' {
            j -= 1;
        }
        if j >= i + 2 && b[j - 1] == b':' && b[j - 2] != b':' {
            j -= 1;
        }
        if !boundary_after(b, j) {
            return None;
        }
        let s = &self.text[i..j];
        if s.bytes().filter(|&c| c == b':').count() < 2 {
            return None;
        }
        s.parse::<Ipv6Addr>().ok()?;
        let groups = s.split(':').filter(|g| !g.is_empty()).count();
        let ok = groups >= 3
            || (groups == 2
                && s.bytes().any(|c| c.is_ascii_digit())
                && s.split(':').any(|g| g.len() >= 4));
        ok.then_some(j)
    }

    fn svnr(&self, i: usize) -> Option<usize> {
        let b = self.b;
        let n = b.len();
        if i + 1 >= n || !b[i + 1].is_ascii_digit() {
            return None;
        }
        let mut buf = [0u8; 12];
        let mut k = 0;
        let mut j = i;
        while k < 12 && j < n {
            let c = b[j];
            if c.is_ascii_alphanumeric() {
                buf[k] = c.to_ascii_uppercase();
                k += 1;
                j += 1;
            } else if c == b' ' && k > 0 && j + 1 < n && b[j + 1].is_ascii_alphanumeric() {
                j += 1;
            } else {
                break;
            }
        }
        if k < 12 || !boundary_after(b, j) {
            return None;
        }
        svnr_de(&buf).then_some(j)
    }

    fn digit_groups(&self, i: usize, seps: &[u8]) -> Digits {
        let b = self.b;
        let n = b.len();
        let mut g = Digits {
            d: [0; 20],
            ends: [(0, 0); MAX_GROUPS],
            groups: 0,
        };
        let mut nd = 0;
        let mut j = i;
        loop {
            let s = j;
            while j < n && b[j].is_ascii_digit() {
                if nd < g.d.len() {
                    g.d[nd] = b[j] - b'0';
                }
                nd += 1;
                j += 1;
            }
            if j == s || nd > 19 || g.groups == MAX_GROUPS {
                break;
            }
            g.ends[g.groups] = (nd, j);
            g.groups += 1;
            if j + 1 < n && seps.contains(&b[j]) && b[j + 1].is_ascii_digit() {
                j += 1;
            } else {
                break;
            }
        }
        g
    }

    fn phone(&self, i: usize) -> Option<usize> {
        let b = self.b;
        let n = b.len();
        let at = |k: usize| b.get(k).copied().unwrap_or(0);
        let mut j = i;
        if at(j) == b'(' {
            j += 1;
        }
        let intl = if at(j) == b'+' && matches!(at(j + 1), b'1'..=b'9') {
            j += 1;
            true
        } else if at(j) == b'0' && at(j + 1) == b'0' && matches!(at(j + 2), b'1'..=b'9') {
            j += 2;
            true
        } else if at(j) == b'0' && matches!(at(j + 1), b'1'..=b'9') {
            false
        } else {
            return None;
        };
        let (mut nd, mut groups, mut first_len, mut end) = (0usize, 0usize, 0usize, j);
        loop {
            let s = j;
            while j < n && b[j].is_ascii_digit() {
                j += 1;
            }
            let len = j - s;
            if len == 0 {
                break;
            }
            nd += len;
            groups += 1;
            if groups == 1 {
                first_len = len;
            }
            end = j;
            if nd > 15 {
                return None;
            }
            let mut k = j;
            while k < n && k - j < 3 && b" -/()".contains(&b[k]) {
                k += 1;
            }
            if k > j && k < n && b[k].is_ascii_digit() {
                j = k;
            } else {
                break;
            }
        }
        if !boundary_after(b, end) || (at(end) == b'.' && at(end + 1).is_ascii_digit()) {
            return None;
        }
        let ok = if intl {
            (8..=15).contains(&nd)
        } else {
            (9..=13).contains(&nd)
                && groups <= 5
                && if groups == 1 {
                    (10..=12).contains(&nd)
                } else {
                    (3..=6).contains(&first_len)
                }
        };
        ok.then_some(end)
    }

    fn iban(&self, i: usize) -> Option<usize> {
        let b = self.b;
        let n = b.len();
        if i + 4 > n
            || !b[i + 1].is_ascii_uppercase()
            || !b[i + 2].is_ascii_digit()
            || !b[i + 3].is_ascii_digit()
        {
            return None;
        }
        let len = iban_length(&b[i..i + 2])?;
        let mut buf = [0u8; 34];
        let mut k = 0;
        let mut j = i;
        while k < len && j < n {
            let c = b[j];
            if c.is_ascii_digit() || c.is_ascii_uppercase() {
                buf[k] = c;
                k += 1;
                j += 1;
            } else if c == b' ' && k >= 4 && j + 1 < n && b[j + 1].is_ascii_alphanumeric() {
                j += 1;
            } else {
                break;
            }
        }
        if k < len || !boundary_after(b, j) {
            return None;
        }
        iban_mod97(&buf[..len]).then_some(j)
    }

    fn upper_key(&self, i: usize) -> Option<usize> {
        if self.b[i] != b'A' {
            return None;
        }
        let e = run_end(self.b, i, is_token);
        let tok = &self.b[i..e];
        let aws = (tok.starts_with(b"AKIA") || tok.starts_with(b"ASIA"))
            && tok.len() == 20
            && tok
                .iter()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
        let google = tok.starts_with(b"AIza") && tok.len() == 39;
        (aws || google).then_some(e)
    }

    fn lower_key(&self, i: usize) -> Option<usize> {
        let b = self.b;
        if !matches!(
            b[i],
            b's' | b'r' | b'w' | b'g' | b'x' | b'h' | b'n' | b'p' | b'e'
        ) {
            return None;
        }
        let e = run_end(b, i, is_token);
        let tok = &b[i..e];
        if tok.starts_with(b"eyJ") {
            // JWT: drei Base64url-Teile, durch Punkte getrennt.
            let e = run_end(b, i, |c| is_token(c) || c == b'.');
            let jwt = &b[i..e];
            let mut parts = jwt.split(|&c| c == b'.');
            let (h, p, s) = (parts.next()?, parts.next()?, parts.next());
            if s.is_some() && parts.next().is_none() && h.len() >= 10 && p.len() >= 10 {
                return Some(e);
            }
            return None;
        }
        if tok.len() < 20 {
            return None; // kürzester bekannter Schlüssel: 20 Zeichen
        }
        let (prefix, min) = LOWER_KEYS.iter().find(|(p, _)| tok.starts_with(p))?;
        let body = &tok[prefix.len()..];
        let digits = body.iter().filter(|c| c.is_ascii_digit()).count();
        let letters = body.iter().filter(|c| c.is_ascii_alphabetic()).count();
        (tok.len() >= *min && digits >= 2 && letters >= 2).then_some(e)
    }

    fn id_document(&self, i: usize) -> Option<(Category, usize)> {
        if !self.on(Category::IdCardDe) && !self.on(Category::PassportDe) {
            return None;
        }
        let b = self.b;
        let e = run_end(b, i, |c| c.is_ascii_alphanumeric());
        if e - i != 10 || !boundary_after(b, e) {
            return None;
        }
        let s = &b[i..e];
        if !s[..9].iter().all(|&c| de_doc_char(c)) || !s[9].is_ascii_digit() {
            return None;
        }
        if s[1..9].iter().filter(|c| c.is_ascii_digit()).count() < 3 {
            return None;
        }
        if icao_check(&s[..9]) != Some(s[9] - b'0') {
            return None;
        }
        let cat = if b"CFGHJK".contains(&s[0]) {
            Category::PassportDe
        } else if b"LMNPRTVWXY".contains(&s[0]) {
            Category::IdCardDe
        } else {
            return None;
        };
        self.on(cat).then_some((cat, e))
    }

    fn entropy_token(&self, i: usize) -> Option<usize> {
        let cfg = self.engine.entropy?;
        let b = self.b;
        let in_set = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'+' | b'/');
        if i > 0 && in_set(b[i - 1]) {
            return None;
        }
        // `=` nur als Base64-Auffüllung am Ende (sonst würde `key=…` mitgenommen).
        let e = run_end(b, i, in_set);
        let e = (e..e + 2)
            .take_while(|&k| b.get(k) == Some(&b'='))
            .last()
            .map_or(e, |k| k + 1);
        if !boundary_after(b, e) {
            return None;
        }
        let tok = &b[i..e];
        if tok.len() < cfg.min_len || tok.len() > cfg.max_len {
            return None;
        }
        let (mut upper, mut lower, mut digit_runs) = (0usize, 0usize, 0usize);
        let mut prev_digit = false;
        for &c in tok {
            upper += usize::from(c.is_ascii_uppercase());
            lower += usize::from(c.is_ascii_lowercase());
            let d = c.is_ascii_digit();
            digit_runs += usize::from(d && !prev_digit);
            prev_digit = d;
        }
        let letters = upper + lower;
        let mixed = upper * 4 >= letters && lower * 4 >= letters && digit_runs >= 2;
        (mixed && shannon_bits(tok) >= cfg.min_bits).then_some(e)
    }
}

fn card(g: &Digits, b: &[u8]) -> Option<usize> {
    g.ends[..g.groups].iter().rev().find_map(|&(nd, e)| {
        ((13..=19).contains(&nd)
            && boundary_after(b, e)
            && luhn(&g.d[..nd])
            && card_brand_ok(&g.d[..nd]))
        .then_some(e)
    })
}

fn tax_id(g: &Digits, b: &[u8]) -> Option<usize> {
    g.ends[..g.groups].iter().find_map(|&(nd, e)| {
        (nd == 11 && boundary_after(b, e) && tax_id_de(&g.d[..11])).then_some(e)
    })
}
