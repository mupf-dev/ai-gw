// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Authentifizierung der Admin-API (austauschbare Schicht).
//!
//! Die Middleware [`require_principal`] fragt einen [`AdminAuthenticator`] und legt den
//! angemeldeten [`Principal`] in die Request-Extensions; Handler prüfen über
//! [`Principal::require`] die nötige [`Permission`]. Vorläufig gibt es nur
//! [`BootstrapToken`] (§15: Bootstrap-Admin für die Erstinstallation). OIDC-Sessions und
//! Admin-API-Keys mit Rollen (AIGW-32) kommen als weitere Implementierungen hinzu, ohne die
//! Handler zu ändern.

use std::fmt::{self, Write as _};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use aigw_core::resource::Role;
use aws_lc_rs::digest;
use aws_lc_rs::rand::{SecureRandom as _, SystemRandom};
use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};

use crate::problem::Problem;

/// Mindestlänge eines vorgegebenen Bootstrap-Tokens.
pub const MIN_TOKEN_LEN: usize = 32;

/// Wie sich ein Principal angemeldet hat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    /// Bootstrap-Admin-Token.
    BootstrapToken,
}

/// Angemeldeter Akteur der Admin-API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// Kennung für das Audit-Log (`bootstrap-admin`, später `user:<name>`, `admin-key:<name>`).
    pub subject: String,
    /// Rollen (global; Einschränkung auf Org/Team folgt mit AIGW-32).
    pub roles: Vec<Role>,
    /// Anmeldeverfahren.
    pub method: AuthMethod,
}

/// Benötigte Berechtigung einer Operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    /// Lesen.
    Read,
    /// Ändern.
    Write,
}

impl Principal {
    /// Akteur im Audit-Log.
    pub fn actor(&self) -> &str {
        &self.subject
    }

    /// Prüft eine Berechtigung (vorläufig: `admin` darf alles, `viewer` lesen).
    pub fn require(&self, perm: Permission) -> Result<(), Problem> {
        let ok = self.roles.iter().any(|r| match r {
            Role::Admin => true,
            Role::Viewer | Role::Auditor => perm == Permission::Read,
            // Eingeschränkte Rollen brauchen den Org-/Team-Bezug (AIGW-32).
            Role::OrgAdmin | Role::TeamAdmin => false,
        });
        if ok {
            Ok(())
        } else {
            Err(Problem::forbidden("Rolle reicht für diese Operation nicht"))
        }
    }
}

/// Geboxte Future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Prüft die Anmeldung einer Admin-Anfrage. Asynchron, damit spätere Verfahren
/// (Session-Speicher, Token-Introspektion) Platz haben.
pub trait AdminAuthenticator: Send + Sync + 'static {
    /// Liefert den Principal oder `None` (→ 401).
    fn authenticate<'a>(&'a self, headers: &'a HeaderMap) -> BoxFuture<'a, Option<Principal>>;
}

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

fn sha256(s: &str) -> [u8; 32] {
    let d = digest::digest(&digest::SHA256, s.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    out
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
        Box::pin(async move {
            ok.then(|| Principal {
                subject: "bootstrap-admin".to_owned(),
                roles: vec![Role::Admin],
                method: AuthMethod::BootstrapToken,
            })
        })
    }
}

/// Middleware: verlangt einen angemeldeten Principal und legt ihn in die Extensions.
pub async fn require_principal(
    State(auth): State<Arc<dyn AdminAuthenticator>>,
    mut req: Request,
    next: Next,
) -> Response {
    match auth.authenticate(req.headers()).await {
        Some(p) => {
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

    fn headers(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, HeaderValue::from_str(v).unwrap());
        h
    }

    #[tokio::test]
    async fn bootstrap_token_checks_bearer() {
        let (tok, secret) = BootstrapToken::generate().unwrap();
        assert!(secret.starts_with("aigw-admin-") && secret.len() == 11 + 64);
        assert!(!format!("{tok:?}").contains(&secret));
        let p = tok
            .authenticate(&headers(&format!("Bearer {secret}")))
            .await
            .unwrap();
        assert_eq!(p.actor(), "bootstrap-admin");
        assert!(p.require(Permission::Write).is_ok());
        assert!(tok.authenticate(&headers("Bearer falsch")).await.is_none());
        assert!(tok.authenticate(&HeaderMap::new()).await.is_none());
        assert!(BootstrapToken::new("kurz").is_err());
        assert!(BootstrapToken::new(&"x".repeat(32)).is_ok());
    }

    #[test]
    fn viewer_may_only_read() {
        let p = Principal {
            subject: "v".into(),
            roles: vec![Role::Viewer],
            method: AuthMethod::BootstrapToken,
        };
        assert!(p.require(Permission::Read).is_ok());
        assert_eq!(p.require(Permission::Write).unwrap_err().status, 403);
    }
}
