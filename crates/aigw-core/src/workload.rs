// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Workload- und Agenten-Identitäten (AIGW-115, `docs/konzept/18-workload-identitaeten.md`).
//!
//! Neben virtuellen API-Keys akzeptiert die Datenebene (und das MCP-Gateway) signierte
//! Access-Tokens (JWT) vertrauenswürdiger Aussteller als `Authorization: Bearer …` – z. B. von
//! Microsoft Entra ID für Agent-Identitäten (Microsoft Entra Agent ID) oder andere
//! Workload-Identitäten. Ein geprüftes Token wird über die Liste
//! [`ApiKeySpec::workload_identities`](crate::resource::ApiKeySpec::workload_identities)
//! **einem bestehenden virtuellen Key** zugeordnet; damit gelten Modellfreigaben, Limits,
//! Budgets, Guardrail-/Plugin-Bindungen, Team/Organisation und Tool-Richtlinien unverändert.
//!
//! Dieses Modul enthält die Startkonfiguration (`auth.jwt`), den Ressourcen-Teil
//! ([`WorkloadIdentity`]) und die geprüfte Identität einer Anfrage ([`WorkloadPrincipal`]).
//! Die Prüfung selbst (Signatur, JWKS, Claims) liegt im Crate `aigw-jwt`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Signaturverfahren, die für Workload-Tokens überhaupt zugelassen werden können. `none`,
/// HMAC (`HS*`) und alle anderen Verfahren werden immer abgelehnt.
pub const ALLOWED_ALGORITHMS: [&str; 2] = ["RS256", "ES256"];

/// Standard-Anmeldeendpunkt von Microsoft Entra ID (öffentliche Cloud).
pub const ENTRA_AUTHORITY: &str = "https://login.microsoftonline.com";
/// Aussteller der v1-Tokens von Entra ID (öffentliche Cloud), gefolgt von `<tid>/`.
pub const ENTRA_V1_ISSUER_HOST: &str = "https://sts.windows.net";

/// Größte zulässige Länge eines Kennungswerts (Aussteller-Name, Subjekt).
pub const MAX_IDENTITY_LEN: usize = 256;
/// Höchstzahl von Workload-Identitäten je Key.
pub const MAX_IDENTITIES_PER_KEY: usize = 100;

/// Abschnitt `auth` der Startkonfiguration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    /// Access-Tokens (JWT) als Alternative zu API-Keys (AIGW-115).
    pub jwt: JwtAuthConfig,
}

impl AuthConfig {
    /// Prüft die Konfiguration (Startabbruch bei Fehlern).
    pub fn validate(&self) -> Result<(), String> {
        self.jwt.validate()
    }
}

/// `auth.jwt`: vertrauenswürdige Aussteller und gemeinsame Grenzen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JwtAuthConfig {
    /// Vertrauenswürdige Aussteller; leer = JWT-Anmeldung aus (nur API-Keys).
    pub issuers: Vec<JwtIssuerConfig>,
    /// Erlaubte Uhrenabweichung bei `exp`/`nbf`/`iat` (Sekunden, höchstens 300).
    pub clock_skew_secs: u64,
    /// Gültigkeit des geladenen JWKS im Speicher (Sekunden). Unbekannte `kid` lösen vorher ein
    /// gedrosseltes Nachladen aus (Schlüsselwechsel beim Aussteller).
    pub jwks_cache_secs: u64,
    /// Frühestens so oft wird das JWKS wegen eines unbekannten `kid` neu geladen (Sekunden).
    pub jwks_refresh_min_secs: u64,
    /// Zeitlimit je Abruf von JWKS/Discovery (Sekunden).
    pub http_timeout_secs: u64,
    /// Größte akzeptierte JWKS-/Discovery-Antwort (Bytes).
    pub max_jwks_bytes: usize,
    /// Größtes akzeptiertes Token (Bytes); größere werden ohne Prüfung abgelehnt.
    pub max_token_bytes: usize,
    /// Einträge im Cache geprüfter Tokens (Schlüssel: SHA-256 des Tokens, gültig bis `exp`);
    /// `0` = kein Cache.
    pub token_cache_entries: usize,
    /// Zusätzliche CA-Zertifikate (PEM) für JWKS-/Discovery-Abrufe.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ca_file: Option<PathBuf>,
}

impl Default for JwtAuthConfig {
    fn default() -> Self {
        Self {
            issuers: Vec::new(),
            clock_skew_secs: 60,
            jwks_cache_secs: 3600,
            jwks_refresh_min_secs: 30,
            http_timeout_secs: 10,
            max_jwks_bytes: 256 * 1024,
            max_token_bytes: 16 * 1024,
            token_cache_entries: 10_000,
            ca_file: None,
        }
    }
}

impl JwtAuthConfig {
    /// JWT-Anmeldung eingeschaltet?
    pub fn enabled(&self) -> bool {
        !self.issuers.is_empty()
    }

    /// Prüft Grenzen und Aussteller.
    pub fn validate(&self) -> Result<(), String> {
        if self.clock_skew_secs > 300 {
            return Err("auth.jwt.clock_skew_secs: höchstens 300".into());
        }
        if self.jwks_cache_secs == 0 || self.http_timeout_secs == 0 {
            return Err("auth.jwt.jwks_cache_secs und http_timeout_secs: mindestens 1".into());
        }
        if !(1024..=64 * 1024).contains(&self.max_token_bytes) {
            return Err("auth.jwt.max_token_bytes: 1024–65536".into());
        }
        if !(1024..=4 * 1024 * 1024).contains(&self.max_jwks_bytes) {
            return Err("auth.jwt.max_jwks_bytes: 1024–4194304".into());
        }
        let mut names = std::collections::BTreeSet::new();
        let mut all_issuers = std::collections::BTreeSet::new();
        for i in &self.issuers {
            let ctx = |e: String| format!("auth.jwt.issuers[{}]: {e}", i.name);
            crate::resource::validate_name(&i.name).map_err(ctx)?;
            if !names.insert(i.name.as_str()) {
                return Err(ctx("Name doppelt".into()));
            }
            let resolved = i.resolve().map_err(ctx)?;
            for iss in &resolved.issuers {
                if !all_issuers.insert(iss.clone()) {
                    return Err(ctx(format!(
                        "Aussteller {iss:?} ist bereits einem anderen Eintrag zugeordnet"
                    )));
                }
            }
        }
        Ok(())
    }
}

/// Profil eines Ausstellers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JwtProfile {
    /// Beliebiger OAuth-2.0-/OIDC-Aussteller: `issuers` und `audiences` Pflicht, Kennung aus
    /// `identity_claims` (Standard `sub`).
    #[default]
    Generic,
    /// Microsoft Entra ID (inkl. Microsoft Entra Agent ID): `tenant_id` Pflicht; Aussteller
    /// (v1 und v2) und JWKS werden daraus abgeleitet; `tid` muss passen; Kennung `azp`
    /// (v2) bzw. `appid` (v1) – die Anwendungs-ID des Aufrufers bzw. der Agent-Identität.
    Entra,
}

/// Ein vertrauenswürdiger Aussteller (`auth.jwt.issuers[]`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JwtIssuerConfig {
    /// Name (Ressourcen-Namensregeln); Workload-Identitäten an Keys verweisen darauf.
    pub name: String,
    /// Profil (`generic` | `entra`).
    pub profile: JwtProfile,
    /// Entra: Tenant-ID (GUID) – Pflicht beim Profil `entra`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    /// Entra: Anmeldeendpunkt (Standard `https://login.microsoftonline.com`; souveräne Clouds
    /// abweichend, dann meist auch `issuers` angeben).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authority: Option<String>,
    /// Exakte `iss`-Werte. Generic: Pflicht. Entra: Standard
    /// `https://sts.windows.net/<tid>/` (v1) und `<authority>/<tid>/v2.0` (v2).
    pub issuers: Vec<String>,
    /// JWKS-URL. Generic ohne Angabe: Discovery über
    /// `<issuers[0]>/.well-known/openid-configuration`. Entra: Standard
    /// `<authority>/<tid>/discovery/v2.0/keys`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jwks_uri: Option<String>,
    /// Erwartete Zielgruppen (`aud`), Pflicht: z. B. App-ID-URI des Gateways
    /// (`api://ai-gw`, v1-Tokens) und dessen Client-ID (GUID, v2-Tokens).
    pub audiences: Vec<String>,
    /// Erlaubte Signaturverfahren (Teilmenge von `RS256`, `ES256`; Standard beide).
    pub algorithms: Vec<String>,
    /// Claims der Kennung, der erste vorhandene gilt. Standard: Entra `azp`, `appid`;
    /// generic `sub`.
    pub identity_claims: Vec<String>,
    /// Claim mit dem Nutzer bei delegierten Tokens (nur für Attribution, nie für die
    /// Zuordnung). Standard: Entra `oid` (nur bei `idtyp: user`); generic keiner.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_claim: Option<String>,
    /// Mindestens eine dieser App-Rollen (`roles`) muss im Token stehen (App-only-Tokens).
    /// Mit `required_scopes` zusammen: eines von beiden genügt. Leer = keine Prüfung.
    pub required_roles: Vec<String>,
    /// Mindestens einer dieser delegierten Scopes (`scp`, durch Leerzeichen getrennt) muss im
    /// Token stehen (OBO/Agent-User-Tokens). Leer = keine Prüfung.
    pub required_scopes: Vec<String>,
    /// Entra: nur Tokens von Agent-Identitäten annehmen (`xms_act_fct` enthält `11`).
    pub require_agent: bool,
}

/// Aufgelöster Aussteller (Standardwerte eingesetzt, geprüft).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedIssuer {
    /// Name.
    pub name: String,
    /// Profil.
    pub profile: JwtProfile,
    /// Entra: erwarteter Tenant (klein geschrieben).
    pub tenant_id: Option<String>,
    /// Exakte `iss`-Werte.
    pub issuers: Vec<String>,
    /// JWKS-Quelle.
    pub jwks: JwksSource,
    /// Erwartete `aud`-Werte.
    pub audiences: Vec<String>,
    /// Erlaubte Verfahren.
    pub algorithms: Vec<String>,
    /// Kennungs-Claims (Reihenfolge).
    pub identity_claims: Vec<String>,
    /// Nutzer-Claim (Attribution).
    pub user_claim: Option<String>,
    /// Erforderliche Rollen.
    pub required_roles: Vec<String>,
    /// Erforderliche Scopes.
    pub required_scopes: Vec<String>,
    /// Nur Agent-Identitäten.
    pub require_agent: bool,
}

/// Woher das JWKS kommt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JwksSource {
    /// Feste URL.
    Uri(String),
    /// Discovery-Dokument (`jwks_uri` daraus).
    Discovery(String),
}

/// Prüft eine URL: `https`, oder `http` nur für Loopback (Tests, lokale Entwicklung).
pub fn check_https_url(u: &str, what: &str) -> Result<(), String> {
    let url = url::Url::parse(u).map_err(|e| format!("{what}: ungültige URL {u:?}: {e}"))?;
    let loopback = match url.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    match url.scheme() {
        "https" => Ok(()),
        "http" if loopback => Ok(()),
        _ => Err(format!(
            "{what}: nur https:// erlaubt (http:// nur für Loopback): {u:?}"
        )),
    }
}

fn is_guid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

impl JwtIssuerConfig {
    /// Setzt Standardwerte ein und prüft die Angaben.
    pub fn resolve(&self) -> Result<ResolvedIssuer, String> {
        let trimmed = |v: &[String]| -> Vec<String> {
            v.iter()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect()
        };
        let audiences = trimmed(&self.audiences);
        if audiences.is_empty() {
            return Err("audiences: mindestens eine Zielgruppe (z. B. api://ai-gw) angeben".into());
        }
        let mut algorithms = trimmed(&self.algorithms);
        if algorithms.is_empty() {
            algorithms = ALLOWED_ALGORITHMS.iter().map(|a| (*a).to_owned()).collect();
        }
        if let Some(a) = algorithms
            .iter()
            .find(|a| !ALLOWED_ALGORITHMS.contains(&a.as_str()))
        {
            return Err(format!(
                "algorithms: {a:?} nicht erlaubt (nur {ALLOWED_ALGORITHMS:?}; nie none/HS*)"
            ));
        }
        let mut issuers = trimmed(&self.issuers);
        let mut identity_claims = trimmed(&self.identity_claims);
        let mut user_claim = self
            .user_claim
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let (tenant_id, jwks) = match self.profile {
            JwtProfile::Entra => {
                let tid = self
                    .tenant_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .ok_or("tenant_id ist beim Profil entra Pflicht")?
                    .to_ascii_lowercase();
                if !is_guid(&tid) {
                    return Err(format!(
                        "tenant_id: GUID erwartet (keine Domain, kein common/organizations): {tid:?}"
                    ));
                }
                let authority = self
                    .authority
                    .as_deref()
                    .unwrap_or(ENTRA_AUTHORITY)
                    .trim_end_matches('/')
                    .to_owned();
                check_https_url(&authority, "authority")?;
                if issuers.is_empty() {
                    issuers = vec![
                        format!("{ENTRA_V1_ISSUER_HOST}/{tid}/"),
                        format!("{authority}/{tid}/v2.0"),
                    ];
                }
                if identity_claims.is_empty() {
                    identity_claims = vec!["azp".into(), "appid".into()];
                }
                if user_claim.is_none() {
                    user_claim = Some("oid".into());
                }
                let jwks = JwksSource::Uri(
                    self.jwks_uri
                        .clone()
                        .unwrap_or_else(|| format!("{authority}/{tid}/discovery/v2.0/keys")),
                );
                (Some(tid), jwks)
            }
            JwtProfile::Generic => {
                if self.tenant_id.is_some() || self.require_agent {
                    return Err("tenant_id/require_agent nur beim Profil entra".into());
                }
                if issuers.is_empty() {
                    return Err("issuers: beim Profil generic Pflicht".into());
                }
                if identity_claims.is_empty() {
                    identity_claims = vec!["sub".into()];
                }
                let jwks = match &self.jwks_uri {
                    Some(u) => JwksSource::Uri(u.clone()),
                    None => JwksSource::Discovery(format!(
                        "{}/.well-known/openid-configuration",
                        issuers[0].trim_end_matches('/')
                    )),
                };
                (None, jwks)
            }
        };
        match &jwks {
            JwksSource::Uri(u) => check_https_url(u, "jwks_uri")?,
            JwksSource::Discovery(u) => check_https_url(u, "issuers[0] (Discovery)")?,
        }
        Ok(ResolvedIssuer {
            name: self.name.clone(),
            profile: self.profile,
            tenant_id,
            issuers,
            jwks,
            audiences,
            algorithms,
            identity_claims,
            user_claim,
            required_roles: trimmed(&self.required_roles),
            required_scopes: trimmed(&self.required_scopes),
            require_agent: self.require_agent,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Ressourcen-Teil: Zuordnung Identität → virtueller Key
// ---------------------------------------------------------------------------------------------

/// Eine Workload- bzw. Agent-Identität, die sich mit einem Access-Token statt mit dem
/// Klartext-Key anmelden darf (Feld `workloadIdentities` am API-Key).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkloadIdentity {
    /// Name des Ausstellers aus `auth.jwt.issuers[].name` (z. B. `entra-contoso`).
    pub issuer: String,
    /// Wert des Kennungs-Claims, z. B. bei Entra die Anwendungs-ID (`azp`/`appid`) der
    /// Agent-Identität bzw. des Service Principals.
    pub subject: String,
    /// Freitext (z. B. Name des Agenten).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl WorkloadIdentity {
    /// Schlüssel im Index des Snapshots.
    pub fn key(&self) -> WorkloadKey {
        WorkloadKey::new(&self.issuer, &self.subject)
    }
}

/// Index-Schlüssel (Aussteller-Name, Subjekt). GUIDs werden ohne Rücksicht auf Groß-/
/// Kleinschreibung verglichen (Kleinbuchstaben), andere Werte exakt.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkloadKey {
    /// Aussteller-Name.
    pub issuer: String,
    /// Subjekt (normalisiert).
    pub subject: String,
}

impl WorkloadKey {
    /// Normalisierter Schlüssel.
    pub fn new(issuer: &str, subject: &str) -> Self {
        let subject = subject.trim();
        let subject = if is_guid(subject) {
            subject.to_ascii_lowercase()
        } else {
            subject.to_owned()
        };
        Self {
            issuer: issuer.trim().to_owned(),
            subject,
        }
    }
}

/// Prüft die Workload-Identitäten eines Keys (Format, Längen, Doppelte).
pub fn validate_identities(list: &[WorkloadIdentity]) -> Result<(), String> {
    if list.len() > MAX_IDENTITIES_PER_KEY {
        return Err(format!(
            "workloadIdentities: höchstens {MAX_IDENTITIES_PER_KEY} Einträge"
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for (i, w) in list.iter().enumerate() {
        let at = |m: &str| format!("workloadIdentities[{i}]: {m}");
        crate::resource::validate_name(w.issuer.trim()).map_err(|e| at(&format!("issuer: {e}")))?;
        let s = w.subject.trim();
        if s.is_empty() || s.len() > MAX_IDENTITY_LEN || s.chars().any(char::is_control) {
            return Err(at(&format!(
                "subject: 1–{MAX_IDENTITY_LEN} Zeichen ohne Steuerzeichen"
            )));
        }
        if w.subject != s || w.issuer != w.issuer.trim() {
            return Err(at("ohne führende/folgende Leerzeichen angeben"));
        }
        if w.description.as_ref().is_some_and(|d| d.len() > 512) {
            return Err(at("description: höchstens 512 Zeichen"));
        }
        if !seen.insert(w.key()) {
            return Err(at("doppelt"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Geprüfte Identität einer Anfrage
// ---------------------------------------------------------------------------------------------

/// Art des Tokens (aus den dokumentierten Entra-Claims `idtyp` und `xms_sub_fct`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum WorkloadTokenKind {
    /// App-only (`idtyp: app`): die Identität handelt selbst (z. B. autonomer Agent,
    /// Client-Credentials).
    App,
    /// Delegiert für einen Menschen (`idtyp: user`, OBO bzw. interaktiver Agent).
    Delegated,
    /// Agent-eigenes Nutzerkonto (`idtyp: user`, `xms_sub_fct` enthält `13`).
    AgentUser,
    /// Nicht bestimmbar (Profil generic).
    Unknown,
}

impl WorkloadTokenKind {
    /// Bezeichner (Logs, Trace-Attribut).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Delegated => "delegated",
            Self::AgentUser => "agent-user",
            Self::Unknown => "unknown",
        }
    }
}

/// Geprüfte Identität aus einem Access-Token – Grundlage von Zuordnung und Attribution.
/// Enthält nie das Token selbst.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkloadPrincipal {
    /// Aussteller-Name aus der Konfiguration.
    pub issuer: String,
    /// Kennung (Wert des Kennungs-Claims, z. B. Entra `azp`/`appid`).
    pub subject: String,
    /// Art des Tokens.
    pub kind: WorkloadTokenKind,
    /// Entra: Token stammt von einer Agent-Identität (`xms_act_fct` enthält `11`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub agent: bool,
    /// Nutzer bei delegierten Tokens (Wert von `user_claim`, Entra `oid`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Entra: Agent Identity Blueprint (`xms_par_app_azp`) – nur Protokoll, nie Zuordnung.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blueprint: Option<String>,
    /// Entra: Tenant (`tid`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// Ablauf des Tokens (Unix-Sekunden).
    #[serde(skip)]
    pub expires_at: i64,
}

impl WorkloadPrincipal {
    /// Index-Schlüssel für die Zuordnung zum Key.
    pub fn key(&self) -> WorkloadKey {
        WorkloadKey::new(&self.issuer, &self.subject)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entra() -> JwtIssuerConfig {
        JwtIssuerConfig {
            name: "entra".into(),
            profile: JwtProfile::Entra,
            tenant_id: Some("AAAABBBB-0000-CCCC-1111-DDDD2222EEEE".into()),
            audiences: vec!["api://ai-gw".into()],
            ..JwtIssuerConfig::default()
        }
    }

    #[test]
    fn entra_defaults() {
        let r = entra().resolve().unwrap();
        assert_eq!(
            r.issuers,
            vec![
                "https://sts.windows.net/aaaabbbb-0000-cccc-1111-dddd2222eeee/".to_owned(),
                "https://login.microsoftonline.com/aaaabbbb-0000-cccc-1111-dddd2222eeee/v2.0"
                    .to_owned()
            ]
        );
        assert_eq!(r.identity_claims, vec!["azp", "appid"]);
        assert_eq!(r.user_claim.as_deref(), Some("oid"));
        assert_eq!(r.algorithms, vec!["RS256", "ES256"]);
        assert_eq!(
            r.jwks,
            JwksSource::Uri(
                "https://login.microsoftonline.com/aaaabbbb-0000-cccc-1111-dddd2222eeee/discovery/v2.0/keys"
                    .into()
            )
        );
    }

    #[test]
    fn rejects_bad_issuer_configs() {
        let mut c = entra();
        c.tenant_id = Some("contoso.onmicrosoft.com".into());
        assert!(c.resolve().is_err());
        let mut c = entra();
        c.tenant_id = None;
        assert!(c.resolve().is_err());
        let mut c = entra();
        c.audiences.clear();
        assert!(c.resolve().is_err());
        for alg in ["none", "HS256", "RS512", "EdDSA"] {
            let mut c = entra();
            c.algorithms = vec![alg.into()];
            assert!(c.resolve().is_err(), "{alg}");
        }
        let g = JwtIssuerConfig {
            name: "g".into(),
            audiences: vec!["x".into()],
            ..JwtIssuerConfig::default()
        };
        assert!(g.resolve().is_err(), "generic ohne issuers");
        let g = JwtIssuerConfig {
            issuers: vec!["http://idp.example".into()],
            ..g
        };
        assert!(g.resolve().is_err(), "http nur Loopback");
        let g = JwtIssuerConfig {
            issuers: vec!["http://127.0.0.1:9/x".into()],
            ..g
        };
        let r = g.resolve().unwrap();
        assert_eq!(
            r.jwks,
            JwksSource::Discovery("http://127.0.0.1:9/x/.well-known/openid-configuration".into())
        );
        assert_eq!(r.identity_claims, vec!["sub"]);
    }

    #[test]
    fn config_validation() {
        let mut cfg = JwtAuthConfig::default();
        assert!(cfg.validate().is_ok());
        cfg.issuers = vec![entra(), entra()];
        assert!(cfg.validate().unwrap_err().contains("doppelt"));
        let mut second = entra();
        second.name = "zweit".into();
        cfg.issuers = vec![entra(), second];
        assert!(cfg.validate().unwrap_err().contains("bereits"));
        cfg.issuers = vec![entra()];
        cfg.clock_skew_secs = 3600;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn identity_validation_and_keys() {
        let w = |i: &str, s: &str| WorkloadIdentity {
            issuer: i.into(),
            subject: s.into(),
            description: None,
        };
        assert!(validate_identities(&[w("entra", "11112222-bbbb-3333-cccc-4444dddd5555")]).is_ok());
        assert!(validate_identities(&[w("Entra", "x")]).is_err());
        assert!(validate_identities(&[w("entra", "")]).is_err());
        assert!(validate_identities(&[w("entra", " x")]).is_err());
        assert!(validate_identities(&[w("entra", "a\nb")]).is_err());
        let dup = [
            w("entra", "11112222-BBBB-3333-cccc-4444dddd5555"),
            w("entra", "11112222-bbbb-3333-cccc-4444dddd5555"),
        ];
        assert!(validate_identities(&dup).unwrap_err().contains("doppelt"));
        assert_eq!(dup[0].key(), dup[1].key());
        // Nicht-GUIDs bleiben exakt.
        assert_ne!(w("g", "Agent").key(), w("g", "agent").key());
    }
}
