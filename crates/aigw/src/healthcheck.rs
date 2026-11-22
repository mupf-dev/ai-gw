// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! `aigw healthcheck` – Bereitschaftsprüfung für Container ohne Shell/curl (AIGW-59).
//!
//! Fragt `GET /readyz` der Steuerebene ab (Adresse aus `server.control.listen` der
//! Startkonfiguration, `0.0.0.0`/`::` → Loopback) bzw. eine ausdrücklich angegebene
//! `http://`-URL. Exit-Code 0 bei HTTP 200 (auch `degraded`), sonst 1. Bewusst ohne
//! HTTP-Bibliothek: ein einzelner HTTP/1.0-Aufruf über `std::net` mit festen Zeitlimits.

use std::io::{Read as _, Write as _};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs as _};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Args;

/// Argumente von `aigw healthcheck`.
#[derive(Debug, Args)]
pub(crate) struct HealthcheckArgs {
    /// Startkonfiguration; die Adresse kommt aus `server.control.listen`.
    #[arg(short, long, env = "AIGW_CONFIG", conflicts_with = "url")]
    config: Option<PathBuf>,
    /// Statt der Konfiguration diese URL abfragen (nur `http://`), z. B.
    /// `http://127.0.0.1:4001/readyz`.
    #[arg(long)]
    url: Option<String>,
    /// Zeitlimit für Verbindung und Antwort (Sekunden).
    #[arg(long, default_value_t = 3)]
    timeout: u64,
}

/// Standardziel ohne Konfiguration und URL.
const DEFAULT_URL: &str = "http://127.0.0.1:4001/readyz";

/// Führt die Prüfung aus und gibt den Exit-Code zurück.
pub(crate) fn run(args: &HealthcheckArgs) -> ExitCode {
    match check(args) {
        Ok(line) => {
            let _ = writeln!(std::io::stdout(), "{line}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "aigw healthcheck: {e}");
            ExitCode::FAILURE
        }
    }
}

fn check(args: &HealthcheckArgs) -> Result<String, String> {
    let (host, addr, path) = match (&args.url, &args.config) {
        (Some(url), _) => parse_url(url)?,
        (None, Some(cfg)) => {
            let config = aigw_core::Config::load(cfg).map_err(|e| e.to_string())?;
            let addr = loopback_for(config.server.control.listen);
            (addr.to_string(), addr, "/readyz".to_owned())
        }
        (None, None) => parse_url(DEFAULT_URL)?,
    };
    let timeout = Duration::from_secs(args.timeout.max(1));
    let mut stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|e| format!("Verbindung zu {addr} fehlgeschlagen: {e}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|()| stream.set_write_timeout(Some(timeout)))
        .map_err(|e| e.to_string())?;
    let req = format!(
        "GET {path} HTTP/1.0\r\nHost: {host}\r\nUser-Agent: aigw-healthcheck\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("Senden fehlgeschlagen: {e}"))?;
    // Nur die Statuszeile und ein kurzer Body sind von Interesse; höchstens 64 KiB lesen.
    let mut buf = Vec::new();
    stream
        .take(64 * 1024)
        .read_to_end(&mut buf)
        .map_err(|e| format!("Antwort nicht lesbar: {e}"))?;
    let text = String::from_utf8_lossy(&buf);
    let status = parse_status(&text).ok_or("keine gültige HTTP-Antwort")?;
    let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b.trim());
    if status == 200 {
        Ok(format!("OK {addr}{path}: {body}"))
    } else {
        Err(format!("{addr}{path}: HTTP {status} {body}"))
    }
}

/// `0.0.0.0`/`::` (alle Schnittstellen) ist kein Ziel → Loopback derselben Familie.
fn loopback_for(listen: SocketAddr) -> SocketAddr {
    match listen.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), listen.port())
        }
        IpAddr::V6(ip) if ip.is_unspecified() => {
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), listen.port())
        }
        _ => listen,
    }
}

/// Zerlegt `http://host[:port][/pfad]` in Host-Header, Adresse und Pfad.
fn parse_url(url: &str) -> Result<(String, SocketAddr, String), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("nur http:// unterstützt: {url:?}"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_owned()),
        None => (rest, "/readyz".to_owned()),
    };
    if authority.is_empty() || path.contains(['\r', '\n', ' ']) {
        return Err(format!("ungültige URL: {url:?}"));
    }
    let with_port = if authority.ends_with(']') || !authority.contains(':') {
        format!("{authority}:80")
    } else {
        authority.to_owned()
    };
    let addr = with_port
        .to_socket_addrs()
        .map_err(|e| format!("{authority}: {e}"))?
        .next()
        .ok_or_else(|| format!("{authority}: keine Adresse"))?;
    Ok((authority.to_owned(), addr, path))
}

fn parse_status(text: &str) -> Option<u16> {
    let line = text.lines().next()?;
    let mut parts = line.split_whitespace();
    if !parts.next()?.starts_with("HTTP/") {
        return None;
    }
    parts.next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_and_status_parsing() {
        let (host, addr, path) = parse_url("http://127.0.0.1:4001/readyz").unwrap();
        assert_eq!(host, "127.0.0.1:4001");
        assert_eq!(addr, "127.0.0.1:4001".parse().unwrap());
        assert_eq!(path, "/readyz");
        let (_, addr, path) = parse_url("http://[::1]").unwrap();
        assert_eq!(addr, "[::1]:80".parse().unwrap());
        assert_eq!(path, "/readyz");
        assert!(parse_url("https://x").is_err());
        assert!(parse_url("http:///x").is_err());
        assert_eq!(parse_status("HTTP/1.1 200 OK\r\n"), Some(200));
        assert_eq!(parse_status("HTTP/1.0 503 x\r\n"), Some(503));
        assert_eq!(parse_status("garbage"), None);
        assert_eq!(
            loopback_for("0.0.0.0:4001".parse().unwrap()),
            "127.0.0.1:4001".parse().unwrap()
        );
        assert_eq!(
            loopback_for("[::]:4001".parse().unwrap()),
            "[::1]:4001".parse().unwrap()
        );
        assert_eq!(
            loopback_for("10.0.0.5:4001".parse().unwrap()),
            "10.0.0.5:4001".parse().unwrap()
        );
    }

    #[test]
    fn healthcheck_against_local_server() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = std::thread::spawn(move || {
            for (i, conn) in listener.incoming().take(2).enumerate() {
                let mut c = conn.unwrap();
                let mut req = [0u8; 1024];
                let _ = c.read(&mut req).unwrap();
                let resp: &[u8] = if i == 0 {
                    b"HTTP/1.1 200 OK\r\ncontent-length: 18\r\n\r\n{\"status\":\"ready\"}"
                } else {
                    b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n"
                };
                c.write_all(resp).unwrap();
            }
        });
        let args = HealthcheckArgs {
            config: None,
            url: Some(format!("http://{addr}/readyz")),
            timeout: 2,
        };
        assert!(check(&args).unwrap().contains("ready"));
        assert!(check(&args).unwrap_err().contains("503"));
        srv.join().unwrap();
    }
}
