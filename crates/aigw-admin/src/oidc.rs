// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! OIDC-Relying-Party (Authorization Code + PKCE) gegen einen beliebigen IdP (AIGW-32).
//!
//! Bewusst schlank und ohne OIDC-Bibliothek (Begründung: `docs/konzept/08-admin-api.md` §5.2):
//! - Discovery (`<issuer>/.well-known/openid-configuration`) und JWKS werden geladen und im
//!   Speicher gehalten (`cache_secs`); unbekannte `kid` lösen ein gedrosseltes Nachladen aus
//!   (Schlüsselwechsel beim IdP).
//! - ID-Token (JWS Compact): Signaturprüfung **ausschließlich** über aws-lc-rs (RS256,
//!   ES256, EdDSA/Ed25519) – keine eigene Kryptografie; `none` und HMAC werden abgelehnt.
//!   Geprüft werden `iss` (exakt), `aud` (enthält die Client-ID; `azp` falls vorhanden),
//!   `exp`/`iat`/`nbf` (mit Uhrenabweichung) und `nonce`.
//! - Nutzerkennung (F90): Issuer + Wert von `identity_claim` (Standard `sub`; Profil `entra`:
//!   `oid`, dazu muss `tid` dem konfigurierten Tenant entsprechen).
//! - HTTP über den gemeinsamen Outbound-Baustein (`aigw-outbound`, AIGW-72): rustls/aws-lc-rs
//!   mit den Vertrauensankern aus `upstream.roots`/`upstream.ca_file` plus optional
//!   `admin.auth.oidc.ca_file`, keine Weiterleitungen, Antwort- und Zeitlimit. Der IdP stammt
//!   aus der Startkonfiguration (Betreiber) – daher ohne SSRF-Adressfilter. `http://` nur für
//!   Loopback-Adressen (Tests, lokale Entwicklung).

use std::sync::Arc;
use std::time::{Duration, Instant};

use aigw_core::config::{OidcConfig, OidcProfile};
use aigw_jwt::jws::{Jwks, JwsHeader, claim_strings};
use aigw_outbound::{AddressGuard, FetchLimits, OutboundClient, OutboundOptions};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use hyper::Request;
use hyper::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde::Deserialize;
use serde_json::Value;
use time::OffsetDateTime;
use tokio::sync::RwLock;
use url::Url;

/// Größte akzeptierte Antwort des IdP.
const MAX_BODY: usize = 1024 * 1024;
/// Frühestens so oft wird das JWKS wegen eines unbekannten `kid` neu geladen.
const JWKS_REFRESH_MIN: Duration = Duration::from_secs(30);
/// Unterstützte Signaturverfahren.
pub const SUPPORTED_ALGS: [&str; 3] = ["RS256", "ES256", "EdDSA"];

/// Fehler im OIDC-Ablauf (Details nur ins Log, nie an den Browser).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct OidcError(pub String);

fn err(s: impl Into<String>) -> OidcError {
    OidcError(s.into())
}

/// Prüft eine URL: `https`, oder `http` nur für Loopback.
pub fn check_url(u: &str, what: &str) -> Result<Url, OidcError> {
    let url = Url::parse(u).map_err(|e| err(format!("{what}: ungültige URL {u:?}: {e}")))?;
    let loopback = match url.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    match url.scheme() {
        "https" => Ok(url),
        "http" if loopback => Ok(url),
        _ => Err(err(format!(
            "{what}: nur https:// erlaubt (http:// nur für Loopback): {u:?}"
        ))),
    }
}

/// Discovery-Dokument (benötigte Felder).
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderMetadata {
    /// Issuer.
    pub issuer: String,
    /// Autorisierungs-Endpunkt.
    pub authorization_endpoint: String,
    /// Token-Endpunkt.
    pub token_endpoint: String,
    /// JWKS.
    pub jwks_uri: String,
    /// RP-initiated Logout.
    #[serde(default)]
    pub end_session_endpoint: Option<String>,
    /// Unterstützte PKCE-Verfahren.
    #[serde(default)]
    pub code_challenge_methods_supported: Option<Vec<String>>,
}

/// Geprüfte Angaben aus dem ID-Token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdClaims {
    /// Issuer.
    pub issuer: String,
    /// Unveränderliche Nutzerkennung: Wert des Claims `identity_claim` (Standard `sub`,
    /// Profil `entra`: `oid`; F90). Zusammen mit `issuer` die Identität.
    pub subject: String,
    /// E-Mail.
    pub email: Option<String>,
    /// E-Mail vom IdP bestätigt.
    pub email_verified: bool,
    /// Anzeigename (`name`).
    pub name: Option<String>,
    /// Bevorzugter Benutzername.
    pub preferred_username: Option<String>,
    /// Gruppen (aus dem konfigurierten Claim).
    pub groups: Vec<String>,
}

struct Cached<T> {
    value: Arc<T>,
    at: Instant,
}

/// OIDC-Client einer ai-gw-Instanz.
pub struct OidcClient {
    issuer: String,
    client_id: String,
    client_secret: Option<String>,
    redirect_url: String,
    scopes: Vec<String>,
    groups_claim: String,
    /// Claim der Nutzerkennung (F90).
    identity_claim: String,
    /// Profil `entra`: erwarteter Tenant (`tid`).
    tenant: Option<String>,
    cache_ttl: Duration,
    timeout: Duration,
    skew: i64,
    http: OutboundClient,
    meta: RwLock<Option<Cached<ProviderMetadata>>>,
    jwks: RwLock<Option<Cached<Jwks>>>,
}

impl std::fmt::Debug for OidcClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcClient")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("confidential", &self.client_secret.is_some())
            .finish_non_exhaustive()
    }
}

impl OidcClient {
    /// Baut den Client aus der Konfiguration (ohne Netzwerkzugriff; Discovery erfolgt beim
    /// ersten Login). `client_secret` liest der Aufrufer aus Datei/Umgebung.
    pub fn new(cfg: &OidcConfig, client_secret: Option<String>) -> Result<Self, OidcError> {
        let issuer = cfg
            .issuer
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| err("admin.auth.oidc.issuer fehlt"))?;
        check_url(issuer, "admin.auth.oidc.issuer")?;
        if cfg.client_id.trim().is_empty() {
            return Err(err("admin.auth.oidc.client_id fehlt"));
        }
        let redirect_url = cfg
            .redirect_url
            .as_deref()
            .ok_or_else(|| err("admin.auth.oidc.redirect_url fehlt"))?;
        let redirect = check_url(redirect_url, "admin.auth.oidc.redirect_url")?;
        if !redirect.path().ends_with("/admin/auth/callback") {
            return Err(err(
                "admin.auth.oidc.redirect_url muss auf /admin/auth/callback zeigen",
            ));
        }
        let mut scopes: Vec<String> = vec!["openid".to_owned()];
        for s in &cfg.scopes {
            if !scopes.contains(s) {
                scopes.push(s.clone());
            }
        }
        let tenant = match cfg.profile {
            OidcProfile::Generic => None,
            OidcProfile::Entra => Some(
                cfg.tenant_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .ok_or_else(|| {
                        err("admin.auth.oidc.tenant_id ist beim Profil entra Pflicht (F90)")
                    })?
                    .to_owned(),
            ),
        };
        let identity_claim = cfg.effective_identity_claim().to_owned();
        let mut tls = aigw_outbound::default_tls();
        if let Some(ca) = &cfg.ca_file {
            tls = tls.with_ca_file(ca);
        }
        let http = OutboundClient::builder(AddressGuard::Trusted)
            .options(OutboundOptions {
                connect_timeout: Duration::from_secs(cfg.http_timeout_secs.max(1)),
                pool_max_idle_per_host: 4,
                tls: Some(tls),
                ..OutboundOptions::default()
            })
            .build()
            .map_err(|e| err(format!("admin.auth.oidc: {e}")))?;
        Ok(Self {
            issuer: issuer.to_owned(),
            client_id: cfg.client_id.trim().to_owned(),
            client_secret: client_secret.filter(|s| !s.is_empty()),
            redirect_url: redirect_url.to_owned(),
            scopes,
            groups_claim: cfg.groups_claim.clone(),
            identity_claim,
            tenant,
            cache_ttl: Duration::from_secs(cfg.cache_secs.max(1)),
            timeout: Duration::from_secs(cfg.http_timeout_secs.max(1)),
            skew: i64::try_from(cfg.clock_skew_secs).unwrap_or(60),
            http,
            meta: RwLock::new(None),
            jwks: RwLock::new(None),
        })
    }

    /// Client-ID.
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// Rücksprung-URL.
    pub fn redirect_url(&self) -> &str {
        &self.redirect_url
    }

    async fn send(&self, req: Request<Bytes>) -> Result<(u16, Bytes), OidcError> {
        let uri = req.uri().clone();
        let reply = self
            .http
            .fetch(req, &FetchLimits::new(self.timeout, MAX_BODY))
            .await
            .map_err(|e| match e {
                aigw_outbound::OutboundError::Timeout(_) => {
                    err(format!("{uri}: Zeitüberschreitung"))
                }
                other => err(format!("{uri}: {other}")),
            })?;
        Ok((reply.status.as_u16(), reply.body))
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, OidcError> {
        check_url(url, "IdP-Endpunkt")?;
        let req = Request::get(url)
            .header(ACCEPT, "application/json")
            .body(Bytes::new())
            .map_err(|e| err(format!("{url}: {e}")))?;
        let (status, body) = self.send(req).await?;
        if status != 200 {
            return Err(err(format!("{url}: HTTP {status}")));
        }
        serde_json::from_slice(&body).map_err(|e| err(format!("{url}: ungültiges JSON: {e}")))
    }

    /// Discovery-Dokument (gecacht).
    pub async fn metadata(&self) -> Result<Arc<ProviderMetadata>, OidcError> {
        if let Some(c) = self.meta.read().await.as_ref()
            && c.at.elapsed() < self.cache_ttl
        {
            return Ok(Arc::clone(&c.value));
        }
        let url = format!(
            "{}/.well-known/openid-configuration",
            self.issuer.trim_end_matches('/')
        );
        let meta: ProviderMetadata = self.get_json(&url).await?;
        if meta.issuer.trim_end_matches('/') != self.issuer.trim_end_matches('/') {
            return Err(err(format!(
                "Discovery: issuer {:?} passt nicht zur Konfiguration {:?}",
                meta.issuer, self.issuer
            )));
        }
        for (u, what) in [
            (&meta.authorization_endpoint, "authorization_endpoint"),
            (&meta.token_endpoint, "token_endpoint"),
            (&meta.jwks_uri, "jwks_uri"),
        ] {
            check_url(u, what)?;
        }
        if let Some(m) = &meta.code_challenge_methods_supported
            && !m.iter().any(|x| x == "S256")
        {
            return Err(err("IdP unterstützt PKCE S256 nicht"));
        }
        let value = Arc::new(meta);
        *self.meta.write().await = Some(Cached {
            value: Arc::clone(&value),
            at: Instant::now(),
        });
        Ok(value)
    }

    async fn jwks(&self, force: bool) -> Result<Arc<Jwks>, OidcError> {
        if let Some(c) = self.jwks.read().await.as_ref() {
            let age = c.at.elapsed();
            if (!force && age < self.cache_ttl) || (force && age < JWKS_REFRESH_MIN) {
                return Ok(Arc::clone(&c.value));
            }
        }
        let meta = self.metadata().await?;
        let jwks: Jwks = self.get_json(&meta.jwks_uri).await?;
        let value = Arc::new(jwks);
        *self.jwks.write().await = Some(Cached {
            value: Arc::clone(&value),
            at: Instant::now(),
        });
        Ok(value)
    }

    /// URL zur Anmeldung beim IdP.
    pub async fn authorization_url(
        &self,
        state: &str,
        nonce: &str,
        code_challenge: &str,
    ) -> Result<String, OidcError> {
        let meta = self.metadata().await?;
        let mut url = Url::parse(&meta.authorization_endpoint)
            .map_err(|e| err(format!("authorization_endpoint: {e}")))?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", &self.redirect_url)
            .append_pair("scope", &self.scopes.join(" "))
            .append_pair("state", state)
            .append_pair("nonce", nonce)
            .append_pair("code_challenge", code_challenge)
            .append_pair("code_challenge_method", "S256");
        Ok(url.into())
    }

    /// Tauscht den Code gegen Tokens und liefert das (noch ungeprüfte) ID-Token.
    pub async fn exchange_code(&self, code: &str, verifier: &str) -> Result<String, OidcError> {
        let meta = self.metadata().await?;
        // Der Serializer ist nicht `Send` – daher in einem eigenen Block fertigstellen.
        let body = {
            let mut form = url::form_urlencoded::Serializer::new(String::new());
            form.append_pair("grant_type", "authorization_code")
                .append_pair("code", code)
                .append_pair("redirect_uri", &self.redirect_url)
                .append_pair("code_verifier", verifier);
            if self.client_secret.is_none() {
                form.append_pair("client_id", &self.client_id);
            }
            form.finish()
        };
        let mut req = Request::post(&meta.token_endpoint)
            .header(ACCEPT, "application/json")
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(secret) = &self.client_secret {
            // client_secret_basic (RFC 6749 §2.3.1: Bestandteile form-kodiert).
            let enc =
                |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
            let cred = STANDARD.encode(format!("{}:{}", enc(&self.client_id), enc(secret)));
            req = req.header(AUTHORIZATION, format!("Basic {cred}"));
        }
        let req = req
            .body(Bytes::from(body))
            .map_err(|e| err(format!("Token-Anfrage: {e}")))?;
        let (status, body) = self.send(req).await?;
        let v: Value = serde_json::from_slice(&body)
            .map_err(|e| err(format!("Token-Antwort (HTTP {status}): kein JSON: {e}")))?;
        if status != 200 {
            return Err(err(format!(
                "Token-Endpunkt: HTTP {status}, error={} {}",
                v["error"].as_str().unwrap_or("?"),
                v["error_description"].as_str().unwrap_or("")
            )));
        }
        v["id_token"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| err("Token-Antwort ohne id_token"))
    }

    /// Prüft ein ID-Token (Signatur, `iss`, `aud`/`azp`, Zeiten, `nonce`).
    pub async fn verify_id_token(&self, token: &str, nonce: &str) -> Result<IdClaims, OidcError> {
        let meta = self.metadata().await?;
        let parts: Vec<&str> = token.split('.').collect();
        let [h64, p64, s64] = parts.as_slice() else {
            return Err(err("ID-Token ist kein JWS (3 Teile erwartet)"));
        };
        let header: JwsHeader =
            serde_json::from_slice(&b64(h64)?).map_err(|e| err(format!("JWS-Header: {e}")))?;
        if header.crit.is_some() {
            return Err(err("JWS-Header mit crit wird nicht unterstützt"));
        }
        if !SUPPORTED_ALGS.contains(&header.alg.as_str()) {
            return Err(err(format!(
                "Signaturverfahren {:?} nicht erlaubt (erlaubt: {SUPPORTED_ALGS:?})",
                header.alg
            )));
        }
        let signing_input = &token[..h64.len() + 1 + p64.len()];
        let sig = b64(s64)?;
        let mut verified = self
            .verify_with_jwks(&header, signing_input.as_bytes(), &sig, false)
            .await?;
        if !verified {
            // Schlüsselwechsel beim IdP: JWKS (gedrosselt) neu laden und erneut prüfen.
            verified = self
                .verify_with_jwks(&header, signing_input.as_bytes(), &sig, true)
                .await?;
        }
        if !verified {
            return Err(err(format!(
                "Signatur ungültig oder kein passender Schlüssel (kid {:?})",
                header.kid
            )));
        }
        let claims: Value =
            serde_json::from_slice(&b64(p64)?).map_err(|e| err(format!("ID-Token-Claims: {e}")))?;
        self.check_claims(
            &claims,
            &meta.issuer,
            nonce,
            OffsetDateTime::now_utc().unix_timestamp(),
        )
    }

    async fn verify_with_jwks(
        &self,
        header: &JwsHeader,
        input: &[u8],
        sig: &[u8],
        force: bool,
    ) -> Result<bool, OidcError> {
        let jwks = self.jwks(force).await?;
        jwks.verify(header, input, sig).map_err(|e| err(e.0))
    }

    fn check_claims(
        &self,
        c: &Value,
        issuer: &str,
        nonce: &str,
        now: i64,
    ) -> Result<IdClaims, OidcError> {
        let s = |k: &str| c.get(k).and_then(Value::as_str);
        if s("iss") != Some(issuer) {
            return Err(err(format!("iss {:?} ≠ {issuer:?}", s("iss"))));
        }
        let aud_ok = match c.get("aud") {
            Some(Value::String(a)) => *a == self.client_id,
            Some(Value::Array(a)) => a.iter().any(|x| x.as_str() == Some(&self.client_id)),
            _ => false,
        };
        if !aud_ok {
            return Err(err("aud enthält die Client-ID nicht"));
        }
        if let Some(azp) = c.get("azp")
            && azp.as_str() != Some(&self.client_id)
        {
            return Err(err("azp passt nicht zur Client-ID"));
        }
        let exp = c
            .get("exp")
            .and_then(Value::as_i64)
            .ok_or_else(|| err("exp fehlt"))?;
        if exp + self.skew < now {
            return Err(err("ID-Token abgelaufen"));
        }
        let iat = c
            .get("iat")
            .and_then(Value::as_i64)
            .ok_or_else(|| err("iat fehlt"))?;
        if iat - self.skew > now {
            return Err(err("iat liegt in der Zukunft"));
        }
        if let Some(nbf) = c.get("nbf").and_then(Value::as_i64)
            && nbf - self.skew > now
        {
            return Err(err("ID-Token noch nicht gültig (nbf)"));
        }
        match s("nonce") {
            Some(n) if crate::auth::secure_eq(n, nonce) => {}
            _ => return Err(err("nonce fehlt oder passt nicht")),
        }
        // `sub` ist nach OIDC Core Pflicht – auch wenn die Kennung aus einem anderen Claim kommt.
        s("sub")
            .filter(|x| !x.is_empty())
            .ok_or_else(|| err("sub fehlt"))?;
        if let Some(tenant) = &self.tenant {
            match s("tid") {
                Some(t) if t.eq_ignore_ascii_case(tenant) => {}
                other => {
                    return Err(err(format!(
                        "tid {other:?} passt nicht zum konfigurierten Tenant (Profil entra)"
                    )));
                }
            }
        }
        let subject = s(&self.identity_claim)
            .filter(|x| !x.is_empty())
            .ok_or_else(|| err(format!("Kennungs-Claim {} fehlt", self.identity_claim)))?
            .to_owned();
        let email_verified = match c.get("email_verified") {
            Some(Value::Bool(b)) => *b,
            // Manche IdPs liefern "true" als Zeichenkette.
            Some(Value::String(v)) => v == "true",
            _ => false,
        };
        Ok(IdClaims {
            issuer: issuer.to_owned(),
            subject,
            email: s("email").map(str::to_owned),
            email_verified,
            name: s("name").map(str::to_owned),
            preferred_username: s("preferred_username").map(str::to_owned),
            groups: claim_strings(c, &self.groups_claim),
        })
    }

    /// URL für die Abmeldung beim IdP (RP-initiated Logout), falls angeboten.
    pub async fn end_session_url(&self, post_logout_redirect: &str) -> Option<String> {
        let meta = match self.metadata().await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "Discovery für Logout fehlgeschlagen");
                return None;
            }
        };
        let ep = meta.end_session_endpoint.as_deref()?;
        let mut url = check_url(ep, "end_session_endpoint").ok()?;
        url.query_pairs_mut()
            .append_pair("client_id", &self.client_id)
            .append_pair("post_logout_redirect_uri", post_logout_redirect);
        Some(url.into())
    }
}

fn b64(s: &str) -> Result<Vec<u8>, OidcError> {
    aigw_jwt::jws::b64(s).map_err(|e| err(e.0))
}

/// Base64url ohne Auffüllung (für PKCE und zufällige Werte).
pub fn b64url(bytes: &[u8]) -> String {
    aigw_jwt::jws::b64url(bytes)
}

/// PKCE-Challenge (S256) zu einem Verifier.
pub fn pkce_challenge(verifier: &str) -> String {
    b64url(&crate::auth::sha256(verifier))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn client() -> OidcClient {
        let cfg = OidcConfig {
            issuer: Some("https://idp.example".into()),
            client_id: "aigw".into(),
            redirect_url: Some("https://gw.example/admin/auth/callback".into()),
            ..OidcConfig::default()
        };
        OidcClient::new(&cfg, None).unwrap()
    }

    #[test]
    fn url_rules() {
        assert!(check_url("https://idp.example", "x").is_ok());
        assert!(check_url("http://127.0.0.1:8080", "x").is_ok());
        assert!(check_url("http://localhost:8080", "x").is_ok());
        assert!(check_url("http://[::1]:8080", "x").is_ok());
        assert!(check_url("http://idp.example", "x").is_err());
        assert!(check_url("ftp://idp.example", "x").is_err());
        assert!(check_url("nonsense", "x").is_err());
    }

    #[test]
    fn config_validation() {
        let mut cfg = OidcConfig {
            issuer: Some("https://idp.example".into()),
            client_id: "aigw".into(),
            redirect_url: Some("https://gw.example/elsewhere".into()),
            ..OidcConfig::default()
        };
        assert!(OidcClient::new(&cfg, None).is_err());
        cfg.redirect_url = None;
        assert!(OidcClient::new(&cfg, None).is_err());
        cfg.redirect_url = Some("https://gw.example/admin/auth/callback".into());
        cfg.client_id = String::new();
        assert!(OidcClient::new(&cfg, None).is_err());
    }

    #[test]
    fn pkce_rfc7636_example() {
        // RFC 7636, Anhang B
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn claims_checks() {
        let c = client();
        let now = 1_000_000;
        let good = json!({"iss": "https://idp.example", "aud": ["aigw", "x"], "azp": "aigw",
            "sub": "u1", "exp": now + 60, "iat": now, "nonce": "n1",
            "email": "a@x", "email_verified": true,
            "realm_access": {"roles": ["r1", "r2"]}, "groups": ["g1"]});
        let ok = c
            .check_claims(&good, "https://idp.example", "n1", now)
            .unwrap();
        assert_eq!(ok.subject, "u1");
        assert_eq!(ok.groups, vec!["g1".to_owned()]);
        assert!(ok.email_verified);
        let with = |k: &str, v: Value| {
            let mut x = good.clone();
            x[k] = v;
            c.check_claims(&x, "https://idp.example", "n1", now)
        };
        assert!(with("iss", json!("https://evil")).is_err());
        assert!(with("aud", json!("other")).is_err());
        assert!(with("azp", json!("other")).is_err());
        assert!(with("exp", json!(now - 120)).is_err());
        assert!(with("exp", json!(now - 30)).is_ok()); // Uhrenabweichung
        assert!(with("iat", json!(now + 600)).is_err());
        assert!(with("nbf", json!(now + 600)).is_err());
        assert!(with("nonce", json!("n2")).is_err());
        assert!(with("sub", json!("")).is_err());
        let mut no_sub = good.clone();
        no_sub.as_object_mut().unwrap().remove("sub");
        assert!(
            c.check_claims(&no_sub, "https://idp.example", "n1", now)
                .is_err()
        );
        assert_eq!(
            claim_strings(&good, "realm_access.roles"),
            vec!["r1".to_owned(), "r2".to_owned()]
        );
        assert!(claim_strings(&good, "missing.path").is_empty());
    }

    #[test]
    fn identity_claim_and_entra_tenant() {
        let now = 1_000_000;
        let token = |tid: &str| {
            json!({"iss": "https://idp.example", "aud": "aigw", "sub": "pairwise-1",
                "oid": "00000000-0000-0000-0000-0000000000aa", "tid": tid,
                "exp": now + 60, "iat": now, "nonce": "n1"})
        };
        // eigener Claim (generic)
        let cfg = OidcConfig {
            issuer: Some("https://idp.example".into()),
            client_id: "aigw".into(),
            redirect_url: Some("https://gw.example/admin/auth/callback".into()),
            identity_claim: Some("oid".into()),
            ..OidcConfig::default()
        };
        let c = OidcClient::new(&cfg, None).unwrap();
        let ok = c
            .check_claims(&token("t1"), "https://idp.example", "n1", now)
            .unwrap();
        assert_eq!(ok.subject, "00000000-0000-0000-0000-0000000000aa");
        // entra: oid + Tenant-Prüfung, tenant_id Pflicht
        let entra = OidcConfig {
            identity_claim: None,
            profile: OidcProfile::Entra,
            tenant_id: Some("T1".into()),
            ..cfg.clone()
        };
        let c = OidcClient::new(&entra, None).unwrap();
        let ok = c
            .check_claims(&token("t1"), "https://idp.example", "n1", now)
            .unwrap();
        assert_eq!(ok.subject, "00000000-0000-0000-0000-0000000000aa");
        let e = c
            .check_claims(&token("t2"), "https://idp.example", "n1", now)
            .unwrap_err();
        assert!(e.to_string().contains("tid"), "{e}");
        let mut no_oid = token("t1");
        no_oid.as_object_mut().unwrap().remove("oid");
        assert!(
            c.check_claims(&no_oid, "https://idp.example", "n1", now)
                .is_err()
        );
        assert!(
            OidcClient::new(
                &OidcConfig {
                    tenant_id: None,
                    ..entra
                },
                None
            )
            .is_err()
        );
    }
}
