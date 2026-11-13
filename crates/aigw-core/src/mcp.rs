// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Gemeinsame Regeln des MCP-Gateways (AIGW-41): Einstellungen eines `McpServer`
//! ([`McpServerSettings`]), Prüfung von Servern und Freigaben (Admin-API und GitOps) und
//! Tool-Muster der Freigaben ([`ToolPattern`]).
//!
//! Siehe `docs/konzept/15-mcp-gateway.md`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::resource::{JsonObject, McpGrantSpec, McpServerSpec, McpTransport, ScopeLevel};
use crate::upstream::UpstreamPolicy;

/// Trenner zwischen Server- und Tool-Name im aggregierten Endpunkt (`<server>__<tool>`).
pub const NAMESPACE_SEP: &str = "__";

/// Standardfeld der Zugangsdaten, dessen Wert als Token gesendet wird.
pub const DEFAULT_CREDENTIAL_FIELD: &str = "apiKey";

/// Höchstzahl Einträge in `tools` einer Freigabe.
const MAX_GRANT_TOOLS: usize = 256;
/// Höchstlänge eines Tool-Musters.
const MAX_PATTERN_LEN: usize = 128;
/// Höchstzahl Argumente bzw. Umgebungsvariablen (stdio) und statischer Header.
const MAX_LIST: usize = 64;
/// Obergrenze für Zeitlimits (1 Stunde).
const MAX_TIMEOUT_MS: u64 = 3_600_000;

/// Header, die nicht als statische Header gesetzt werden dürfen (Geheimnisse gehören in die
/// Zugangsdaten; Protokoll-Header setzt das Gateway selbst).
const RESERVED_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "host",
    "content-type",
    "content-length",
    "accept",
    "connection",
    "transfer-encoding",
    "te",
    "upgrade",
    "mcp-session-id",
    "mcp-protocol-version",
    "last-event-id",
];

/// `McpServer.spec.settings` (unbekannte Schlüssel sind ein Fehler).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpServerSettings {
    /// Zusätzliche, nicht geheime Header an den Upstream (HTTP-Transporte).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Header für das Token aus den Zugangsdaten (Standard `authorization` → `Bearer …`;
    /// jeder andere Header erhält den Wert unverändert, z. B. `x-api-key`).
    #[serde(default)]
    pub credential_header: Option<String>,
    /// Feld der Zugangsdaten mit dem Token (Standard `apiKey`).
    #[serde(default)]
    pub credential_field: Option<String>,
    /// Zeitlimit je Anfrage an diesen Server in Millisekunden (Standard: `mcp.call_timeout_secs`).
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// stdio: Argumente des Programms.
    #[serde(default)]
    pub args: Vec<String>,
    /// stdio: Umgebungsvariablen (nicht geheim; der Prozess erbt sonst nur `PATH`).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// stdio: Umgebungsvariable, die das Token aus den Zugangsdaten erhält.
    #[serde(default)]
    pub credential_env: Option<String>,
}

impl McpServerSettings {
    /// Liest und prüft die Einstellungen.
    pub fn from_json(settings: &JsonObject) -> Result<Self, String> {
        let s: Self = serde_json::from_value(serde_json::Value::Object(settings.clone()))
            .map_err(|e| format!("settings: {e}"))?;
        if s.headers.len() > MAX_LIST {
            return Err(format!("settings.headers: höchstens {MAX_LIST} Header"));
        }
        for (k, v) in &s.headers {
            header_name(k).map_err(|m| format!("settings.headers.{k}: {m}"))?;
            if v.len() > 4096 || v.chars().any(|c| c.is_control() && c != '\t') {
                return Err(format!(
                    "settings.headers.{k}: Wert zu lang oder mit Steuerzeichen"
                ));
            }
        }
        if let Some(h) = &s.credential_header
            && !h.eq_ignore_ascii_case("authorization")
        {
            header_name(h).map_err(|m| format!("settings.credentialHeader: {m}"))?;
        }
        if let Some(f) = &s.credential_field
            && (f.is_empty()
                || f.len() > 64
                || !f.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
        {
            return Err("settings.credentialField: Feldname der Zugangsdaten erwartet".into());
        }
        if s.timeout_ms.is_some_and(|t| t == 0 || t > MAX_TIMEOUT_MS) {
            return Err(format!("settings.timeoutMs: 1–{MAX_TIMEOUT_MS}"));
        }
        if s.args.len() > MAX_LIST || s.args.iter().any(|a| a.contains('\0') || a.len() > 4096) {
            return Err(format!(
                "settings.args: höchstens {MAX_LIST} Argumente ohne NUL, je max. 4096 Zeichen"
            ));
        }
        if s.env.len() > MAX_LIST {
            return Err(format!("settings.env: höchstens {MAX_LIST} Variablen"));
        }
        for (k, v) in &s.env {
            env_name(k).map_err(|m| format!("settings.env.{k}: {m}"))?;
            if v.contains('\0') {
                return Err(format!("settings.env.{k}: Wert enthält NUL"));
            }
        }
        if let Some(e) = &s.credential_env {
            env_name(e).map_err(|m| format!("settings.credentialEnv: {m}"))?;
        }
        Ok(s)
    }

    /// Feld der Zugangsdaten mit dem Token.
    pub fn credential_field(&self) -> &str {
        self.credential_field
            .as_deref()
            .unwrap_or(DEFAULT_CREDENTIAL_FIELD)
    }
}

fn header_name(h: &str) -> Result<(), String> {
    let ok = !h.is_empty()
        && h.len() <= 128
        && h.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b));
    if !ok {
        return Err("ungültiger Header-Name".into());
    }
    if RESERVED_HEADERS.iter().any(|r| r.eq_ignore_ascii_case(h)) {
        return Err(
            "Header ist reserviert (Geheimnisse gehören in die Zugangsdaten, siehe credential)"
                .into(),
        );
    }
    Ok(())
}

fn env_name(e: &str) -> Result<(), String> {
    let mut b = e.bytes();
    let ok = e.len() <= 128
        && b.next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && b.all(|c| c.is_ascii_alphanumeric() || c == b'_');
    if ok {
        Ok(())
    } else {
        Err("ungültiger Variablenname".into())
    }
}

/// Prüft einen MCP-Server (Admin-API und GitOps): Name ohne `__` (Namensraum), URL gegen die
/// SSRF-Regeln (HTTP-Transporte) bzw. absoluter Programmpfad (stdio), Einstellungen.
///
/// Ob ein stdio-Programm tatsächlich gestartet werden darf, entscheidet die Datenebene
/// (`mcp.allow_stdio_commands`).
pub fn validate_server(
    upstreams: &UpstreamPolicy,
    name: &str,
    spec: &McpServerSpec,
) -> Result<(), String> {
    if name.contains(NAMESPACE_SEP) {
        return Err(format!(
            "metadata.name: darf {NAMESPACE_SEP:?} nicht enthalten (Trenner im Tool-Namensraum)"
        ));
    }
    match spec.transport {
        McpTransport::StreamableHttp | McpTransport::Sse => upstreams
            .check_endpoint(&spec.url)
            .map_err(|m| format!("url: {m}"))?,
        McpTransport::Stdio => {
            if !spec.url.starts_with('/')
                || spec.url.len() > 1024
                || spec
                    .url
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control())
            {
                return Err(
                    "url: für stdio den absoluten Pfad des Programms angeben (Argumente in \
                     settings.args)"
                        .into(),
                );
            }
        }
    }
    let settings = McpServerSettings::from_json(&spec.settings)?;
    if spec.transport != McpTransport::Stdio
        && (!settings.args.is_empty()
            || !settings.env.is_empty()
            || settings.credential_env.is_some())
    {
        return Err("settings.args/env/credentialEnv gelten nur für stdio".into());
    }
    if spec.transport == McpTransport::Stdio && !settings.headers.is_empty() {
        return Err("settings.headers gelten nur für HTTP-Transporte".into());
    }
    Ok(())
}

/// Prüft eine MCP-Freigabe: Empfänger `team` oder `api-key`, gültige Tool-Muster.
pub fn validate_grant(spec: &McpGrantSpec) -> Result<(), String> {
    if !matches!(spec.subject.level, ScopeLevel::Team | ScopeLevel::ApiKey) {
        return Err("subject.level: nur `team` oder `api-key` erlaubt".into());
    }
    if spec.subject.target.as_deref().is_none_or(str::is_empty) {
        return Err("subject.target: Name des Teams bzw. API-Keys fehlt".into());
    }
    if spec.tools.len() > MAX_GRANT_TOOLS {
        return Err(format!("tools: höchstens {MAX_GRANT_TOOLS} Einträge"));
    }
    for t in &spec.tools {
        ToolPattern::parse(t).map_err(|m| format!("tools: {m}"))?;
    }
    Ok(())
}

/// Ein Eintrag aus `McpGrant.spec.tools`: Glob-Muster (`*` beliebig viele, `?` genau ein
/// Zeichen) auf den Tool-Namen des Upstreams; mit führendem `!` ein Ausschluss.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPattern {
    /// Ausschluss (`!muster`).
    pub deny: bool,
    glob: Vec<char>,
}

impl ToolPattern {
    /// Parst einen Eintrag.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let (deny, body) = match raw.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, raw),
        };
        if body.is_empty()
            || body.len() > MAX_PATTERN_LEN
            || body
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || c == '!')
        {
            return Err(format!(
                "ungültiges Muster {raw:?} (1–{MAX_PATTERN_LEN} Zeichen ohne Leerzeichen, \
                 `!` nur am Anfang)"
            ));
        }
        Ok(Self {
            deny,
            glob: body.chars().collect(),
        })
    }

    /// Ausschluss aller Tools (`!*`).
    pub fn deny_all() -> Self {
        Self {
            deny: true,
            glob: vec!['*'],
        }
    }

    /// Passt das Muster auf `tool`?
    pub fn matches(&self, tool: &str) -> bool {
        let t: Vec<char> = tool.chars().collect();
        glob_match(&self.glob, &t)
    }
}

/// Glob-Vergleich mit `*` und `?` (iterativ, linear mit Rücksprung auf den letzten `*`).
fn glob_match(p: &[char], t: &[char]) -> bool {
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        match p.get(pi) {
            Some('*') => {
                star = Some((pi, ti));
                pi += 1;
            }
            Some(c) if *c == '?' || Some(c) == t.get(ti) => {
                pi += 1;
                ti += 1;
            }
            _ => match star {
                Some((sp, st)) => {
                    pi = sp + 1;
                    ti = st + 1;
                    star = Some((sp, st + 1));
                }
                None => return false,
            },
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::Scope;

    #[test]
    fn glob() {
        let m = |p: &str, t: &str| ToolPattern::parse(p).unwrap().matches(t);
        assert!(m("search", "search"));
        assert!(!m("search", "search2"));
        assert!(m("get_*", "get_issue"));
        assert!(m("get_*", "get_"));
        assert!(!m("get_*", "list_issues"));
        assert!(m("*_issue*", "create_issue_comment"));
        assert!(m("a?c", "abc"));
        assert!(!m("a?c", "ac"));
        assert!(m("*", ""));
        let d = ToolPattern::parse("!delete_*").unwrap();
        assert!(d.deny && d.matches("delete_repo"));
        assert!(ToolPattern::parse("!").is_err());
        assert!(ToolPattern::parse("a!b").is_err());
        assert!(ToolPattern::parse("a b").is_err());
    }

    fn server(url: &str, transport: McpTransport, settings: serde_json::Value) -> McpServerSpec {
        McpServerSpec {
            url: url.into(),
            transport,
            credential: None,
            enabled: true,
            settings: settings.as_object().cloned().unwrap_or_default(),
        }
    }

    #[test]
    fn server_rules() {
        let p = UpstreamPolicy::strict();
        let ok = server(
            "https://mcp.example.org/mcp",
            McpTransport::StreamableHttp,
            serde_json::json!({"headers": {"x-tenant": "a"}, "timeoutMs": 5000}),
        );
        validate_server(&p, "github", &ok).unwrap();
        assert!(
            validate_server(&p, "git__hub", &ok)
                .unwrap_err()
                .contains("__")
        );
        for bad in [
            "http://mcp.example.org",
            "https://127.0.0.1/mcp",
            "https://169.254.169.254/",
        ] {
            let s = server(bad, McpTransport::Sse, serde_json::json!({}));
            assert!(
                validate_server(&p, "x", &s)
                    .unwrap_err()
                    .starts_with("url:"),
                "{bad}"
            );
        }
        let s = server(
            "https://a.example",
            McpTransport::StreamableHttp,
            serde_json::json!({"headers": {"Authorization": "x"}}),
        );
        assert!(
            validate_server(&p, "x", &s)
                .unwrap_err()
                .contains("reserviert")
        );
        let s = server(
            "https://a.example",
            McpTransport::StreamableHttp,
            serde_json::json!({"tiemout": 1}),
        );
        assert!(validate_server(&p, "x", &s).is_err());
        let s = server(
            "/usr/bin/mcp-fs",
            McpTransport::Stdio,
            serde_json::json!({"args": ["--root", "/srv"], "env": {"LOG": "1"}, "credentialEnv": "TOKEN"}),
        );
        validate_server(&p, "fs", &s).unwrap();
        let s = server(
            "mcp-fs --root /",
            McpTransport::Stdio,
            serde_json::json!({}),
        );
        assert!(validate_server(&p, "fs", &s).is_err());
        let s = server(
            "https://a.example",
            McpTransport::StreamableHttp,
            serde_json::json!({"args": ["x"]}),
        );
        assert!(validate_server(&p, "fs", &s).is_err());
    }

    #[test]
    fn grant_rules() {
        let mut g = McpGrantSpec {
            server: "github".into(),
            subject: Scope {
                level: ScopeLevel::Team,
                target: Some("t".into()),
            },
            tools: vec!["get_*".into(), "!get_secret".into()],
        };
        validate_grant(&g).unwrap();
        g.tools.push("bad tool".into());
        assert!(validate_grant(&g).is_err());
        g.tools.pop();
        g.subject.level = ScopeLevel::Organization;
        assert!(validate_grant(&g).is_err());
    }
}
