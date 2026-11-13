// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Authentifizierung der Admin-API (austauschbare Schicht, AIGW-28/32).
//!
//! Die Middleware [`require_principal`] fragt einen [`AdminAuthenticator`] und legt den
//! angemeldeten [`Principal`] in die Request-Extensions; Handler prüfen über
//! [`Principal::require`] bzw. [`Principal::require_on`] die nötige [`Permission`] auf einem
//! [`Target`] (Rollenmodell: [`crate::rbac`]).
//!
//! Verfahren (Reihenfolge in [`AuthChain`]):
//! 1. `Authorization: Bearer sk-aigw-admin-…` – Admin-API-Key ([`crate::admin_keys`]),
//! 2. `Authorization: Bearer <bootstrap>` – Bootstrap-Token ([`BootstrapGate`]),
//! 3. ohne `Authorization`: Session-Cookie aus dem OIDC-Login ([`crate::session`]).
//!
//! Schreibende Anfragen mit Session-Cookie brauchen zusätzlich `X-CSRF-Token` und – falls der
//! Browser `Origin`/`Sec-Fetch-Site` sendet – einen passenden Ursprung (CSRF-Schutz zusätzlich
//! zu `SameSite=Strict`).

use std::fmt::{self, Write as _};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aigw_core::config::BootstrapMode;
use aigw_core::resource::{Role, RoleBinding};
use aigw_store::Store;
use aws_lc_rs::digest;
use aws_lc_rs::rand::{SecureRandom as _, SystemRandom};
use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, Method};
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};
use time::OffsetDateTime;

use crate::problem::Problem;
pub use crate::rbac::{Permission, Target};

/// Mindestlänge eines vorgegebenen Bootstrap-Tokens.
pub const MIN_TOKEN_LEN: usize = 32;

/// Header mit dem CSRF-Token (schreibende Anfragen mit Session-Cookie).
pub const CSRF_HEADER: &str = "x-csrf-token";

/// Wie sich ein Principal angemeldet hat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    /// Bootstrap-Admin-Token.
    BootstrapToken,
    /// Session nach OIDC-Login (Cookie).
    Session,
    /// Admin-API-Key (`sk-aigw-admin-…`).
    AdminApiKey,
}

impl AuthMethod {
    /// Bezeichnung in API-Antworten.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BootstrapToken => "bootstrap-token",
            Self::Session => "session",
            Self::AdminApiKey => "admin-api-key",
        }
    }
}

/// Erwartetes CSRF-Token einer Session (im `Debug` verborgen).
#[derive(Clone, PartialEq, Eq)]
pub struct CsrfToken(pub(crate) String);

impl CsrfToken {
    /// Klartext (nur für die eigene Session-Antwort).
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for CsrfToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CsrfToken(***)")
    }
}

/// Angemeldeter Akteur der Admin-API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// Kennung für das Audit-Log (`bootstrap-admin`, `user:<name>`, `admin-key:<name>`).
    pub subject: String,
    /// Wirksame Rollen (direkt zugewiesen und aus IdP-Gruppen).
    pub roles: Vec<RoleBinding>,
    /// Anmeldeverfahren.
    pub method: AuthMethod,
    /// Anzeigename (Session).
    pub display_name: Option<String>,
    /// E-Mail (Session).
    pub email: Option<String>,
    /// Ablauf der Anmeldung (Session: absolut; Admin-Key: `expiresAt`).
    pub expires_at: Option<OffsetDateTime>,
    /// CSRF-Token (nur Session).
    pub csrf: Option<CsrfToken>,
}

impl Principal {
    /// Principal mit globalen Rollen (Bootstrap, Tests).
    pub fn global(subject: impl Into<String>, roles: &[Role], method: AuthMethod) -> Self {
        Self {
            subject: subject.into(),
            roles: roles
                .iter()
                .map(|r| RoleBinding {
                    role: *r,
                    organization: None,
                    team: None,
                })
                .collect(),
            method,
            display_name: None,
            email: None,
            expires_at: None,
            csrf: None,
        }
    }

    /// Akteur im Audit-Log.
    pub fn actor(&self) -> &str {
        &self.subject
    }

    /// Prüft eine Berechtigung auf globaler Ebene (Nutzer, Admin-Keys, Anlegen von
    /// Organisationen …). Eingeschränkte Rollen (`org-admin`, `team-admin`) reichen dafür nie.
    pub fn require(&self, perm: Permission) -> Result<(), Problem> {
        self.require_on(perm, &Target::Global)
    }

    /// Prüft eine Berechtigung auf einem Ziel (Organisation, Team, Objekt im Team …).
    pub fn require_on(&self, perm: Permission, target: &Target) -> Result<(), Problem> {
        if self.can(perm, target) {
            Ok(())
        } else {
            Err(Problem::forbidden("Rolle reicht für diese Operation nicht"))
        }
    }

    /// `true`, wenn die Berechtigung besteht (für Listenfilter).
    pub fn can(&self, perm: Permission, target: &Target) -> bool {
        crate::rbac::allows(&self.roles, perm, target)
    }
}

/// Geboxte Future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Prüft die Anmeldung einer Admin-Anfrage. Asynchron (Session-Speicher, Key-Abfrage).
pub trait AdminAuthenticator: Send + Sync + 'static {
    /// Liefert den Principal oder `None` (→ 401).
    fn authenticate<'a>(&'a self, headers: &'a HeaderMap) -> BoxFuture<'a, Option<Principal>>;

    /// Erlaubter `Origin` schreibender Session-Anfragen (CSRF-Prüfung); `None` = `Host`.
    fn allowed_origin(&self) -> Option<&str> {
        None
    }
}

pub(crate) fn sha256(s: &str) -> [u8; 32] {
    let d = digest::digest(&digest::SHA256, s.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    out
}

/// Zufällige Bytes.
pub(crate) fn random_bytes<const N: usize>() -> Result<[u8; N], Problem> {
    let mut raw = [0u8; N];
    SystemRandom::new().fill(&mut raw).map_err(|_| {
        tracing::error!("Zufallsquelle nicht verfügbar");
        Problem::internal()
    })?;
    Ok(raw)
}

/// Vergleich in konstanter Zeit (über die SHA-256-Werte, damit die Länge nichts verrät).
pub(crate) fn secure_eq(a: &str, b: &str) -> bool {
    aws_lc_rs::constant_time::verify_slices_are_equal(&sha256(a), &sha256(b)).is_ok()
}

// ---------------------------------------------------------------------------------------------
// Bootstrap-Token
// ---------------------------------------------------------------------------------------------

/// Bootstrap-Admin-Token (`Authorization: Bearer <token>`). Gespeichert wird nur SHA-256.
#[derive(Clone)]
pub struct BootstrapToken {
    hash: [u8; 32],
}

impl fmt::Debug for BootstrapToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BootstrapToken(***)")
    }
}

impl BootstrapToken {
    /// Vorgegebenes Token (z. B. aus `AIGW_BOOTSTRAP_TOKEN`), mindestens
    /// [`MIN_TOKEN_LEN`] Zeichen.
    pub fn new(token: &str) -> Result<Self, String> {
        let token = token.trim();
        if token.len() < MIN_TOKEN_LEN {
            return Err(format!(
                "Bootstrap-Token zu kurz (mindestens {MIN_TOKEN_LEN} Zeichen)"
            ));
        }
        Ok(Self {
            hash: sha256(token),
        })
    }

    /// Erzeugt ein zufälliges Token (`aigw-admin-<64 hex>`); der Klartext wird nur
    /// zurückgegeben, nicht gespeichert.
    pub fn generate() -> Result<(Self, String), String> {
        let mut raw = [0u8; 32];
        SystemRandom::new()
            .fill(&mut raw)
            .map_err(|_| "Zufallsquelle nicht verfügbar".to_owned())?;
        let mut token = String::from("aigw-admin-");
        for b in raw {
            let _ = write!(token, "{b:02x}");
        }
        Ok((
            Self {
                hash: sha256(&token),
            },
            token,
        ))
    }

    fn matches(&self, presented: &str) -> bool {
        // Vergleich der Hashes (konstante Zeit über gleich lange Werte).
        aws_lc_rs::constant_time::verify_slices_are_equal(&sha256(presented), &self.hash).is_ok()
    }

    fn principal() -> Principal {
        Principal::global(
            "bootstrap-admin",
            &[Role::Admin],
            AuthMethod::BootstrapToken,
        )
    }
}

/// Liest `Authorization: Bearer <token>`.
pub fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let v = headers.get(AUTHORIZATION)?.to_str().ok()?.trim();
    let (scheme, token) = v.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

impl AdminAuthenticator for BootstrapToken {
    fn authenticate<'a>(&'a self, headers: &'a HeaderMap) -> BoxFuture<'a, Option<Principal>> {
        let ok = bearer_token(headers).is_some_and(|t| self.matches(t));
        Box::pin(async move { ok.then(BootstrapToken::principal) })
    }
}

/// Bootstrap-Token mit Abschaltung (`admin.auth.bootstrap`).
///
/// Im Modus `auto` gilt das Token nur, solange in der Datenbank kein Abschaltvermerk steht
/// (gesetzt beim ersten OIDC-Login eines globalen Admins bzw. beim Anlegen eines Admin-API-Keys
/// mit Rolle `admin`, auch von anderen Instanzen). Der Vermerk ist endgültig; ist er einmal
/// gesehen, fragt diese Instanz die Datenbank nicht mehr.
#[derive(Debug)]
pub struct BootstrapGate {
    token: BootstrapToken,
    mode: BootstrapMode,
    store: Store,
    disabled: AtomicBool,
}

impl BootstrapGate {
    /// Neues Gate.
    pub fn new(token: BootstrapToken, mode: BootstrapMode, store: Store) -> Self {
        Self {
            token,
            disabled: AtomicBool::new(mode == BootstrapMode::Off),
            mode,
            store,
        }
    }

    async fn active(&self) -> bool {
        match self.mode {
            BootstrapMode::Off => false,
            BootstrapMode::On => true,
            BootstrapMode::Auto => {
                if self.disabled.load(Ordering::Relaxed) {
                    return false;
                }
                match self.store.bootstrap_disabled().await {
                    Ok(true) => {
                        self.disabled.store(true, Ordering::Relaxed);
                        tracing::info!(
                            "Bootstrap-Token ist abgeschaltet (admin.auth.bootstrap: auto)"
                        );
                        false
                    }
                    Ok(false) => true,
                    Err(e) => {
                        // Im Zweifel ablehnen.
                        tracing::warn!(error = %e, "Zustand des Bootstrap-Tokens nicht lesbar");
                        false
                    }
                }
            }
        }
    }
}

impl AdminAuthenticator for BootstrapGate {
    fn authenticate<'a>(&'a self, headers: &'a HeaderMap) -> BoxFuture<'a, Option<Principal>> {
        let ok = bearer_token(headers).is_some_and(|t| self.token.matches(t));
        Box::pin(async move {
            if ok && self.active().await {
                Some(BootstrapToken::principal())
            } else {
                None
            }
        })
    }
}

/// Mehrere Verfahren: mit `Authorization`-Header nur die Bearer-Verfahren (der Reihe nach),
/// ohne ihn die Session.
#[derive(Default)]
pub struct AuthChain {
    bearer: Vec<Arc<dyn AdminAuthenticator>>,
    session: Option<Arc<dyn AdminAuthenticator>>,
    allowed_origin: Option<String>,
}

impl fmt::Debug for AuthChain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthChain")
            .field("bearer", &self.bearer.len())
            .field("session", &self.session.is_some())
            .finish()
    }
}

impl AuthChain {
    /// Leere Kette.
    pub fn new() -> Self {
        Self::default()
    }

    /// Hängt ein Bearer-Verfahren an.
    #[must_use]
    pub fn bearer(mut self, a: Arc<dyn AdminAuthenticator>) -> Self {
        self.bearer.push(a);
        self
    }

    /// Setzt das Session-Verfahren.
    #[must_use]
    pub fn session(mut self, a: Arc<dyn AdminAuthenticator>) -> Self {
        self.session = Some(a);
        self
    }

    /// Setzt den erlaubten `Origin` (Ursprung der öffentlichen URL).
    #[must_use]
    pub fn allowed_origin(mut self, origin: Option<String>) -> Self {
        self.allowed_origin = origin;
        self
    }
}

impl AdminAuthenticator for AuthChain {
    fn authenticate<'a>(&'a self, headers: &'a HeaderMap) -> BoxFuture<'a, Option<Principal>> {
        Box::pin(async move {
            if headers.contains_key(AUTHORIZATION) {
                for a in &self.bearer {
                    if let Some(p) = a.authenticate(headers).await {
                        return Some(p);
                    }
                }
                None
            } else {
                match &self.session {
                    Some(s) => s.authenticate(headers).await,
                    None => None,
                }
            }
        })
    }

    fn allowed_origin(&self) -> Option<&str> {
        self.allowed_origin.as_deref()
    }
}

// ---------------------------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------------------------

fn is_safe(m: &Method) -> bool {
    matches!(*m, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// CSRF-Prüfung für schreibende Anfragen mit Session-Cookie.
///
/// - `X-CSRF-Token` muss zum Token der Session passen (aus `GET /admin/api/v1/session`),
/// - `Sec-Fetch-Site` darf nicht `cross-site`/`same-site` sein,
/// - ein `Origin`-Header muss dem erlaubten Ursprung (aus `redirect_url`) bzw. dem `Host`
///   entsprechen.
pub(crate) fn check_csrf(
    headers: &HeaderMap,
    expected: Option<&CsrfToken>,
    allowed_origin: Option<&str>,
) -> Result<(), Problem> {
    let presented = headers.get(CSRF_HEADER).and_then(|v| v.to_str().ok());
    let token_ok = matches!((presented, expected), (Some(p), Some(e)) if secure_eq(p, &e.0));
    if !token_ok {
        return Err(Problem::forbidden(
            "CSRF-Token fehlt oder passt nicht (Header X-CSRF-Token)",
        ));
    }
    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok())
        && (site.eq_ignore_ascii_case("cross-site") || site.eq_ignore_ascii_case("same-site"))
    {
        return Err(Problem::forbidden("Anfrage von fremdem Ursprung"));
    }
    if let Some(origin) = headers.get("origin") {
        let origin = origin.to_str().unwrap_or("");
        let ok = match allowed_origin {
            Some(a) => origin.eq_ignore_ascii_case(a),
            None => headers
                .get("host")
                .and_then(|h| h.to_str().ok())
                .is_some_and(|host| {
                    origin
                        .split_once("://")
                        .is_some_and(|(_, rest)| rest.eq_ignore_ascii_case(host))
                }),
        };
        if !ok {
            return Err(Problem::forbidden("Anfrage von fremdem Ursprung"));
        }
    }
    Ok(())
}

/// Middleware: verlangt einen angemeldeten Principal, prüft bei Sessions CSRF und legt den
/// Principal in die Extensions.
pub async fn require_principal(
    State(auth): State<Arc<dyn AdminAuthenticator>>,
    mut req: Request,
    next: Next,
) -> Response {
    match auth.authenticate(req.headers()).await {
        Some(p) => {
            if p.method == AuthMethod::Session
                && !is_safe(req.method())
                && let Err(e) = check_csrf(req.headers(), p.csrf.as_ref(), auth.allowed_origin())
            {
                return e.at(req.uri().path().to_owned()).into_response();
            }
            req.extensions_mut().insert(p);
            next.run(req).await
        }
        None => Problem::unauthorized()
            .at(req.uri().path().to_owned())
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[tokio::test]
    async fn bootstrap_token_checks_bearer() {
        let (tok, secret) = BootstrapToken::generate().unwrap();
        assert!(secret.starts_with("aigw-admin-") && secret.len() == 11 + 64);
        assert!(!format!("{tok:?}").contains(&secret));
        let p = tok
            .authenticate(&headers(&[("authorization", &format!("Bearer {secret}"))]))
            .await
            .unwrap();
        assert_eq!(p.actor(), "bootstrap-admin");
        assert!(p.require(Permission::Write).is_ok());
        assert!(
            tok.authenticate(&headers(&[("authorization", "Bearer falsch")]))
                .await
                .is_none()
        );
        assert!(tok.authenticate(&HeaderMap::new()).await.is_none());
        assert!(BootstrapToken::new("kurz").is_err());
        assert!(BootstrapToken::new(&"x".repeat(32)).is_ok());
    }

    #[test]
    fn viewer_may_only_read() {
        let p = Principal::global("v", &[Role::Viewer], AuthMethod::BootstrapToken);
        assert!(p.require(Permission::Read).is_ok());
        assert_eq!(p.require(Permission::Write).unwrap_err().status, 403);
    }

    #[test]
    fn csrf_rules() {
        let t = CsrfToken("abc".into());
        assert!(format!("{t:?}").contains("***"));
        let ok = |h: &HeaderMap, origin: Option<&str>| check_csrf(h, Some(&t), origin).is_ok();
        assert!(ok(&headers(&[(CSRF_HEADER, "abc")]), None));
        assert!(!ok(&headers(&[(CSRF_HEADER, "abd")]), None));
        assert!(!ok(&headers(&[]), None));
        assert!(check_csrf(&headers(&[(CSRF_HEADER, "abc")]), None, None).is_err());
        assert!(ok(
            &headers(&[
                (CSRF_HEADER, "abc"),
                ("origin", "https://gw.example"),
                ("host", "gw.example")
            ]),
            None
        ));
        assert!(!ok(
            &headers(&[
                (CSRF_HEADER, "abc"),
                ("origin", "https://evil.example"),
                ("host", "gw.example")
            ]),
            None
        ));
        assert!(ok(
            &headers(&[(CSRF_HEADER, "abc"), ("origin", "https://gw.example")]),
            Some("https://gw.example")
        ));
        assert!(!ok(
            &headers(&[(CSRF_HEADER, "abc"), ("origin", "http://gw.example")]),
            Some("https://gw.example")
        ));
        assert!(!ok(
            &headers(&[(CSRF_HEADER, "abc"), ("sec-fetch-site", "cross-site")]),
            None
        ));
        assert!(ok(
            &headers(&[(CSRF_HEADER, "abc"), ("sec-fetch-site", "same-origin")]),
            None
        ));
    }
}
