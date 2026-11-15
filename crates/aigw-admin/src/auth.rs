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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

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
pub struct BootstrapGate {
    token: BootstrapToken,
    mode: BootstrapMode,
    store: Store,
    disabled: AtomicBool,
    /// Abschaltung, die (noch) nicht gespeichert werden konnte (S-29): Grund für den nächsten
    /// Versuch. Bis dahin gilt das Token in diesem Prozess schon als abgeschaltet.
    pending: Mutex<Option<String>>,
    failures: Mutex<Failures>,
    /// Fehlversuche insgesamt (seit Prozessstart).
    failures_total: AtomicU64,
}

/// Höchstzahl Fehlversuche mit dem Bootstrap-Token je [`BOOTSTRAP_FAILURE_WINDOW`] (S-29).
/// Danach lehnt das Gate bis zum Ende des Fensters **jedes** Bootstrap-Token ab (auch das
/// richtige) – das Token ist 256 Bit stark, die Sperre bremst nur Raten und macht es im Log
/// sichtbar.
pub const BOOTSTRAP_MAX_FAILURES: u32 = 10;
/// Zeitfenster der Drosselung.
pub const BOOTSTRAP_FAILURE_WINDOW: Duration = Duration::from_secs(60);
/// Präfix der Admin-API-Keys: deren Fehlversuche zählen hier nicht (eigene Prüfung).
const ADMIN_KEY_PREFIX: &str = "sk-aigw-admin-";

/// Fehlversuche im aktuellen Fenster.
#[derive(Debug, Default)]
struct Failures {
    window_start: Option<Instant>,
    count: u32,
    /// Sperre schon gemeldet (einmal je Fenster loggen).
    reported: bool,
}

impl fmt::Debug for BootstrapGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BootstrapGate")
            .field("mode", &self.mode)
            .field("disabled", &self.disabled.load(Ordering::Relaxed))
            .field(
                "failures_total",
                &self.failures_total.load(Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

impl BootstrapGate {
    /// Neues Gate.
    pub fn new(token: BootstrapToken, mode: BootstrapMode, store: Store) -> Self {
        Self {
            token,
            disabled: AtomicBool::new(mode == BootstrapMode::Off),
            mode,
            store,
            pending: Mutex::new(None),
            failures: Mutex::new(Failures::default()),
            failures_total: AtomicU64::new(0),
        }
    }

    /// Fehlversuche seit Prozessstart (S-29).
    pub fn failures_total(&self) -> u64 {
        self.failures_total.load(Ordering::Relaxed)
    }

    /// Gilt das Token derzeit (Modus und Abschaltvermerk; für `GET /auth/info`, F56)?
    pub(crate) async fn active(&self) -> bool {
        match self.mode {
            BootstrapMode::Off => false,
            BootstrapMode::On => true,
            BootstrapMode::Auto => {
                if self.disabled.load(Ordering::Relaxed) {
                    self.retry_pending().await;
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

    /// Vermerkt die dauerhafte Abschaltung (wirksam bei `admin.auth.bootstrap: auto`; erster
    /// globaler Admin, F56). S-29: Die Abschaltung gilt **sofort** in diesem Prozess, auch
    /// wenn das Speichern scheitert; dann wird sie laut geloggt (`error`) und bei jeder
    /// weiteren Prüfung erneut versucht. Beim nächsten Start holt [`reconcile_bootstrap`] sie
    /// nach (scheitert auch das, bleibt das Token aus).
    pub async fn disable(&self, reason: &str) {
        // Der Vermerk wird in jedem Modus gespeichert (ein späterer Wechsel auf `auto` sieht
        // ihn); im Prozess wirkt er nur bei `auto`.
        if self.mode == BootstrapMode::Auto {
            self.disabled.store(true, Ordering::Relaxed);
        }
        if let Err(e) = self.persist(reason).await {
            tracing::error!(
                error = %e,
                reason,
                "Bootstrap-Abschaltung nicht gespeichert – Token in diesem Prozess gesperrt, \
                 Speichern wird wiederholt (andere Instanzen akzeptieren es bis dahin noch)"
            );
            *self.pending.lock().unwrap_or_else(PoisonError::into_inner) = Some(reason.to_owned());
        }
    }

    async fn persist(&self, reason: &str) -> Result<(), aigw_store::StoreError> {
        if self.store.disable_bootstrap(reason).await? {
            tracing::info!(reason, "Bootstrap-Token (auto) dauerhaft abgeschaltet");
        }
        Ok(())
    }

    async fn retry_pending(&self) {
        let reason = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let Some(reason) = reason else { return };
        match self.persist(&reason).await {
            Ok(()) => {
                *self.pending.lock().unwrap_or_else(PoisonError::into_inner) = None;
            }
            Err(e) => {
                tracing::error!(error = %e, "Bootstrap-Abschaltung weiterhin nicht gespeichert");
            }
        }
    }

    /// Ist das Gate wegen zu vieler Fehlversuche gesperrt (zum Zeitpunkt `now`)?
    fn locked(&self, now: Instant) -> bool {
        let f = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        f.count >= BOOTSTRAP_MAX_FAILURES
            && f.window_start
                .is_some_and(|t| now.duration_since(t) < BOOTSTRAP_FAILURE_WINDOW)
    }

    /// Zählt einen Fehlversuch (S-29).
    fn record_failure(&self, now: Instant) {
        let total = self.failures_total.fetch_add(1, Ordering::Relaxed) + 1;
        let mut f = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        if f.window_start
            .is_none_or(|t| now.duration_since(t) >= BOOTSTRAP_FAILURE_WINDOW)
        {
            *f = Failures {
                window_start: Some(now),
                count: 0,
                reported: false,
            };
        }
        f.count = f.count.saturating_add(1);
        if f.count >= BOOTSTRAP_MAX_FAILURES && !f.reported {
            f.reported = true;
            tracing::warn!(
                failures = f.count,
                total,
                window_secs = BOOTSTRAP_FAILURE_WINDOW.as_secs(),
                "Bootstrap-Token: zu viele Fehlversuche – bis zum Ende des Fensters gesperrt"
            );
        } else {
            tracing::debug!(total, "Bootstrap-Token: Fehlversuch");
        }
    }

    /// Prüft ein vorgelegtes Token (ohne Modus/Abschaltung) mit Drosselung.
    fn check(&self, presented: &str, now: Instant) -> bool {
        if self.locked(now) {
            return false;
        }
        if self.token.matches(presented) {
            return true;
        }
        if !presented.starts_with(ADMIN_KEY_PREFIX) {
            self.record_failure(now);
        }
        false
    }
}

impl AdminAuthenticator for BootstrapGate {
    fn authenticate<'a>(&'a self, headers: &'a HeaderMap) -> BoxFuture<'a, Option<Principal>> {
        let ok = bearer_token(headers).is_some_and(|t| self.check(t, Instant::now()));
        Box::pin(async move {
            if ok && self.active().await {
                Some(BootstrapToken::principal())
            } else {
                None
            }
        })
    }
}

/// Schaltet das Bootstrap-Token (Modus `auto`) nach dem ersten globalen Admin ab (F56): über
/// das Gate dieser Instanz (sofort wirksam, Wiederholung bei Speicherfehler, S-29), ohne Gate
/// (Token hier nicht aktiv) direkt in der Datenbank – ein Fehler wird laut geloggt und beim
/// nächsten Start von [`reconcile_bootstrap`] nachgeholt.
pub(crate) async fn latch_bootstrap(st: &crate::AdminState, reason: &str) {
    if let Some(g) = &st.bootstrap {
        g.disable(reason).await;
        return;
    }
    match st.store.disable_bootstrap(reason).await {
        Ok(true) => tracing::info!(reason, "Bootstrap-Token (auto) dauerhaft abgeschaltet"),
        Ok(false) => {}
        Err(e) => tracing::error!(
            error = %e,
            reason,
            "Bootstrap-Abschaltung nicht gespeichert (wird beim nächsten Start nachgeholt)"
        ),
    }
}

/// Holt beim Start eine verlorene Bootstrap-Abschaltung nach (S-29): Gibt es schon einen
/// Admin-API-Key mit globaler Rolle `admin`, ist das Token (Modus `auto`) dauerhaft
/// abzuschalten. Liefert `true`, wenn es abgeschaltet ist (bzw. jetzt wurde). Ein Fehler beim
/// Lesen oder Speichern ist ein Fehler (der Aufrufer bricht den Start ab).
pub async fn reconcile_bootstrap(store: &Store) -> Result<bool, aigw_store::StoreError> {
    if store.bootstrap_disabled().await? {
        return Ok(true);
    }
    let keys = store.list_admin_api_keys().await?;
    let Some(k) = keys
        .iter()
        .find(|k| crate::rbac::is_global_admin(&k.spec.roles))
    else {
        return Ok(false);
    };
    store
        .disable_bootstrap(&format!("admin-api-key:{} (beim Start nachgeholt)", k.name))
        .await?;
    tracing::warn!(
        key = %k.name,
        "Bootstrap-Abschaltung beim Start nachgeholt (Admin-API-Key mit Rolle admin vorhanden)"
    );
    Ok(true)
}

#[cfg(test)]
mod bootstrap_tests {
    use super::*;

    async fn gate() -> BootstrapGate {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        store.migrate().await.unwrap();
        BootstrapGate::new(
            BootstrapToken::new("t0123456789-0123456789-0123456789").unwrap(),
            BootstrapMode::Auto,
            store,
        )
    }

    #[tokio::test]
    async fn failures_are_counted_and_throttled() {
        let g = gate().await;
        let good = "t0123456789-0123456789-0123456789";
        let t0 = Instant::now();
        assert!(g.check(good, t0));
        for _ in 0..BOOTSTRAP_MAX_FAILURES {
            assert!(!g.check("falsch", t0));
        }
        assert_eq!(g.failures_total(), u64::from(BOOTSTRAP_MAX_FAILURES));
        // Gesperrt – auch das richtige Token, bis das Fenster vorbei ist.
        assert!(!g.check(good, t0 + Duration::from_secs(1)));
        assert!(g.check(good, t0 + BOOTSTRAP_FAILURE_WINDOW));
        // Admin-API-Keys zählen nicht.
        let t1 = t0 + BOOTSTRAP_FAILURE_WINDOW * 3;
        for _ in 0..BOOTSTRAP_MAX_FAILURES * 2 {
            assert!(!g.check("sk-aigw-admin-abc-def", t1));
        }
        assert!(g.check(good, t1));
    }

    #[tokio::test]
    async fn disable_applies_immediately_and_is_stored() {
        let g = gate().await;
        assert!(g.active().await);
        g.disable("test").await;
        assert!(!g.active().await);
        assert!(g.store.bootstrap_disabled().await.unwrap());
    }

    #[tokio::test]
    async fn disable_fails_closed_without_database() {
        let g = gate().await;
        g.store.close().await;
        g.disable("test").await;
        // Speichern scheitert, das Token gilt hier trotzdem nicht mehr.
        assert!(!g.active().await);
        assert!(g.pending.lock().unwrap().is_some());
    }

    #[tokio::test]
    async fn reconcile_latches_when_admin_key_exists() {
        let g = gate().await;
        assert!(!reconcile_bootstrap(&g.store).await.unwrap());
        assert!(g.active().await);
        // Ein Team-Admin-Key schaltet nicht ab, ein globaler Admin-Key schon.
        let spec = |roles| aigw_store::admin_auth::AdminApiKeySpec {
            description: None,
            roles,
            expires_at: None,
        };
        g.store
            .create_admin_api_key(
                "t",
                "team",
                spec(vec![RoleBinding {
                    role: Role::TeamAdmin,
                    organization: Some("o".into()),
                    team: Some("t".into()),
                }]),
            )
            .await
            .unwrap();
        assert!(!reconcile_bootstrap(&g.store).await.unwrap());
        g.store
            .create_admin_api_key(
                "t",
                "ci",
                spec(vec![RoleBinding {
                    role: Role::Admin,
                    organization: None,
                    team: None,
                }]),
            )
            .await
            .unwrap();
        assert!(reconcile_bootstrap(&g.store).await.unwrap());
        assert!(!g.active().await);
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
