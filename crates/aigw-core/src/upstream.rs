// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Prüfung von Upstream-Endpunkten (Deployments) gegen SSRF-Grundregeln (AIGW-65).
//!
//! Standard (ohne Ausnahmen in `admin.*` der Startkonfiguration):
//! - Schema nur `https`; `http` nur für Hosts aus `admin.allow_insecure_upstreams`.
//! - Keine privaten, lokalen oder internen Ziele: Loopback, RFC 1918, CGNAT, Link-Local
//!   (inkl. Cloud-Metadaten `169.254.169.254`), ULA/Site-Local (IPv6), IPv4 in IPv6
//!   (`::ffff:…`, `64:ff9b::/96`), dazu Namen wie `localhost`, `*.localhost`, `*.local`,
//!   `*.internal`, `*.lan`, `*.home.arpa`, `*.localdomain` und einteilige Namen (Docker-/
//!   Kubernetes-Dienstnamen wie `ollama`). Freischaltbar über `admin.allow_private_upstreams`.
//! - Immer verboten: Zugangsdaten in der URL (`user:pass@`), Fragmente, unspezifizierte,
//!   Broadcast- und Multicast-Adressen, numerische Altschreibweisen von IPv4
//!   (`2130706433`, `0x7f.1`, `127.1`), die manche Resolver als IP deuten.
//!
//! Grenze: Hostnamen werden hier **nicht** aufgelöst – ein öffentlicher Name, der auf eine
//! private Adresse zeigt (oder per DNS-Rebinding später zeigt), fällt erst beim
//! Verbindungsaufbau auf. Dafür steht [`UpstreamPolicy::ip_allowed`] bereit, das der
//! HTTP-Client der Anbieter-Adapter nach der Namensauflösung anwenden kann.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::config::AdminConfig;
use http::Uri;

/// Höchstlänge eines Endpunkts.
pub const MAX_ENDPOINT_LEN: usize = 2048;

/// Namensendungen, die nie öffentlich auflösen (lokale/interne Namensräume).
const PRIVATE_SUFFIXES: &[&str] = &[
    ".localhost",
    ".local",
    ".internal",
    ".lan",
    ".home.arpa",
    ".localdomain",
];

/// Ein Eintrag einer Ausnahmeliste.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HostRule {
    /// Genauer Hostname (klein geschrieben, ohne abschließenden Punkt).
    Name(String),
    /// Muster `*.endung` (passt auf Unterdomains, nicht auf `endung` selbst); gespeichert
    /// mit führendem Punkt.
    Suffix(String),
    /// IP-Adresse oder CIDR-Bereich.
    Net(IpAddr, u8),
}

impl HostRule {
    fn parse(raw: &str) -> Result<Self, String> {
        let s = raw.trim().to_ascii_lowercase();
        if s.is_empty() {
            return Err("leerer Eintrag".into());
        }
        if let Some(rest) = s.strip_prefix("*.") {
            let name = normalize_name(rest).ok_or_else(|| format!("ungültiges Muster {raw:?}"))?;
            return Ok(Self::Suffix(format!(".{name}")));
        }
        if let Some((addr, len)) = s.split_once('/') {
            let ip: IpAddr = addr
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse()
                .map_err(|_| format!("ungültiger CIDR-Bereich {raw:?}"))?;
            let max = if ip.is_ipv4() { 32 } else { 128 };
            let len: u8 = len
                .parse()
                .ok()
                .filter(|l| *l <= max)
                .ok_or_else(|| format!("ungültige Präfixlänge in {raw:?}"))?;
            return Ok(Self::Net(canonical(ip), len));
        }
        if let Ok(ip) = s
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
        {
            let ip = canonical(ip);
            let len = if ip.is_ipv4() { 32 } else { 128 };
            return Ok(Self::Net(ip, len));
        }
        normalize_name(&s)
            .map(Self::Name)
            .ok_or_else(|| format!("ungültiger Hostname {raw:?}"))
    }

    fn matches(&self, host: &Host) -> bool {
        match (self, host) {
            (Self::Name(n), Host::Name(h)) => n == h,
            (Self::Suffix(sfx), Host::Name(h)) => h.ends_with(sfx.as_str()),
            (Self::Net(net, len), Host::Ip(ip)) => in_net(*ip, *net, *len),
            _ => false,
        }
    }
}

/// Host eines Endpunkts.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Host {
    Ip(IpAddr),
    Name(String),
}

impl fmt::Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ip(ip) => write!(f, "{ip}"),
            Self::Name(n) => f.write_str(n),
        }
    }
}

/// Regeln für Upstream-Endpunkte (aus `admin.*` der Startkonfiguration).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpstreamPolicy {
    insecure: Vec<HostRule>,
    private: Vec<HostRule>,
}

impl UpstreamPolicy {
    /// Strenge Standardregeln ohne Ausnahmen.
    pub fn strict() -> Self {
        Self::default()
    }

    /// Regeln aus der Konfiguration; ungültige Einträge sind ein Fehler (Startabbruch).
    pub fn from_config(cfg: &AdminConfig) -> Result<Self, String> {
        let parse = |field: &str, list: &[String]| {
            list.iter()
                .map(|e| HostRule::parse(e).map_err(|m| format!("admin.{field}: {m}")))
                .collect::<Result<Vec<_>, _>>()
        };
        Ok(Self {
            insecure: parse("allow_insecure_upstreams", &cfg.allow_insecure_upstreams)?,
            private: parse("allow_private_upstreams", &cfg.allow_private_upstreams)?,
        })
    }

    /// Prüft einen Endpunkt (Basis-URL eines Deployments). Fehlertext für `422`.
    pub fn check_endpoint(&self, endpoint: &str) -> Result<(), String> {
        if endpoint.is_empty() || endpoint.len() > MAX_ENDPOINT_LEN {
            return Err(format!(
                "Endpunkt leer oder länger als {MAX_ENDPOINT_LEN} Zeichen"
            ));
        }
        if endpoint
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
        {
            return Err("Endpunkt enthält Leer- oder Steuerzeichen".into());
        }
        if endpoint.contains('#') {
            return Err("Endpunkt darf kein Fragment (#…) enthalten".into());
        }
        // Zugangsdaten vor dem Parsen erkennen (unabhängig davon, ob der Parser sie annimmt).
        if let Some((_, rest)) = endpoint.split_once("://")
            && rest
                .split(['/', '?'])
                .next()
                .is_some_and(|auth| auth.contains('@'))
        {
            return Err(
                "Endpunkt darf keine Zugangsdaten enthalten (user:pass@) – dafür gibt es \
                 ProviderCredential"
                    .into(),
            );
        }
        let uri: Uri = endpoint
            .parse()
            .map_err(|_| format!("Endpunkt {endpoint:?} ist keine gültige absolute URL"))?;
        let scheme = uri
            .scheme_str()
            .ok_or_else(|| "Endpunkt braucht ein Schema (https://…)".to_owned())?
            .to_ascii_lowercase();
        let authority = uri
            .authority()
            .ok_or_else(|| "Endpunkt braucht einen Host".to_owned())?;
        if authority.as_str().contains('@') {
            return Err(
                "Endpunkt darf keine Zugangsdaten enthalten (user:pass@) – dafür gibt es \
                 ProviderCredential"
                    .into(),
            );
        }
        if uri.port_u16() == Some(0) {
            return Err("Port 0 ist nicht erlaubt".into());
        }
        let host = parse_host(authority.host())?;

        match scheme.as_str() {
            "https" => {}
            "http" => {
                if !self.insecure.iter().any(|r| r.matches(&host)) {
                    return Err(format!(
                        "http:// ist für {host} nicht erlaubt – https verwenden oder den Host in \
                         admin.allow_insecure_upstreams freischalten"
                    ));
                }
            }
            other => {
                return Err(format!(
                    "Schema {other:?} nicht erlaubt (nur https, http nach Freischaltung)"
                ));
            }
        }

        if let Host::Ip(ip) = host
            && always_forbidden(ip)
        {
            return Err(format!("Zieladresse {ip} ist nie erlaubt"));
        }
        if is_private_host(&host) && !self.private.iter().any(|r| r.matches(&host)) {
            return Err(format!(
                "{host} ist ein privates/lokales Ziel – nur nach Freischaltung in \
                 admin.allow_private_upstreams erlaubt"
            ));
        }
        Ok(())
    }

    /// Darf nach der Namensauflösung mit `ip` verbunden werden? (Für den HTTP-Client der
    /// Anbieter-Adapter: schützt auch vor Namen, die auf private Adressen zeigen.)
    pub fn ip_allowed(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        if always_forbidden(ip) {
            return false;
        }
        !is_private_ip(ip) || self.private.iter().any(|r| r.matches(&Host::Ip(ip)))
    }
}

/// Hostname normalisieren (klein, ohne abschließenden Punkt) und prüfen.
fn normalize_name(s: &str) -> Option<String> {
    let s = s.strip_suffix('.').unwrap_or(s).to_ascii_lowercase();
    let ok = !s.is_empty()
        && s.len() <= 253
        && s.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        });
    ok.then_some(s)
}

fn parse_host(raw: &str) -> Result<Host, String> {
    if let Some(v6) = raw.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        return v6
            .parse::<Ipv6Addr>()
            .map(|ip| Host::Ip(canonical(IpAddr::V6(ip))))
            .map_err(|_| format!("ungültige IPv6-Adresse {raw:?}"));
    }
    if let Ok(ip) = raw.parse::<Ipv4Addr>() {
        return Ok(Host::Ip(IpAddr::V4(ip)));
    }
    let name = normalize_name(raw).ok_or_else(|| format!("ungültiger Hostname {raw:?}"))?;
    // Letztes Label rein numerisch oder hex (`0x…`): Altschreibweise einer IPv4-Adresse
    // (`2130706433`, `127.1`, `0x7f.0.0.1`) – kein echter DNS-Name (TLDs sind nie numerisch).
    let last = name.rsplit('.').next().unwrap_or(&name);
    if last.bytes().all(|b| b.is_ascii_digit()) || last.starts_with("0x") {
        return Err(format!(
            "Host {raw:?} ist keine gültige IP-Adresse in Punktschreibweise"
        ));
    }
    Ok(Host::Name(name))
}

/// IPv4 in IPv6 (`::ffff:a.b.c.d`, NAT64 `64:ff9b::/96`) auf IPv4 abbilden.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return IpAddr::V4(v4);
            }
            let s = v6.segments();
            if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                let [.., a, b, c, d] = v6.octets();
                return IpAddr::V4(Ipv4Addr::new(a, b, c, d));
            }
            IpAddr::V6(v6)
        }
        v4 @ IpAddr::V4(_) => v4,
    }
}

fn in_net(ip: IpAddr, net: IpAddr, len: u8) -> bool {
    match (canonical(ip), net) {
        (IpAddr::V4(a), IpAddr::V4(n)) => {
            let mask = u32::MAX.checked_shl(32 - u32::from(len)).unwrap_or(0);
            u32::from(a) & mask == u32::from(n) & mask
        }
        (IpAddr::V6(a), IpAddr::V6(n)) => {
            let mask = u128::MAX.checked_shl(128 - u32::from(len)).unwrap_or(0);
            u128::from(a) & mask == u128::from(n) & mask
        }
        _ => false,
    }
}

/// Adressen, die auch per Ausnahme nie erlaubt sind.
fn always_forbidden(ip: IpAddr) -> bool {
    match canonical(ip) {
        IpAddr::V4(v4) => v4.is_unspecified() || v4.is_broadcast() || v4.is_multicast(),
        IpAddr::V6(v6) => v6.is_unspecified() || v6.is_multicast(),
    }
}

/// Private, lokale und reservierte Bereiche.
fn is_private_ip(ip: IpAddr) -> bool {
    const V4: &[(Ipv4Addr, u8)] = &[
        (Ipv4Addr::new(0, 0, 0, 0), 8),      // „dieses Netz“
        (Ipv4Addr::new(10, 0, 0, 0), 8),     // RFC 1918
        (Ipv4Addr::new(100, 64, 0, 0), 10),  // CGNAT
        (Ipv4Addr::new(127, 0, 0, 0), 8),    // Loopback
        (Ipv4Addr::new(169, 254, 0, 0), 16), // Link-Local, Cloud-Metadaten
        (Ipv4Addr::new(172, 16, 0, 0), 12),  // RFC 1918
        (Ipv4Addr::new(192, 0, 0, 0), 24),   // IETF-Protokollzuweisungen
        (Ipv4Addr::new(192, 168, 0, 0), 16), // RFC 1918
        (Ipv4Addr::new(198, 18, 0, 0), 15),  // Benchmark-Netze
        (Ipv4Addr::new(224, 0, 0, 0), 4),    // Multicast
        (Ipv4Addr::new(240, 0, 0, 0), 4),    // reserviert, Broadcast
    ];
    const V6: &[(Ipv6Addr, u8)] = &[
        (Ipv6Addr::UNSPECIFIED, 128),
        (Ipv6Addr::LOCALHOST, 128),
        (Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7), // ULA
        (Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10), // Link-Local
        (Ipv6Addr::new(0xfec0, 0, 0, 0, 0, 0, 0, 0), 10), // Site-Local (veraltet)
        (Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8), // Multicast
        (Ipv6Addr::new(0x0100, 0, 0, 0, 0, 0, 0, 0), 64), // Discard
    ];
    match canonical(ip) {
        ip @ IpAddr::V4(_) => V4.iter().any(|(n, l)| in_net(ip, IpAddr::V4(*n), *l)),
        ip @ IpAddr::V6(_) => V6.iter().any(|(n, l)| in_net(ip, IpAddr::V6(*n), *l)),
    }
}

fn is_private_host(host: &Host) -> bool {
    match host {
        Host::Ip(ip) => is_private_ip(*ip),
        Host::Name(n) => {
            n == "localhost" || !n.contains('.') || PRIVATE_SUFFIXES.iter().any(|s| n.ends_with(s))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(insecure: &[&str], private: &[&str]) -> UpstreamPolicy {
        UpstreamPolicy::from_config(&AdminConfig {
            allow_insecure_upstreams: insecure.iter().map(|s| (*s).to_owned()).collect(),
            allow_private_upstreams: private.iter().map(|s| (*s).to_owned()).collect(),
        })
        .unwrap()
    }

    #[test]
    fn strict_policy() {
        let p = UpstreamPolicy::strict();
        for ok in [
            "https://api.openai.com/v1",
            "https://my-res.openai.azure.com",
            "https://example.com:8443/base?x=1",
            "https://[2001:db8::1]/v1",
            "HTTPS://API.EXAMPLE.COM",
            "https://8.8.8.8",
        ] {
            assert!(
                p.check_endpoint(ok).is_ok(),
                "{ok}: {:?}",
                p.check_endpoint(ok)
            );
        }
        for bad in [
            "",
            "api.openai.com",
            "/v1",
            "ftp://example.com",
            "file:///etc/passwd",
            "http://example.com",
            "https://user:pw@example.com",
            "https://example.com/#frag",
            "https://example.com/ x",
            "https://localhost:11434",
            "https://foo.localhost",
            "https://ollama",
            "https://ollama.lan",
            "https://metadata.google.internal",
            "https://printer.local",
            "https://127.0.0.1",
            "https://10.1.2.3",
            "https://172.31.0.1",
            "https://192.168.1.10",
            "https://100.64.0.1",
            "https://169.254.169.254/latest/meta-data",
            "https://0.0.0.0",
            "https://255.255.255.255",
            "https://224.0.0.1",
            "https://[::1]",
            "https://[fd00::1]",
            "https://[fe80::1]",
            "https://[::ffff:127.0.0.1]",
            "https://[::ffff:a9fe:a9fe]",
            "https://[64:ff9b::a00:1]",
            "https://2130706433",
            "https://0x7f.0.0.1",
            "https://127.1",
            "https://example.com:0",
        ] {
            assert!(
                p.check_endpoint(bad).is_err(),
                "{bad} sollte abgelehnt werden"
            );
        }
    }

    #[test]
    fn exceptions() {
        let p = policy(
            &["localhost", "ollama", "*.lan", "10.0.0.5"],
            &["localhost", "ollama", "*.lan", "10.0.0.0/8", "fd00::/8"],
        );
        for ok in [
            "http://localhost:11434",
            "http://ollama:11434/v1",
            "http://gpu1.lan:8000/v1",
            "https://10.20.30.40/v1",
            "http://10.0.0.5:8000",
            "https://[fd12::1]:8000",
            "https://[::ffff:10.1.1.1]",
        ] {
            assert!(
                p.check_endpoint(ok).is_ok(),
                "{ok}: {:?}",
                p.check_endpoint(ok)
            );
        }
        for bad in [
            "http://10.0.0.6",     // https-Pflicht für nicht freigeschaltete Hosts
            "https://192.168.1.1", // nicht im erlaubten Bereich
            "http://lan",          // `*.lan` passt nicht auf `lan` selbst
            "https://127.0.0.1",   // Loopback per IP nicht freigeschaltet
            "https://vllm",        // anderer Dienstname
            "https://0.0.0.0",     // nie erlaubt
        ] {
            assert!(
                p.check_endpoint(bad).is_err(),
                "{bad} sollte abgelehnt werden"
            );
        }
        // http allein reicht nicht für private Ziele (beide Listen nötig).
        let only_http = policy(&["localhost"], &[]);
        assert!(only_http.check_endpoint("http://localhost:1").is_err());
    }

    #[test]
    fn ip_check_after_resolution() {
        let p = policy(&[], &["192.168.0.0/16"]);
        assert!(p.ip_allowed("1.1.1.1".parse().unwrap()));
        assert!(p.ip_allowed("192.168.3.4".parse().unwrap()));
        assert!(!p.ip_allowed("10.0.0.1".parse().unwrap()));
        assert!(!p.ip_allowed("::ffff:10.0.0.1".parse().unwrap()));
        assert!(!p.ip_allowed("169.254.169.254".parse().unwrap()));
        assert!(!p.ip_allowed("0.0.0.0".parse().unwrap()));
    }

    #[test]
    fn invalid_config_entries() {
        for bad in ["", "10.0.0.0/33", "fd00::/129", "*.", "bad host", "a/b"] {
            assert!(
                UpstreamPolicy::from_config(&AdminConfig {
                    allow_private_upstreams: vec![bad.to_owned()],
                    ..AdminConfig::default()
                })
                .is_err(),
                "{bad:?}"
            );
        }
    }
}
