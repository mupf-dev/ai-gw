// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! OIDC-Login, Sessions und Abmeldung der Admin-UI (AIGW-32).
//!
//! Ablauf:
//! 1. `GET /admin/auth/login?returnTo=/admin/keys` – erzeugt `state`, `nonce` und
//!    PKCE-Verifier, speichert sie (Datenbank, 10 min), setzt das Login-Cookie (= `state`,
//!    `SameSite=Lax`, damit es den Rücksprung vom IdP überlebt) und leitet zum IdP weiter.
//! 2. `GET /admin/auth/callback?code&state` – prüft `state` gegen das Cookie (Schutz vor
//!    Login-CSRF), tauscht den Code (mit Verifier), prüft das ID-Token, ordnet den Nutzer zu,
//!    legt die Session an und leitet nach `returnTo` weiter. Fehler → `/admin/?auth_error=<code>`.
//! 3. `GET /admin/api/v1/session` – angemeldeter Principal samt CSRF-Token für die UI.
//! 4. `POST /admin/auth/logout` – löscht die Session; liefert die Logout-URL des IdP
//!    (RP-initiated Logout), falls angeboten.
//! 5. `POST|DELETE /admin/api/v1/users/{name}/invitation` – Einladungslink für einen vorab
//!    angelegten Nutzer (F90): einmalig, mit Ablauf; `/admin/auth/login?invite=<token>`
//!    verknüpft beim Rücksprung die OIDC-Identität (Issuer + Kennung) mit diesem Nutzer.
//!
//! Login und Logout schreiben ihren Audit-Eintrag mit `admin.auth.audit_required` (Standard)
//! in derselben Transaktion wie die Session (F113): scheitert er, scheitert die Anmeldung.
//!
//! Session-Cookie: `__Host-aigw-session=<256 Bit zufällig>; HttpOnly; Secure;
//! SameSite=Strict; Path=/` (S-23: `__Host-` = ohne `Domain`, nur vom eigenen Host setzbar –
//! keine Login-CSRF/Session-Fixierung über Schwester-Subdomains; dafür muss der Pfad `/` sein).
//! Die früheren `__Secure-`-Cookies (Pfad `/admin` bzw. `/admin/auth`) werden **nicht** mehr
//! angenommen (sie wären weiter von Subdomains setzbar) und bei Login/Logout gelöscht: bestehende
//! Sessions melden sich nach dem Update einmal neu an. Gespeichert wird nur SHA-256 der Session-ID; das CSRF-Token
//! ist SHA-256 über die Session-ID mit eigenem Kontext (aus dem Hash in der Datenbank nicht
//! ableitbar, daher keine eigene Spalte).

// `#[utoipa::path(params(…))]` erzeugt für IntoParams-Typen qualifizierte Pfade.
#![allow(unused_qualifications)]

use std::sync::Arc;

use aigw_core::config::{GroupRoleMapping, OidcConfig, SessionConfig};
use aigw_core::resource::{Object, ObjectMeta, RoleBinding, UserSpec, validate_name};
use aigw_core::snapshot::KeyHash;
use aigw_store::admin_auth::{InvitationRecord, LoginState};
use aigw_store::repo::audit::ADMIN_SESSION_KIND;
use aigw_store::{AuditEvent, Store, StoreError, hash_api_key};
use axum::extract::rejection::QueryRejection;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};
use utoipa::{IntoParams, ToSchema};

use crate::AdminState;
use crate::auth::{
    AdminAuthenticator, AuthMethod, BoxFuture, CsrfToken, Permission, Principal, check_csrf,
    random_bytes,
};
use crate::oidc::{IdClaims, OidcClient, b64url, pkce_challenge};
use crate::problem::Problem;
use crate::rbac;

/// Name des Session-Cookies (S-23: `__Host-`, `Path=/`, ohne `Domain`).
pub const SESSION_COOKIE: &str = "__Host-aigw-session";
/// Name des Cookies für einen laufenden Login.
pub const LOGIN_COOKIE: &str = "__Host-aigw-login";
/// Frühere Cookies (bis AIGW-99), werden nur noch gelöscht (`Set-Cookie` mit `Max-Age=0` und
/// dem alten Pfad).
const LEGACY_COOKIES: [(&str, &str, &str); 2] = [
    ("__Secure-aigw-session", "/admin", "Strict"),
    ("__Secure-aigw-login", "/admin/auth", "Lax"),
];
/// Gültigkeit eines begonnenen Logins.
const LOGIN_TTL_SECS: i64 = 600;
/// Aktivität wird höchstens so oft in die Datenbank geschrieben.
const TOUCH_INTERVAL_SECS: i64 = 60;
/// Audit-Akteur für Änderungen beim Login (Anlegen/Verknüpfen von Nutzern).
const LOGIN_ACTOR: &str = "oidc-login";

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

fn secs(s: u64) -> Duration {
    Duration::seconds(i64::try_from(s).unwrap_or(i64::MAX / 1000))
}

/// Zufälliger Wert (256 Bit, Base64url).
fn random_token() -> Result<String, Problem> {
    Ok(b64url(&random_bytes::<32>()?))
}

fn session_hash(sid: &str) -> KeyHash {
    hash_api_key(sid)
}

/// CSRF-Token einer Session (SHA-256 mit eigenem Kontext; nicht aus dem DB-Hash ableitbar).
pub(crate) fn csrf_for(sid: &str) -> CsrfToken {
    CsrfToken(b64url(&crate::auth::sha256(&format!(
        "aigw-csrf\u{0}{sid}"
    ))))
}

/// Liest ein Cookie aus dem `Cookie`-Header.
pub(crate) fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty())
}

fn set_cookie(res: &mut Response, value: String) {
    if let Ok(v) = HeaderValue::from_str(&value) {
        res.headers_mut().append(header::SET_COOKIE, v);
    }
}

fn session_cookie(sid: &str, max_age: u64) -> String {
    format!("{SESSION_COOKIE}={sid}; Path=/; Max-Age={max_age}; HttpOnly; Secure; SameSite=Strict")
}

fn clear_session_cookie() -> String {
    format!("{SESSION_COOKIE}=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Strict")
}

fn login_cookie(state: &str) -> String {
    format!(
        "{LOGIN_COOKIE}={state}; Path=/; Max-Age={LOGIN_TTL_SECS}; HttpOnly; Secure; SameSite=Lax"
    )
}

fn clear_login_cookie() -> String {
    format!("{LOGIN_COOKIE}=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Lax")
}

/// Löscht die früheren `__Secure-`-Cookies (S-23), falls der Browser sie mitschickt.
fn clear_legacy_cookies(res: &mut Response, headers: &HeaderMap) {
    for (name, path, same_site) in LEGACY_COOKIES {
        if cookie(headers, name).is_some() {
            set_cookie(
                res,
                format!("{name}=; Path={path}; Max-Age=0; HttpOnly; Secure; SameSite={same_site}"),
            );
        }
    }
}

fn redirect(location: &str) -> Response {
    let mut res = StatusCode::SEE_OTHER.into_response();
    if let Ok(v) = HeaderValue::from_str(location) {
        res.headers_mut().insert(header::LOCATION, v);
    }
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

/// Erlaubtes Ziel nach dem Login: nur Pfade unter `/admin/` (kein offener Redirect).
pub(crate) fn sanitize_return_to(v: Option<&str>) -> String {
    let fallback = "/admin/".to_owned();
    let Some(v) = v else { return fallback };
    let ok = (v == "/admin" || v.starts_with("/admin/"))
        && !v.contains("//")
        && !v.contains('\\')
        && !v.contains("/auth/")
        && v.len() <= 2048
        && v.chars().all(|c| !c.is_control());
    if ok { v.to_owned() } else { fallback }
}

// ---------------------------------------------------------------------------------------------
// Konfiguration und Zustand
// ---------------------------------------------------------------------------------------------

/// Einstellungen des Logins.
#[derive(Debug)]
pub struct LoginService {
    pub(crate) oidc: OidcClient,
    pub(crate) session: SessionConfig,
    pub(crate) group_roles: Vec<GroupRoleMapping>,
    pub(crate) provision_users: bool,
    pub(crate) link_by_email: bool,
    pub(crate) end_session: bool,
    pub(crate) post_logout_redirect: String,
}

/// Ursprung (`scheme://host[:port]`) einer URL.
pub(crate) fn origin_of(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    let origin = u.origin();
    origin.is_tuple().then(|| origin.ascii_serialization())
}

impl LoginService {
    /// Baut den Login aus der Konfiguration.
    pub fn new(
        oidc: &OidcConfig,
        session: &SessionConfig,
        client_secret: Option<String>,
    ) -> Result<Self, String> {
        let client = OidcClient::new(oidc, client_secret).map_err(|e| e.to_string())?;
        let post_logout_redirect = match &oidc.post_logout_redirect_url {
            Some(u) => u.clone(),
            None => format!(
                "{}/admin/",
                origin_of(client.redirect_url()).unwrap_or_default()
            ),
        };
        for m in &oidc.group_roles {
            let b = RoleBinding {
                role: m.role,
                organization: m.organization.clone(),
                team: m.team.clone(),
            };
            if !rbac::is_effective(&b) {
                return Err(format!(
                    "admin.auth.oidc.group_roles: Gruppe {:?}: Rolle {:?} braucht organization bzw. team",
                    m.group, m.role
                ));
            }
        }
        if session.max_age_secs == 0 || session.idle_timeout_secs == 0 {
            return Err("admin.auth.session: Zeiten müssen > 0 sein".into());
        }
        Ok(Self {
            oidc: client,
            session: session.clone(),
            group_roles: oidc.group_roles.clone(),
            provision_users: oidc.provision_users,
            link_by_email: oidc.link_by_email,
            end_session: oidc.end_session,
            post_logout_redirect,
        })
    }
}

/// Prüft Session-Cookies.
#[derive(Debug)]
pub struct SessionAuthenticator {
    store: Store,
    session: SessionConfig,
    group_roles: Vec<GroupRoleMapping>,
}

impl SessionAuthenticator {
    /// Neuer Prüfer.
    pub fn new(store: Store, session: SessionConfig, group_roles: Vec<GroupRoleMapping>) -> Self {
        Self {
            store,
            session,
            group_roles,
        }
    }

    async fn check(&self, sid: &str) -> Result<Option<Principal>, StoreError> {
        let hash = session_hash(sid);
        let Some(rec) = self.store.get_admin_session(&hash).await? else {
            return Ok(None);
        };
        let t = now();
        if rec.expires_at <= t || rec.last_seen_at + secs(self.session.idle_timeout_secs) < t {
            self.store.delete_admin_session(&hash).await?;
            return Ok(None);
        }
        let user: Object<UserSpec> = match self.store.get_by_id(rec.user_id).await {
            Ok(u) => u,
            Err(StoreError::NotFound { .. }) => {
                self.store.delete_admin_session(&hash).await?;
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        if t - rec.last_seen_at >= Duration::seconds(TOUCH_INTERVAL_SECS) {
            self.store.touch_admin_session(&hash, t).await?;
        }
        let roles = effective_roles(&user.spec, &rec.groups, &self.group_roles);
        Ok(Some(Principal {
            subject: format!("user:{}", user.metadata.name),
            roles,
            method: AuthMethod::Session,
            display_name: user.spec.display_name.clone(),
            email: user.spec.email.clone(),
            expires_at: Some(rec.expires_at),
            csrf: Some(csrf_for(sid)),
        }))
    }
}

/// Wirksame Rollen: direkt zugewiesen ∪ aus Gruppen (nur wirksame Zuweisungen).
fn effective_roles(
    user: &UserSpec,
    groups: &[String],
    mappings: &[GroupRoleMapping],
) -> Vec<RoleBinding> {
    rbac::merge(
        user.roles.clone(),
        rbac::roles_from_groups(groups, mappings),
    )
    .into_iter()
    .filter(rbac::is_effective)
    .collect()
}

impl AdminAuthenticator for SessionAuthenticator {
    fn authenticate<'a>(&'a self, headers: &'a HeaderMap) -> BoxFuture<'a, Option<Principal>> {
        Box::pin(async move {
            let sid = cookie(headers, SESSION_COOKIE)?;
            match self.check(sid).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "Session-Prüfung fehlgeschlagen");
                    None
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Login
// ---------------------------------------------------------------------------------------------

/// Parameter von `/admin/auth/login`.
#[derive(Debug, Deserialize, IntoParams)]
#[serde(rename_all = "camelCase")]
#[into_params(parameter_in = Query)]
pub(crate) struct LoginQuery {
    /// Ziel nach dem Login (Pfad unter `/admin/`).
    return_to: Option<String>,
    /// Einladungs-Token (F90, aus dem Einladungslink).
    invite: Option<String>,
}

/// Startet den OIDC-Login (Weiterleitung zum IdP).
#[utoipa::path(
    get, path = "/admin/auth/login", tag = "auth", params(LoginQuery),
    responses(
        (status = 303, description = "Weiterleitung zum IdP (setzt das Login-Cookie)"),
        (status = 404, description = "OIDC nicht konfiguriert", body = Problem, content_type = "application/problem+json"),
        (status = 502, description = "IdP nicht erreichbar", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn login(
    State(st): State<AdminState>,
    headers: HeaderMap,
    q: Result<Query<LoginQuery>, QueryRejection>,
) -> Result<Response, Problem> {
    let Some(svc) = st.login.as_ref() else {
        return Err(Problem::not_found("OIDC-Login ist nicht konfiguriert"));
    };
    let Query(q) = q?;
    let return_to = sanitize_return_to(q.return_to.as_deref());
    // Einladung (F90): früh prüfen (verständlicher Hinweis), eingelöst erst beim Rücksprung.
    let invite_hash = match q.invite.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) if t.len() <= 128 => {
            let h = session_hash(t);
            if !st.store.invitation_valid(&h, now()).await? {
                tracing::info!("Login mit ungültiger oder abgelaufener Einladung");
                return Ok(redirect("/admin/?auth_error=invite"));
            }
            Some(h)
        }
        Some(_) => return Ok(redirect("/admin/?auth_error=invite")),
        None => None,
    };
    let state = random_token()?;
    let nonce = random_token()?;
    let verifier = random_token()?;
    let url = svc
        .oidc
        .authorization_url(&state, &nonce, &pkce_challenge(&verifier))
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "OIDC: Discovery fehlgeschlagen");
            Problem::new(
                StatusCode::BAD_GATEWAY,
                "idp-unavailable",
                "Identitätsanbieter nicht erreichbar",
                "Der Identitätsanbieter ist nicht erreichbar – Details im Server-Log",
            )
        })?;
    st.store
        .put_login_state(
            &session_hash(&state),
            &LoginState {
                nonce,
                code_verifier: verifier,
                return_to,
                created_at: now(),
                invite_hash,
            },
        )
        .await?;
    let mut res = redirect(&url);
    set_cookie(&mut res, login_cookie(&state));
    clear_legacy_cookies(&mut res, &headers);
    Ok(res)
}

/// Parameter des Rücksprungs vom IdP.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct CallbackQuery {
    /// Autorisierungscode.
    code: Option<String>,
    /// `state` aus dem Login.
    state: Option<String>,
    /// Fehlercode des IdP.
    error: Option<String>,
    /// Fehlerbeschreibung des IdP.
    error_description: Option<String>,
}

/// Fehler im Login mit Code für die UI (`/admin/?auth_error=<code>`).
struct LoginFailure {
    code: &'static str,
    log: String,
    /// Im Audit-Log festhalten (nur nach gültigem `state`, d. h. für einen echten, in diesem
    /// Browser begonnenen Login – sonst könnte jeder ohne Anmeldung das Audit-Log füllen).
    audit: bool,
    /// Zugeordneter Nutzer (falls schon bekannt, z. B. bei `no-access`).
    user: Option<String>,
}

fn fail(code: &'static str, log: impl Into<String>) -> LoginFailure {
    LoginFailure {
        code,
        log: log.into(),
        audit: false,
        user: None,
    }
}

/// Höchstlänge des Grundes in Audit-Ereignissen fehlgeschlagener Logins.
const AUDIT_REASON_MAX: usize = 300;

/// Schreibt ein Anmelde-Ereignis; ein Fehler beim Schreiben verhindert die Anmeldung nicht.
async fn audit_event(store: &Store, ev: AuditEvent) {
    if let Err(e) = store.record_audit_event(&ev).await {
        tracing::warn!(error = %e, action = %ev.action, "Audit-Ereignis nicht gespeichert");
    }
}

impl From<StoreError> for LoginFailure {
    fn from(e: StoreError) -> Self {
        fail("internal", e.to_string())
    }
}

impl From<Problem> for LoginFailure {
    fn from(p: Problem) -> Self {
        fail("internal", p.detail.unwrap_or_default())
    }
}

/// Rücksprung vom IdP: Code tauschen, ID-Token prüfen, Session anlegen.
#[utoipa::path(
    get, path = "/admin/auth/callback", tag = "auth", params(CallbackQuery),
    responses(
        (status = 303, description = "Angemeldet (setzt das Session-Cookie) und weiter zu `returnTo`; bei Fehlern weiter zu `/admin/?auth_error=<code>` (`idp`, `state`, `token`, `invite`, `no-access`, `internal`)"),
    )
)]
pub(crate) async fn callback(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> Response {
    let mut res = match finish_login(&st, &headers, q).await {
        Ok((sid, return_to, max_age)) => {
            let mut res = redirect(&return_to);
            set_cookie(&mut res, session_cookie(&sid, max_age));
            res
        }
        Err(f) => {
            tracing::warn!(reason = %f.log, code = f.code, "OIDC-Login abgelehnt");
            if f.audit {
                let reason: String = f.log.chars().take(AUDIT_REASON_MAX).collect();
                audit_event(
                    &st.store,
                    AuditEvent {
                        actor: LOGIN_ACTOR.into(),
                        action: "login-failed".into(),
                        kind: ADMIN_SESSION_KIND.into(),
                        object_id: None,
                        object_name: f.user.clone(),
                        details: Some(serde_json::json!({"code": f.code, "reason": reason})),
                    },
                )
                .await;
            }
            redirect(&format!("/admin/?auth_error={}", f.code))
        }
    };
    set_cookie(&mut res, clear_login_cookie());
    clear_legacy_cookies(&mut res, &headers);
    res
}

async fn finish_login(
    st: &AdminState,
    headers: &HeaderMap,
    q: CallbackQuery,
) -> Result<(String, String, u64), LoginFailure> {
    let svc = st
        .login
        .as_ref()
        .ok_or_else(|| fail("internal", "OIDC nicht konfiguriert"))?;
    if let Some(e) = q.error {
        return Err(fail(
            "idp",
            format!(
                "IdP meldet {e}: {}",
                q.error_description.unwrap_or_default()
            ),
        ));
    }
    let (Some(code), Some(state)) = (q.code, q.state) else {
        return Err(fail("state", "code oder state fehlt"));
    };
    // state muss zum Cookie dieses Browsers passen (Login-CSRF).
    match cookie(headers, LOGIN_COOKIE) {
        Some(c) if crate::auth::secure_eq(c, &state) => {}
        _ => {
            return Err(fail(
                "state",
                "Login-Cookie fehlt oder passt nicht zu state",
            ));
        }
    }
    let login = st
        .store
        .take_login_state(&session_hash(&state))
        .await?
        .ok_or_else(|| fail("state", "unbekannter oder bereits verwendeter state"))?;
    complete_login(st, svc, &code, login)
        .await
        .map_err(|f| LoginFailure { audit: true, ..f })
}

/// Zweiter Teil des Rücksprungs (nach gültigem `state`): Token, Nutzer, Session.
async fn complete_login(
    st: &AdminState,
    svc: &LoginService,
    code: &str,
    login: LoginState,
) -> Result<(String, String, u64), LoginFailure> {
    if login.created_at + Duration::seconds(LOGIN_TTL_SECS) < now() {
        return Err(fail("state", "Login abgelaufen"));
    }
    let id_token = svc
        .oidc
        .exchange_code(code, &login.code_verifier)
        .await
        .map_err(|e| fail("token", e.to_string()))?;
    let claims = svc
        .oidc
        .verify_id_token(&id_token, &login.nonce)
        .await
        .map_err(|e| fail("token", e.to_string()))?;

    let group_roles = rbac::roles_from_groups(&claims.groups, &svc.group_roles);
    let user = resolve_user(
        st,
        svc,
        &claims,
        !group_roles.is_empty(),
        login.invite_hash.as_ref(),
    )
    .await?;
    let roles = effective_roles(&user.spec, &claims.groups, &svc.group_roles);
    if roles.is_empty() {
        return Err(LoginFailure {
            user: Some(user.metadata.name.clone()),
            ..fail(
                "no-access",
                format!(
                    "Nutzer {} (sub {}) hat keine Rolle (Gruppen {:?})",
                    user.metadata.name, claims.subject, claims.groups
                ),
            )
        });
    }
    if rbac::is_global_admin(&roles) {
        crate::auth::latch_bootstrap(st, &format!("oidc-admin:{}", user.metadata.name)).await;
    }

    let sid = random_token()?;
    let t = now();
    let expires = t + secs(svc.session.max_age_secs);
    st.store
        .purge_admin_auth(
            t,
            t - secs(svc.session.idle_timeout_secs),
            t - Duration::seconds(LOGIN_TTL_SECS),
        )
        .await?;
    let event = AuditEvent {
        actor: format!("user:{}", user.metadata.name),
        action: "login".into(),
        kind: ADMIN_SESSION_KIND.into(),
        object_id: Some(user.id),
        object_name: Some(user.metadata.name.clone()),
        details: Some(serde_json::json!({
            "method": "oidc",
            "groups": claims.groups,
            "roles": roles,
            "expiresAt": expires.format(&time::format_description::well_known::Rfc3339).ok(),
        })),
    };
    if st.auth_opts.audit_required {
        // F113: Session und Audit-Eintrag in einer Transaktion – ohne Eintrag kein Login.
        st.store
            .create_admin_session_audited(
                &session_hash(&sid),
                user.id,
                &claims.groups,
                expires,
                &event,
            )
            .await
            .map_err(|e| fail("internal", format!("Audit-Eintrag des Logins: {e}")))?;
    } else {
        st.store
            .create_admin_session(&session_hash(&sid), user.id, &claims.groups, expires)
            .await?;
        audit_event(&st.store, event).await;
    }
    tracing::info!(user = %user.metadata.name, "Admin-Login per OIDC");
    Ok((sid, login.return_to, svc.session.max_age_secs))
}

/// Ordnet das ID-Token einem Nutzer zu: über (Issuer, Subjekt), sonst über die bestätigte
/// E-Mail eines noch nicht verknüpften Nutzers, sonst Neuanlage (nur mit Gruppenrolle).
async fn resolve_user(
    st: &AdminState,
    svc: &LoginService,
    c: &IdClaims,
    has_group_role: bool,
    invite: Option<&KeyHash>,
) -> Result<Object<UserSpec>, LoginFailure> {
    let users = st.store.list::<UserSpec>().await?;
    let linked = users.iter().find(|u| {
        u.spec.oidc_issuer.as_deref() == Some(c.issuer.as_str())
            && u.spec.oidc_subject.as_deref() == Some(c.subject.as_str())
    });
    // Einladung (F90): einmalig einlösen – verknüpft die Identität mit dem eingeladenen Nutzer.
    if let Some(hash) = invite {
        let invited = st
            .store
            .take_invitation(hash, now())
            .await?
            .ok_or_else(|| fail("invite", "Einladung ungültig, abgelaufen oder verwendet"))?;
        if let Some(u) = linked {
            return if u.id == invited {
                Ok(u.clone())
            } else {
                Err(fail(
                    "invite",
                    format!(
                        "Identität (iss {}, Kennung {}) gehört bereits zu Nutzer {}",
                        c.issuer, c.subject, u.metadata.name
                    ),
                ))
            };
        }
        let Some(u) = users.iter().find(|u| u.id == invited) else {
            return Err(fail("invite", "eingeladener Nutzer existiert nicht mehr"));
        };
        if u.spec.oidc_subject.is_some() {
            return Err(fail(
                "invite",
                format!("Nutzer {} ist bereits verknüpft", u.metadata.name),
            ));
        }
        let mut spec = u.spec.clone();
        spec.oidc_issuer = Some(c.issuer.clone());
        spec.oidc_subject = Some(c.subject.clone());
        if spec.display_name.is_none() {
            spec.display_name.clone_from(&c.name);
        }
        if spec.email.is_none() && c.email_verified {
            spec.email.clone_from(&c.email);
        }
        let linked = st
            .store
            .update(LOGIN_ACTOR, u.metadata.clone(), spec)
            .await?;
        tracing::info!(user = %linked.metadata.name, "OIDC-Konto über Einladung verknüpft");
        return Ok(linked);
    }
    if let Some(u) = linked {
        return Ok(u.clone());
    }
    if svc.link_by_email
        && c.email_verified
        && let Some(email) = c.email.as_deref()
    {
        let matches: Vec<&Object<UserSpec>> = users
            .iter()
            .filter(|u| {
                u.spec.oidc_subject.is_none()
                    && u.spec
                        .email
                        .as_deref()
                        .is_some_and(|e| e.eq_ignore_ascii_case(email))
            })
            .collect();
        if let [u] = matches.as_slice() {
            let mut spec = u.spec.clone();
            spec.oidc_issuer = Some(c.issuer.clone());
            spec.oidc_subject = Some(c.subject.clone());
            if spec.display_name.is_none() {
                spec.display_name.clone_from(&c.name);
            }
            let linked = st
                .store
                .update(LOGIN_ACTOR, u.metadata.clone(), spec)
                .await?;
            tracing::info!(user = %linked.metadata.name, "OIDC-Konto über E-Mail verknüpft");
            return Ok(linked);
        }
    }
    if !(svc.provision_users && has_group_role) {
        return Err(fail(
            "no-access",
            format!(
                "unbekannter Nutzer (iss {}, sub {}) ohne Gruppenrolle bzw. Anlage abgeschaltet",
                c.issuer, c.subject
            ),
        ));
    }
    let base = user_name_for(c);
    let taken = |n: &str| users.iter().any(|u| u.metadata.name == n);
    let name = if taken(&base) {
        let suffix: String = KeyHash(crate::auth::sha256(&format!(
            "{}\u{0}{}",
            c.issuer, c.subject
        )))
        .to_hex()
        .chars()
        .take(8)
        .collect();
        let keep = base.len().min(128 - 9);
        format!("{}-{suffix}", &base[..keep])
    } else {
        base
    };
    let spec = UserSpec {
        oidc_issuer: Some(c.issuer.clone()),
        oidc_subject: Some(c.subject.clone()),
        email: c.email.clone(),
        display_name: c.name.clone(),
        roles: Vec::new(),
    };
    let created = st
        .store
        .create(LOGIN_ACTOR, ObjectMeta::named(name), spec)
        .await?;
    tracing::info!(user = %created.metadata.name, "Nutzer beim ersten OIDC-Login angelegt");
    Ok(created)
}

/// Ressourcenname für einen neuen Nutzer: `preferred_username`, sonst lokaler Teil der E-Mail,
/// sonst `user-<Hash>`; auf erlaubte Zeichen abgebildet.
fn user_name_for(c: &IdClaims) -> String {
    let raw = c
        .preferred_username
        .clone()
        .or_else(|| {
            c.email
                .as_deref()
                .and_then(|e| e.split('@').next())
                .map(str::to_owned)
        })
        .unwrap_or_default();
    let mut s: String = raw
        .to_ascii_lowercase()
        .chars()
        .map(|ch| {
            if ch.is_ascii_lowercase() || ch.is_ascii_digit() || "-._".contains(ch) {
                ch
            } else {
                '-'
            }
        })
        .collect();
    while s.starts_with(|ch: char| !ch.is_ascii_alphanumeric()) {
        s.remove(0);
    }
    s.truncate(100);
    if validate_name(&s).is_ok() {
        s
    } else {
        let h = KeyHash(crate::auth::sha256(&c.subject)).to_hex();
        format!("user-{}", &h[..12])
    }
}

// ---------------------------------------------------------------------------------------------
// Abmeldung, Session-Info, Anmeldeverfahren
// ---------------------------------------------------------------------------------------------

/// Ergebnis der Abmeldung.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LogoutResult {
    /// Hierhin navigieren (Logout beim IdP oder die Admin-UI).
    pub redirect_url: String,
}

/// Meldet die Session ab (lokal und – falls angeboten – beim IdP).
#[utoipa::path(
    post, path = "/admin/auth/logout", tag = "auth",
    params(("X-CSRF-Token" = Option<String>, Header, description = "CSRF-Token der Session (Pflicht mit Session-Cookie)")),
    responses(
        (status = 200, description = "Abgemeldet; Session-Cookie gelöscht", body = LogoutResult),
        (status = 403, description = "CSRF-Prüfung fehlgeschlagen", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = []))
)]
pub(crate) async fn logout(
    State(st): State<AdminState>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    if let Some(sid) = cookie(&headers, SESSION_COOKIE) {
        check_csrf(&headers, Some(&csrf_for(sid)), st.allowed_origin.as_deref())?;
        let hash = session_hash(sid);
        let session = st.store.get_admin_session(&hash).await?;
        match session {
            Some(rec) => {
                let user = st.store.get_by_id::<UserSpec>(rec.user_id).await.ok();
                let name = user.map(|u| u.metadata.name);
                let event = AuditEvent {
                    actor: name
                        .as_ref()
                        .map_or_else(|| "user:?".to_owned(), |n| format!("user:{n}")),
                    action: "logout".into(),
                    kind: ADMIN_SESSION_KIND.into(),
                    object_id: Some(rec.user_id),
                    object_name: name,
                    details: None,
                };
                if st.auth_opts.audit_required {
                    // F113: ohne Audit-Eintrag bleibt die Session bestehen (500).
                    st.store.delete_admin_session_audited(&hash, &event).await?;
                } else {
                    st.store.delete_admin_session(&hash).await?;
                    audit_event(&st.store, event).await;
                }
            }
            None => st.store.delete_admin_session(&hash).await?,
        }
    }
    let mut redirect_url = "/admin/".to_owned();
    if let Some(svc) = st.login.as_ref()
        && svc.end_session
        && let Some(u) = svc.oidc.end_session_url(&svc.post_logout_redirect).await
    {
        redirect_url = u;
    }
    let mut res = Json(LogoutResult { redirect_url }).into_response();
    set_cookie(&mut res, clear_session_cookie());
    clear_legacy_cookies(&mut res, &headers);
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(res)
}

/// Angemeldeter Principal.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    /// Kennung im Audit-Log (`user:<name>`, `admin-key:<name>`, `bootstrap-admin`).
    pub subject: String,
    /// Verfahren: `session`, `admin-api-key`, `bootstrap-token`.
    pub method: String,
    /// Anzeigename.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// E-Mail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Wirksame Rollen.
    pub roles: Vec<RoleBinding>,
    /// Ablauf der Anmeldung.
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub expires_at: Option<OffsetDateTime>,
    /// CSRF-Token für schreibende Anfragen (Header `X-CSRF-Token`; nur bei Sessions).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub csrf_token: Option<String>,
}

/// Liefert den angemeldeten Principal (Rollen, CSRF-Token).
#[utoipa::path(
    get, path = "/admin/api/v1/session", tag = "auth",
    responses(
        (status = 200, description = "Angemeldet", body = SessionInfo),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub(crate) async fn session_info(Extension(p): Extension<Principal>) -> Response {
    let mut res = Json(SessionInfo::of(&p)).into_response();
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

impl SessionInfo {
    fn of(p: &Principal) -> Self {
        Self {
            subject: p.subject.clone(),
            method: p.method.as_str().to_owned(),
            display_name: p.display_name.clone(),
            email: p.email.clone(),
            roles: p.roles.clone(),
            expires_at: p.expires_at,
            csrf_token: p.csrf.as_ref().map(|c| c.expose().to_owned()),
        }
    }
}

/// Anmeldeverfahren und – falls angemeldet – der Principal (ohne Anmeldung abrufbar, immer
/// `200`; so prüft die UI beim Start ohne `401` in der Browser-Konsole).
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AuthInfo {
    /// OIDC-Login konfiguriert.
    pub oidc: bool,
    /// Start des Logins (falls OIDC konfiguriert).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login_url: Option<String>,
    /// Angemeldeter Principal (Session-Cookie oder Bearer), sonst leer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionInfo>,
    /// Modus „nur YAML“ (`gitops.read_only_admin`, F56): Schreibrouten antworten mit `403`,
    /// Änderungen nur per GitOps – die UI blendet Schreibaktionen aus.
    pub read_only: bool,
    /// Das Bootstrap-Token ist aktiv (F56) – die UI zeigt einen Warnhinweis (Notfallzugang
    /// abschalten: OIDC-Admin anmelden bzw. Admin-API-Key mit Rolle `admin` anlegen).
    pub bootstrap: bool,
    /// Mock-Modus der UI anbieten (`admin.ui.mock`, F135): `false` blendet den Schalter aus.
    pub ui_mock: bool,
    /// Höchstlaufzeit von Admin-API-Keys in Sekunden (`admin.auth.max_admin_key_lifetime`, F87),
    /// nur für Angemeldete: die UI markiert ältere Keys, die länger gelten (F201).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_admin_key_lifetime_secs: Option<u64>,
    /// Link-Vorlage zum Tracing-System (`admin.ui.trace_url`, `{traceId}` wird ersetzt), nur für
    /// Angemeldete.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_url: Option<String>,
    /// Namen der vertrauenswürdigen JWT-Aussteller der Datenebene (`auth.jwt.issuers`,
    /// AIGW-115) – Auswahl für Workload-Identitäten am Key; nur für Angemeldete.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workload_issuers: Vec<String>,
    /// Abrechnungswährung (`billing.currency`, ISO 4217, AIGW-117) für die Geldformate der UI.
    pub currency: String,
}

/// Liefert die Anmeldeverfahren und den angemeldeten Principal (für die UI).
#[utoipa::path(
    get, path = "/admin/api/v1/auth/info", tag = "auth",
    responses((status = 200, description = "Anmeldeverfahren und ggf. Principal", body = AuthInfo))
)]
pub(crate) async fn auth_info(State(st): State<AdminState>, headers: HeaderMap) -> Response {
    let oidc = st.login.is_some();
    let session = st
        .auth
        .authenticate(&headers)
        .await
        .map(|p| SessionInfo::of(&p));
    let bootstrap = match &st.bootstrap {
        Some(g) => g.active().await,
        None => false,
    };
    let max_admin_key_lifetime_secs = session.as_ref().and(st.auth_opts.max_admin_key_lifetime);
    let trace_url = session.as_ref().and(st.trace_url.clone());
    let workload_issuers = if session.is_some() {
        st.workload_issuers.to_vec()
    } else {
        Vec::new()
    };
    let mut res = Json(AuthInfo {
        oidc,
        login_url: oidc.then(|| "/admin/auth/login".to_owned()),
        session,
        read_only: st.read_only,
        bootstrap,
        ui_mock: st.ui_mock,
        max_admin_key_lifetime_secs,
        trace_url,
        workload_issuers,
        currency: st.currency.to_string(),
    })
    .into_response();
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

/// Öffentliche Routen (ohne Anmeldung).
pub(crate) fn public_router() -> Router<AdminState> {
    Router::new()
        .route("/admin/auth/login", get(login))
        .route("/admin/auth/callback", get(callback))
        .route("/admin/auth/logout", post(logout))
        .route("/admin/api/v1/auth/info", get(auth_info))
}

/// Routen mit Anmeldung.
pub(crate) fn protected_router() -> Router<AdminState> {
    Router::new()
        .route("/admin/api/v1/session", get(session_info))
        .route(
            "/admin/api/v1/users/{name}/invitation",
            get(get_invitation)
                .post(create_invitation)
                .delete(delete_invitation),
        )
}

// ---------------------------------------------------------------------------------------------
// Einladungen (F90)
// ---------------------------------------------------------------------------------------------

/// Einladung eines Nutzers (ohne Token).
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Invitation {
    /// Nutzer (Name).
    pub user: String,
    /// Erstellt von (Audit-Akteur).
    pub created_by: String,
    /// Erstellt.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
    /// Gültig bis.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub expires_at: OffsetDateTime,
    /// Abgelaufen (nicht mehr einlösbar).
    pub expired: bool,
}

/// Neu erstellte Einladung samt Link (nur in dieser Antwort).
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreatedInvitation {
    /// Einladung.
    pub invitation: Invitation,
    /// Einladungslink (einmalig, bis `expiresAt`): an den Nutzer weitergeben.
    pub url: String,
}

impl Invitation {
    fn of(user: &str, r: InvitationRecord) -> Self {
        Self {
            user: user.to_owned(),
            created_by: r.created_by,
            created_at: r.created_at,
            expired: r.expires_at <= now(),
            expires_at: r.expires_at,
        }
    }
}

async fn invited_user(
    st: &AdminState,
    p: &Principal,
    name: &str,
) -> Result<Object<UserSpec>, Problem> {
    // Nutzerverwaltung ist global (wie `/users`).
    p.require(Permission::Write)?;
    Ok(st.store.get::<UserSpec>(name).await?)
}

/// Erstellt einen Einladungslink für einen vorab angelegten Nutzer (F90): einmalig, gültig
/// `admin.auth.oidc.invite_ttl_secs`; ersetzt eine frühere Einladung. Beim Login über den Link
/// wird die OIDC-Identität (Issuer + Kennung) mit dem Nutzer verknüpft.
#[utoipa::path(
    post, path = "/admin/api/v1/users/{name}/invitation", tag = "users",
    params(("name" = String, Path, description = "Name des Nutzers")),
    responses(
        (status = 201, description = "Einladung erstellt (Link nur in dieser Antwort)", body = CreatedInvitation),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nutzer nicht gefunden", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "OIDC nicht konfiguriert oder Nutzer bereits verknüpft", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn create_invitation(
    State(st): State<AdminState>,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
) -> Result<Response, Problem> {
    let user = invited_user(&st, &p, &name).await?;
    let Some(svc) = st.login.as_ref() else {
        return Err(Problem::conflict(
            "Einladungen brauchen OIDC-Login (admin.auth.oidc)",
        ));
    };
    if user.spec.oidc_subject.is_some() {
        return Err(Problem::conflict(format!(
            "Nutzer {name:?} ist bereits mit einer OIDC-Identität verknüpft"
        )));
    }
    let token = random_token()?;
    let expires = now() + secs(st.auth_opts.invite_ttl_secs);
    let rec = st
        .store
        .create_invitation(p.actor(), user.id, &name, &session_hash(&token), expires)
        .await?;
    let origin = origin_of(svc.oidc.redirect_url()).unwrap_or_default();
    let body = CreatedInvitation {
        invitation: Invitation::of(&name, rec),
        url: format!("{origin}/admin/auth/login?invite={token}"),
    };
    let mut res = (StatusCode::CREATED, Json(body)).into_response();
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(res)
}

/// Zeigt die offene Einladung eines Nutzers (ohne Link).
#[utoipa::path(
    get, path = "/admin/api/v1/users/{name}/invitation", tag = "users",
    params(("name" = String, Path, description = "Name des Nutzers")),
    responses(
        (status = 200, description = "Offene Einladung", body = Invitation),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nutzer oder Einladung nicht gefunden", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn get_invitation(
    State(st): State<AdminState>,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
) -> Result<Response, Problem> {
    p.require(Permission::Read)?;
    let user = st.store.get::<UserSpec>(&name).await?;
    let rec = st
        .store
        .get_invitation(user.id)
        .await?
        .ok_or_else(|| Problem::not_found(format!("keine Einladung für {name:?}")))?;
    Ok(Json(Invitation::of(&name, rec)).into_response())
}

/// Widerruft die offene Einladung eines Nutzers.
#[utoipa::path(
    delete, path = "/admin/api/v1/users/{name}/invitation", tag = "users",
    params(("name" = String, Path, description = "Name des Nutzers")),
    responses(
        (status = 204, description = "Widerrufen"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Nutzer oder Einladung nicht gefunden", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn delete_invitation(
    State(st): State<AdminState>,
    Extension(p): Extension<Principal>,
    Path(name): Path<String>,
) -> Result<Response, Problem> {
    let user = invited_user(&st, &p, &name).await?;
    if st
        .store
        .delete_invitation(p.actor(), user.id, &name)
        .await?
    {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Err(Problem::not_found(format!("keine Einladung für {name:?}")))
    }
}

/// Liest das Client-Secret aus Datei bzw. Umgebung.
pub fn client_secret(cfg: &OidcConfig) -> Result<Option<String>, String> {
    if let Some(path) = &cfg.client_secret_file {
        let s = std::fs::read_to_string(path)
            .map_err(|e| format!("client_secret_file {}: {e}", path.display()))?;
        return Ok(Some(s.trim().to_owned()).filter(|s| !s.is_empty()));
    }
    Ok(std::env::var(&cfg.client_secret_env)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty()))
}

/// Gemeinsamer Login-Dienst (für [`AdminState`]).
pub type SharedLogin = Arc<LoginService>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn return_to_is_restricted() {
        assert_eq!(sanitize_return_to(None), "/admin/");
        assert_eq!(
            sanitize_return_to(Some("/admin/keys?x=1")),
            "/admin/keys?x=1"
        );
        for bad in [
            "https://evil.example/admin/",
            "//evil.example/admin/",
            "/admin//evil",
            "/admin/\\evil",
            "/other",
            "/admin/auth/login",
            "/admin/\nx",
        ] {
            assert_eq!(sanitize_return_to(Some(bad)), "/admin/", "{bad:?}");
        }
    }

    #[test]
    fn cookies_parse_and_format() {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; __Host-aigw-session=abc ; b=2"),
        );
        assert_eq!(cookie(&h, SESSION_COOKIE), Some("abc"));
        assert_eq!(cookie(&h, LOGIN_COOKIE), None);
        // S-23: alte `__Secure-`-Cookies gelten nicht mehr.
        let mut old = HeaderMap::new();
        old.insert(
            header::COOKIE,
            HeaderValue::from_static("__Secure-aigw-session=abc"),
        );
        assert_eq!(cookie(&old, SESSION_COOKIE), None);
        let c = session_cookie("x", 10);
        for part in [
            "__Host-aigw-session=x;",
            "HttpOnly",
            "Secure",
            "SameSite=Strict",
            "Path=/;",
            "Max-Age=10",
        ] {
            assert!(c.contains(part), "{c}");
        }
        assert!(!c.contains("Domain"), "{c}");
        let l = login_cookie("s");
        assert!(
            l.starts_with("__Host-aigw-login=s;") && l.contains("Path=/;"),
            "{l}"
        );
        assert!(l.contains("SameSite=Lax"));
        let mut res = StatusCode::OK.into_response();
        clear_legacy_cookies(&mut res, &old);
        let set: Vec<_> = res
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            set,
            ["__Secure-aigw-session=; Path=/admin; Max-Age=0; HttpOnly; Secure; SameSite=Strict"]
        );
        assert_ne!(csrf_for("a").0, csrf_for("b").0);
        assert_ne!(csrf_for("a").0, session_hash("a").to_hex());
    }

    #[test]
    fn user_names() {
        let c = |pu: Option<&str>, email: Option<&str>| IdClaims {
            issuer: "i".into(),
            subject: "s".into(),
            email: email.map(Into::into),
            email_verified: true,
            name: None,
            preferred_username: pu.map(Into::into),
            groups: vec![],
        };
        assert_eq!(user_name_for(&c(Some("Anna.Becker"), None)), "anna.becker");
        assert_eq!(user_name_for(&c(None, Some("Bob+x@ex.org"))), "bob-x");
        assert!(user_name_for(&c(Some("---"), None)).starts_with("user-"));
        assert!(user_name_for(&c(None, None)).starts_with("user-"));
        assert_eq!(
            origin_of("https://gw.example:8443/admin/auth/callback").as_deref(),
            Some("https://gw.example:8443")
        );
    }
}
