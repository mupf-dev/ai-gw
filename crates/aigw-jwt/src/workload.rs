// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Prüfung von Access-Tokens für Workload-/Agent-Identitäten (AIGW-115).
//!
//! Ablauf je Anfrage (Konzept: `docs/konzept/18-workload-identitaeten.md`):
//! 1. [`WorkloadVerifier::prepare`] (async, in der Auth-Middleware vor dem Body): lädt bei
//!    Bedarf das JWKS des Ausstellers nach – erstmalig, nach Ablauf von `jwks_cache_secs` oder
//!    gedrosselt bei unbekannter `kid` (Schlüsselwechsel). Abrufe laufen einzeln je Aussteller
//!    über den gemeinsamen Outbound-Baustein (Zeit- und Größenlimit, keine Weiterleitungen).
//! 2. [`WorkloadVerifier::verify`] (synchron, im RAM): Größe, Format, Verfahren (Allowlist
//!    RS256/ES256), Signatur gegen das zwischengespeicherte JWKS, `iss` (exakt), `aud`,
//!    `exp`/`nbf`/`iat` mit Uhrenabweichung, beim Profil `entra` `tid`; danach optionale
//!    Rollen-/Scope-/Agent-Pflicht. Erfolgreich geprüfte Tokens liegen bis `exp` im Cache
//!    (Schlüssel SHA-256 des Tokens, nie das Token selbst).
//!
//! Fehlertexte gehen nur ins Debug-Log; der Client erhält einheitlich `401` bzw. `403`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use aigw_core::workload::{
    JwksSource, JwtAuthConfig, JwtProfile, ResolvedIssuer, WorkloadPrincipal, WorkloadTokenKind,
    check_https_url,
};
use aigw_outbound::{AddressGuard, FetchLimits, OutboundClient, OutboundOptions};
use aws_lc_rs::digest;
use bytes::Bytes;
use hyper::Request;
use hyper::header::ACCEPT;
use serde_json::Value;

use crate::jws::{Jwks, claim_strings, looks_like_jwt, parse_compact};

/// Mindestabstand zwischen zwei Abrufversuchen, solange noch gar kein JWKS geladen ist
/// (Aussteller kurz nicht erreichbar).
const INITIAL_RETRY: Duration = Duration::from_secs(2);

/// Ergebnis einer fehlgeschlagenen Prüfung.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    /// Token ungültig (Format, Signatur, Aussteller, Zielgruppe, Zeit) → `401`.
    #[error("Token ungültig: {0}")]
    Invalid(String),
    /// Token gültig, aber ohne erforderliche Rolle/Scope bzw. keine Agent-Identität → `403`.
    #[error("Token ohne Berechtigung: {0}")]
    Forbidden(String),
}

fn invalid(s: impl Into<String>) -> TokenError {
    TokenError::Invalid(s.into())
}

struct KeySet {
    jwks: Jwks,
    at: Instant,
}

struct IssuerState {
    cfg: ResolvedIssuer,
    keys: RwLock<Option<Arc<KeySet>>>,
    /// Letzter Abrufversuch (auch fehlgeschlagene); zugleich Single-Flight-Sperre.
    fetch: tokio::sync::Mutex<Option<Instant>>,
}

impl IssuerState {
    fn keys(&self) -> Option<Arc<KeySet>> {
        self.keys.read().ok().and_then(|g| g.clone())
    }
}

/// Prüfer für Workload-Tokens aller konfigurierten Aussteller.
pub struct WorkloadVerifier {
    issuers: Vec<IssuerState>,
    by_iss: HashMap<String, usize>,
    skew: i64,
    max_token: usize,
    cache_cap: usize,
    cache: Mutex<HashMap<[u8; 32], Arc<WorkloadPrincipal>>>,
    http: OutboundClient,
    timeout: Duration,
    max_jwks: usize,
    ttl: Duration,
    refresh_min: Duration,
}

impl std::fmt::Debug for WorkloadVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkloadVerifier")
            .field(
                "issuers",
                &self.issuers.iter().map(|i| &i.cfg.name).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl WorkloadVerifier {
    /// Baut den Prüfer; `Ok(None)`, wenn keine Aussteller konfiguriert sind.
    ///
    /// Die Aussteller stammen aus der Startkonfiguration des Betreibers (wie der OIDC-IdP der
    /// Admin-UI) – daher ohne SSRF-Adressfilter, aber nur `https://` (`http://` für Loopback),
    /// ohne Weiterleitungen und mit Zeit- und Größenlimit.
    pub fn new(cfg: &JwtAuthConfig) -> Result<Option<Self>, String> {
        if !cfg.enabled() {
            return Ok(None);
        }
        cfg.validate()?;
        let mut tls = aigw_outbound::default_tls();
        if let Some(ca) = &cfg.ca_file {
            tls = tls.with_ca_file(ca);
        }
        let http = OutboundClient::builder(AddressGuard::Trusted)
            .options(OutboundOptions {
                connect_timeout: Duration::from_secs(cfg.http_timeout_secs.max(1)),
                pool_max_idle_per_host: 2,
                tls: Some(tls),
                ..OutboundOptions::default()
            })
            .build()
            .map_err(|e| format!("auth.jwt: {e}"))?;
        let mut issuers = Vec::new();
        let mut by_iss = HashMap::new();
        for (i, c) in cfg.issuers.iter().enumerate() {
            let r = c
                .resolve()
                .map_err(|e| format!("auth.jwt.issuers[{}]: {e}", c.name))?;
            for iss in &r.issuers {
                by_iss.insert(iss.clone(), i);
            }
            issuers.push(IssuerState {
                cfg: r,
                keys: RwLock::new(None),
                fetch: tokio::sync::Mutex::new(None),
            });
        }
        Ok(Some(Self {
            issuers,
            by_iss,
            skew: i64::try_from(cfg.clock_skew_secs).unwrap_or(60),
            max_token: cfg.max_token_bytes,
            cache_cap: cfg.token_cache_entries,
            cache: Mutex::new(HashMap::new()),
            http,
            timeout: Duration::from_secs(cfg.http_timeout_secs.max(1)),
            max_jwks: cfg.max_jwks_bytes,
            ttl: Duration::from_secs(cfg.jwks_cache_secs.max(1)),
            refresh_min: Duration::from_secs(cfg.jwks_refresh_min_secs),
        }))
    }

    /// Ist `token` ein Kandidat für die JWT-Prüfung (Format, Größe)?
    pub fn is_candidate(&self, token: &str) -> bool {
        token.len() <= self.max_token && looks_like_jwt(token)
    }

    fn cache_key(token: &str) -> [u8; 32] {
        let d = digest::digest(&digest::SHA256, token.as_bytes());
        let mut k = [0u8; 32];
        k.copy_from_slice(d.as_ref());
        k
    }

    fn cached(&self, key: &[u8; 32], now: i64) -> Option<Arc<WorkloadPrincipal>> {
        let cache = self.cache.lock().ok()?;
        let p = cache.get(key)?;
        (p.expires_at + self.skew >= now).then(|| Arc::clone(p))
    }

    /// Lädt bei Bedarf das JWKS des Ausstellers des Tokens (async, vor [`Self::verify`]).
    /// Fehler werden nur protokolliert – die anschließende Prüfung schlägt dann fehl.
    pub async fn prepare(&self, token: &str) {
        if !self.is_candidate(token) {
            return;
        }
        let now = now_unix();
        if self.cached(&Self::cache_key(token), now).is_some() {
            return;
        }
        let Ok(c) = parse_compact(token) else { return };
        let Some(st) = c
            .claims
            .get("iss")
            .and_then(Value::as_str)
            .and_then(|iss| self.by_iss.get(iss))
            .map(|i| &self.issuers[*i])
        else {
            return;
        };
        let needs = |keys: Option<&Arc<KeySet>>| match keys {
            None => true,
            Some(k) => {
                k.at.elapsed() >= self.ttl
                    || c.header
                        .kid
                        .as_deref()
                        .is_some_and(|kid| !k.jwks.has_kid(kid))
            }
        };
        if !needs(st.keys().as_ref()) {
            return;
        }
        let mut last = st.fetch.lock().await;
        // Ein anderer Aufruf hat inzwischen geladen?
        let current = st.keys();
        if !needs(current.as_ref()) {
            return;
        }
        let expired = current.as_ref().is_none_or(|k| k.at.elapsed() >= self.ttl);
        let min_gap = if current.is_none() {
            INITIAL_RETRY
        } else if expired {
            Duration::ZERO
        } else {
            self.refresh_min
        };
        if last.is_some_and(|t| t.elapsed() < min_gap) {
            return;
        }
        *last = Some(Instant::now());
        match self.fetch_jwks(&st.cfg).await {
            Ok(jwks) => {
                tracing::debug!(issuer = %st.cfg.name, keys = jwks.keys.len(), "JWKS geladen");
                if let Ok(mut g) = st.keys.write() {
                    *g = Some(Arc::new(KeySet {
                        jwks,
                        at: Instant::now(),
                    }));
                }
            }
            // Alte Schlüssel bleiben in Gebrauch (Aussteller kurz nicht erreichbar).
            Err(e) => {
                tracing::warn!(issuer = %st.cfg.name, error = %e, "JWKS-Abruf fehlgeschlagen")
            }
        }
    }

    async fn get_json(&self, url: &str) -> Result<Value, String> {
        check_https_url(url, "JWKS/Discovery")?;
        let req = Request::get(url)
            .header(ACCEPT, "application/json")
            .body(Bytes::new())
            .map_err(|e| format!("{url}: {e}"))?;
        let reply = self
            .http
            .fetch(req, &FetchLimits::new(self.timeout, self.max_jwks))
            .await
            .map_err(|e| format!("{url}: {e}"))?;
        if reply.status.as_u16() != 200 {
            return Err(format!("{url}: HTTP {}", reply.status.as_u16()));
        }
        serde_json::from_slice(&reply.body).map_err(|e| format!("{url}: ungültiges JSON: {e}"))
    }

    async fn fetch_jwks(&self, cfg: &ResolvedIssuer) -> Result<Jwks, String> {
        let uri = match &cfg.jwks {
            JwksSource::Uri(u) => u.clone(),
            JwksSource::Discovery(d) => {
                let doc = self.get_json(d).await?;
                let iss = doc
                    .get("issuer")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !cfg.issuers.iter().any(|i| i == iss) {
                    return Err(format!(
                        "Discovery: issuer {iss:?} gehört nicht zur Konfiguration"
                    ));
                }
                doc.get("jwks_uri")
                    .and_then(Value::as_str)
                    .ok_or("Discovery ohne jwks_uri")?
                    .to_owned()
            }
        };
        let v = self.get_json(&uri).await?;
        serde_json::from_value(v).map_err(|e| format!("{uri}: kein JWKS: {e}"))
    }

    /// Prüft ein Token synchron gegen die zwischengespeicherten Schlüssel.
    pub fn verify(&self, token: &str, now: i64) -> Result<Arc<WorkloadPrincipal>, TokenError> {
        if token.len() > self.max_token {
            return Err(invalid("Token zu groß"));
        }
        if !looks_like_jwt(token) {
            return Err(invalid("kein JWT"));
        }
        let key = Self::cache_key(token);
        if let Some(p) = self.cached(&key, now) {
            return Ok(p);
        }
        let c = parse_compact(token).map_err(|e| invalid(e.0))?;
        let iss = c
            .claims
            .get("iss")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("iss fehlt"))?;
        let st = self
            .by_iss
            .get(iss)
            .map(|i| &self.issuers[*i])
            .ok_or_else(|| invalid(format!("unbekannter Aussteller {iss:?}")))?;
        if !st.cfg.algorithms.contains(&c.header.alg) {
            return Err(invalid(format!(
                "Signaturverfahren {:?} nicht erlaubt",
                c.header.alg
            )));
        }
        let keys = st
            .keys()
            .ok_or_else(|| invalid("JWKS des Ausstellers nicht verfügbar"))?;
        let ok = keys
            .jwks
            .verify(&c.header, c.signing_input, &c.signature)
            .map_err(|e| invalid(e.0))?;
        if !ok {
            return Err(invalid(format!(
                "Signatur ungültig oder kein passender Schlüssel (kid {:?})",
                c.header.kid
            )));
        }
        let p = Arc::new(check_claims(&st.cfg, &c.claims, now, self.skew)?);
        self.remember(key, &p, now);
        Ok(p)
    }

    fn remember(&self, key: [u8; 32], p: &Arc<WorkloadPrincipal>, now: i64) {
        if self.cache_cap == 0 {
            return;
        }
        let Ok(mut cache) = self.cache.lock() else {
            return;
        };
        if cache.len() >= self.cache_cap {
            let skew = self.skew;
            cache.retain(|_, v| v.expires_at + skew >= now);
            if cache.len() >= self.cache_cap {
                cache.clear();
            }
        }
        cache.insert(key, Arc::clone(p));
    }

    /// Zahl der Tokens im Cache (Tests, Diagnose).
    pub fn cached_tokens(&self) -> usize {
        self.cache.lock().map_or(0, |c| c.len())
    }
}

/// Aktuelle Unix-Zeit (Sekunden).
pub fn now_unix() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// Mehrwertiger Facetten-Claim (`xms_act_fct`, `xms_sub_fct`): durch Leerzeichen getrennte
/// Zahlen (laut Doku) – Arrays aus Zahlen oder Zeichenketten werden ebenfalls gelesen.
fn facet_contains(c: &Value, claim: &str, value: &str) -> bool {
    match c.get(claim) {
        Some(Value::String(s)) => s.split_whitespace().any(|x| x == value),
        Some(Value::Array(a)) => a.iter().any(|x| match x {
            Value::String(s) => s == value,
            Value::Number(n) => n.to_string() == value,
            _ => false,
        }),
        Some(Value::Number(n)) => n.to_string() == value,
        _ => false,
    }
}

/// Prüft die Claims eines Tokens mit gültiger Signatur (rein, ohne Netz).
pub fn check_claims(
    cfg: &ResolvedIssuer,
    c: &Value,
    now: i64,
    skew: i64,
) -> Result<WorkloadPrincipal, TokenError> {
    let s = |k: &str| c.get(k).and_then(Value::as_str);
    match s("iss") {
        Some(i) if cfg.issuers.iter().any(|x| x == i) => {}
        _ => return Err(invalid("iss passt nicht")),
    }
    let aud_ok = match c.get("aud") {
        Some(Value::String(a)) => cfg.audiences.contains(a),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .any(|x| cfg.audiences.iter().any(|y| y == x)),
        _ => false,
    };
    if !aud_ok {
        return Err(invalid("aud passt nicht zum Gateway"));
    }
    let exp = c
        .get("exp")
        .and_then(Value::as_i64)
        .ok_or_else(|| invalid("exp fehlt"))?;
    if exp + skew < now {
        return Err(invalid("Token abgelaufen"));
    }
    if let Some(nbf) = c.get("nbf").and_then(Value::as_i64)
        && nbf - skew > now
    {
        return Err(invalid("Token noch nicht gültig (nbf)"));
    }
    if let Some(iat) = c.get("iat").and_then(Value::as_i64)
        && iat - skew > now
    {
        return Err(invalid("iat liegt in der Zukunft"));
    }
    let tenant = s("tid").map(str::to_ascii_lowercase);
    if let Some(expected) = &cfg.tenant_id
        && tenant.as_deref() != Some(expected.as_str())
    {
        return Err(invalid("tid passt nicht zum konfigurierten Tenant"));
    }
    let subject = cfg
        .identity_claims
        .iter()
        .find_map(|k| s(k).map(str::trim).filter(|v| !v.is_empty()))
        .ok_or_else(|| invalid("Kennungs-Claim fehlt"))?;
    if subject.len() > aigw_core::workload::MAX_IDENTITY_LEN
        || subject.chars().any(char::is_control)
    {
        return Err(invalid("Kennungs-Claim ungültig"));
    }
    let scopes: Vec<&str> = s("scp")
        .map(|x| x.split_whitespace().collect())
        .unwrap_or_default();
    let roles = claim_strings(c, "roles");
    let entra = cfg.profile == JwtProfile::Entra;
    let kind = if entra {
        match s("idtyp") {
            Some("app") => WorkloadTokenKind::App,
            Some("user") if facet_contains(c, "xms_sub_fct", "13") => WorkloadTokenKind::AgentUser,
            Some("user") => WorkloadTokenKind::Delegated,
            // `idtyp` ist bei klassischen App-Tokens ein optionaler Claim: ohne ihn gilt ein
            // Token mit `scp` als delegiert, sonst als App-only.
            _ if !scopes.is_empty() => WorkloadTokenKind::Delegated,
            _ => WorkloadTokenKind::App,
        }
    } else {
        WorkloadTokenKind::Unknown
    };
    let agent = entra && facet_contains(c, "xms_act_fct", "11");
    if cfg.require_agent && !agent {
        return Err(TokenError::Forbidden(
            "keine Agent-Identität (xms_act_fct ohne 11)".into(),
        ));
    }
    if !cfg.required_roles.is_empty() || !cfg.required_scopes.is_empty() {
        let has_role = roles.iter().any(|r| cfg.required_roles.contains(r));
        let has_scope = scopes
            .iter()
            .any(|x| cfg.required_scopes.iter().any(|y| y == x));
        if !has_role && !has_scope {
            return Err(TokenError::Forbidden(
                "erforderliche Rolle bzw. erforderlicher Scope fehlt".into(),
            ));
        }
    }
    let user = match kind {
        WorkloadTokenKind::App => None,
        _ => cfg
            .user_claim
            .as_deref()
            .and_then(s)
            .filter(|v| !v.is_empty() && v.len() <= 256)
            .map(str::to_owned),
    };
    Ok(WorkloadPrincipal {
        issuer: cfg.name.clone(),
        subject: subject.to_owned(),
        kind,
        agent,
        user,
        blueprint: s("xms_par_app_azp")
            .filter(|v| !v.is_empty() && v.len() <= 64)
            .map(str::to_owned),
        tenant,
        expires_at: exp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigw_core::workload::JwtIssuerConfig;
    use serde_json::json;

    const TID: &str = "aaaabbbb-0000-cccc-1111-dddd2222eeee";
    const AGENT: &str = "11112222-bbbb-3333-cccc-4444dddd5555";
    const NOW: i64 = 1_753_400_000;

    fn cfg() -> ResolvedIssuer {
        JwtIssuerConfig {
            name: "entra".into(),
            profile: JwtProfile::Entra,
            tenant_id: Some(TID.into()),
            audiences: vec![
                "api://ai-gw".into(),
                "00001111-aaaa-2222-bbbb-3333cccc4444".into(),
            ],
            ..JwtIssuerConfig::default()
        }
        .resolve()
        .unwrap()
    }

    /// Beispiel „Agent handelt autonom“ aus der Microsoft-Doku (agent-token-claims, v1).
    fn autonomous() -> Value {
        json!({
            "aud": "00001111-aaaa-2222-bbbb-3333cccc4444",
            "iss": format!("https://sts.windows.net/{TID}/"),
            "iat": NOW - 10, "nbf": NOW - 10, "exp": NOW + 3600,
            "appid": AGENT, "appidacr": "2", "idtyp": "app",
            "oid": "aaaaaaaa-0000-1111-2222-bbbbbbbbbbbb",
            "sub": "bbbbbbbb-1111-2222-3333-cccccccccccc",
            "tid": TID, "ver": "1.0",
            "xms_act_fct": "3 9 11", "xms_idrel": "7 10", "xms_sub_fct": "9 3 11",
            "xms_tnt_fct": "3 9",
            "xms_par_app_azp": "30cf4c22-9985-4ef7-8756-91cc888176bd"
        })
    }

    #[test]
    fn documented_autonomous_agent_token() {
        let p = check_claims(&cfg(), &autonomous(), NOW, 60).unwrap();
        assert_eq!(p.subject, AGENT);
        assert_eq!(p.kind, WorkloadTokenKind::App);
        assert!(p.agent);
        assert_eq!(p.user, None);
        assert_eq!(
            p.blueprint.as_deref(),
            Some("30cf4c22-9985-4ef7-8756-91cc888176bd")
        );
        assert_eq!(p.tenant.as_deref(), Some(TID));
    }

    #[test]
    fn obo_and_agent_user_tokens() {
        // v2: azp statt appid, Aussteller login.microsoftonline.com/<tid>/v2.0.
        let mut c = autonomous();
        let o = c.as_object_mut().unwrap();
        o.remove("appid");
        o.insert("azp".into(), json!(AGENT));
        o.insert(
            "iss".into(),
            json!(format!("https://login.microsoftonline.com/{TID}/v2.0")),
        );
        o.insert("aud".into(), json!("api://ai-gw"));
        o.insert("idtyp".into(), json!("user"));
        o.insert("scp".into(), json!("access_as_user"));
        o.insert("oid".into(), json!("user-oid"));
        o.insert("xms_sub_fct".into(), json!("1"));
        let p = check_claims(&cfg(), &c, NOW, 60).unwrap();
        assert_eq!(
            (p.kind, p.user.as_deref()),
            (WorkloadTokenKind::Delegated, Some("user-oid"))
        );
        c["xms_sub_fct"] = json!("13");
        let p = check_claims(&cfg(), &c, NOW, 60).unwrap();
        assert_eq!(p.kind, WorkloadTokenKind::AgentUser);
        assert_eq!(p.subject, AGENT, "Zuordnung immer über die Agent-App-ID");
    }

    #[test]
    fn rejects_wrong_claims() {
        let check = |f: &dyn Fn(&mut Value)| {
            let mut c = autonomous();
            f(&mut c);
            check_claims(&cfg(), &c, NOW, 60)
        };
        assert!(matches!(
            check(&|c| c["aud"] = json!("api://anders")),
            Err(TokenError::Invalid(_))
        ));
        assert!(check(&|c| c["iss"] = json!("https://sts.windows.net/evil/")).is_err());
        assert!(check(&|c| c["tid"] = json!("99999999-0000-cccc-1111-dddd2222eeee")).is_err());
        assert!(check(&|c| c["exp"] = json!(NOW - 61)).is_err());
        assert!(
            check(&|c| c["exp"] = json!(NOW - 59)).is_ok(),
            "Uhrenabweichung"
        );
        assert!(check(&|c| c["nbf"] = json!(NOW + 120)).is_err());
        assert!(check(&|c| c["iat"] = json!(NOW + 120)).is_err());
        assert!(
            check(&|c| {
                c.as_object_mut().unwrap().remove("exp");
            })
            .is_err()
        );
        assert!(
            check(&|c| {
                c.as_object_mut().unwrap().remove("appid");
            })
            .is_err()
        );
        assert!(check(&|c| c["aud"] = json!(["x", "api://ai-gw"])).is_ok());
    }

    #[test]
    fn roles_scopes_and_agent_requirement() {
        let mut r = cfg();
        r.required_roles = vec!["Gateway.Invoke".into()];
        let c = autonomous();
        assert!(matches!(
            check_claims(&r, &c, NOW, 60),
            Err(TokenError::Forbidden(_))
        ));
        let mut c2 = c.clone();
        c2["roles"] = json!(["Gateway.Invoke"]);
        assert!(check_claims(&r, &c2, NOW, 60).is_ok());
        r.required_scopes = vec!["access_as_user".into()];
        let mut c3 = c.clone();
        c3["scp"] = json!("openid access_as_user");
        assert!(check_claims(&r, &c3, NOW, 60).is_ok());

        let mut r = cfg();
        r.require_agent = true;
        let mut c4 = c.clone();
        c4["xms_act_fct"] = json!("3 9");
        assert!(matches!(
            check_claims(&r, &c4, NOW, 60),
            Err(TokenError::Forbidden(_))
        ));
        c4["xms_act_fct"] = json!([3, 11]);
        assert!(check_claims(&r, &c4, NOW, 60).is_ok());
    }

    #[test]
    fn classic_app_token_without_idtyp() {
        let mut c = autonomous();
        let o = c.as_object_mut().unwrap();
        for k in ["idtyp", "xms_act_fct", "xms_sub_fct", "xms_par_app_azp"] {
            o.remove(k);
        }
        let p = check_claims(&cfg(), &c, NOW, 60).unwrap();
        assert_eq!(p.kind, WorkloadTokenKind::App);
        assert!(!p.agent);
        c["scp"] = json!("x");
        assert_eq!(
            check_claims(&cfg(), &c, NOW, 60).unwrap().kind,
            WorkloadTokenKind::Delegated
        );
    }
}
