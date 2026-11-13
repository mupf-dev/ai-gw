// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Google-Service-Account → OAuth-Access-Token (RFC 7523, JWT-Bearer-Grant), ohne Google-SDK.
//!
//! 1. JWT bauen: Header `{"alg":"RS256","typ":"JWT","kid":<private_key_id>}`, Claims
//!    `iss` = `client_email`, `scope`, `aud` = Token-URL, `iat`, `exp` = `iat` + 3600.
//! 2. Signieren mit RSASSA-PKCS1-v1_5/SHA-256 (aws-lc-rs, Schlüssel PKCS#8 aus dem PEM der
//!    Schlüsseldatei).
//! 3. `POST` an die Token-URL (Standard `https://oauth2.googleapis.com/token`,
//!    konfigurierbar): `grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer&assertion=<jwt>`.
//! 4. Antwort `{"access_token","expires_in"}` im RAM cachen.
//!
//! **Cache:** [`crate::auth::RefreshingToken`] (gemeinsam mit Entra ID): gültig bis
//! `expires_in` − Sicherheitsabstand, Erneuerung im Hintergrund vor Ablauf, ein Abruf für alle
//! wartenden Anfragen.

use std::fmt;

use aigw_core::ir::ErrorClass;
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use http::Uri;
use serde_json::{Value, json};

pub use crate::auth::{DEFAULT_REFRESH_LEAD, SAFETY_MARGIN};
use crate::auth::{FetchFuture, RefreshingToken, TokenFetcher, post_token_form, token_error};
use crate::client::UpstreamClient;
use crate::codec::{base64_decode, base64url, unix_now};

/// Standard-Token-Endpunkt von Google.
pub const DEFAULT_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// Scope für Vertex AI.
pub const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
/// Lebensdauer des JWT (Google: höchstens 1 h).
const JWT_LIFETIME_SECS: u64 = 3600;

/// Fehler beim Lesen eines Service-Account-Schlüssels.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Service-Account-Schlüssel: {0}")]
pub struct KeyError(pub String);

/// Service-Account-Schlüssel (Inhalt der JSON-Schlüsseldatei).
pub struct ServiceAccountKey {
    client_email: String,
    private_key_id: Option<String>,
    project_id: Option<String>,
    token_uri: Option<String>,
    key: RsaKeyPair,
}

impl fmt::Debug for ServiceAccountKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceAccountKey")
            .field("client_email", &self.client_email)
            .field("private_key_id", &self.private_key_id)
            .field("project_id", &self.project_id)
            .finish_non_exhaustive()
    }
}

/// PEM (`-----BEGIN PRIVATE KEY-----`, PKCS#8) → DER.
fn pem_to_der(pem: &str) -> Result<Vec<u8>, KeyError> {
    let pem = pem.replace("\\n", "\n");
    let begin = "-----BEGIN PRIVATE KEY-----";
    let end = "-----END PRIVATE KEY-----";
    let start = pem.find(begin).ok_or_else(|| {
        KeyError("private_key ist kein PKCS#8-PEM (BEGIN PRIVATE KEY)".to_owned())
    })? + begin.len();
    let stop = pem[start..]
        .find(end)
        .ok_or_else(|| KeyError("private_key: END-Zeile fehlt".to_owned()))?
        + start;
    base64_decode(&pem[start..stop])
        .ok_or_else(|| KeyError("private_key: Base64 ungültig".to_owned()))
}

impl ServiceAccountKey {
    /// Liest die JSON-Schlüsseldatei (`type: service_account`).
    pub fn from_json(json: &str) -> Result<Self, KeyError> {
        let v: Value =
            serde_json::from_str(json).map_err(|e| KeyError(format!("kein JSON: {e}")))?;
        if let Some(t) = v.get("type").and_then(Value::as_str)
            && t != "service_account"
        {
            return Err(KeyError(format!("type = {t:?}, erwartet service_account")));
        }
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        let email = s("client_email").ok_or_else(|| KeyError("client_email fehlt".to_owned()))?;
        let pem = s("private_key").ok_or_else(|| KeyError("private_key fehlt".to_owned()))?;
        let mut key = Self::from_pem(&email, &pem)?;
        key.private_key_id = s("private_key_id");
        key.project_id = s("project_id");
        key.token_uri = s("token_uri");
        Ok(key)
    }

    /// Aus E-Mail und PEM-Schlüssel (PKCS#8).
    pub fn from_pem(client_email: &str, private_key_pem: &str) -> Result<Self, KeyError> {
        Self::from_pkcs8(client_email, &pem_to_der(private_key_pem)?)
    }

    /// Aus E-Mail und PKCS#8-DER.
    pub fn from_pkcs8(client_email: &str, der: &[u8]) -> Result<Self, KeyError> {
        let key = RsaKeyPair::from_pkcs8(der)
            .map_err(|e| KeyError(format!("RSA-Schlüssel ungültig: {e}")))?;
        Ok(Self {
            client_email: client_email.to_owned(),
            private_key_id: None,
            project_id: None,
            token_uri: None,
            key,
        })
    }

    /// `client_email`.
    pub fn client_email(&self) -> &str {
        &self.client_email
    }

    /// `project_id` der Schlüsseldatei.
    pub fn project_id(&self) -> Option<&str> {
        self.project_id.as_deref()
    }

    /// `token_uri` der Schlüsseldatei.
    pub fn token_uri(&self) -> Option<&str> {
        self.token_uri.as_deref()
    }

    /// Signiertes JWT für den Token-Austausch.
    pub fn jwt(&self, scope: &str, audience: &str, now_unix: u64) -> Result<String, KeyError> {
        let mut header = json!({ "alg": "RS256", "typ": "JWT" });
        if let Some(kid) = &self.private_key_id {
            header["kid"] = json!(kid);
        }
        let claims = json!({
            "iss": self.client_email,
            "scope": scope,
            "aud": audience,
            "iat": now_unix,
            "exp": now_unix + JWT_LIFETIME_SECS,
        });
        let mut signing_input = base64url(header.to_string().as_bytes());
        signing_input.push('.');
        signing_input.push_str(&base64url(claims.to_string().as_bytes()));
        let mut sig = vec![0u8; self.key.public_modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                signing_input.as_bytes(),
                &mut sig,
            )
            .map_err(|_| KeyError("Signieren fehlgeschlagen".to_owned()))?;
        signing_input.push('.');
        signing_input.push_str(&base64url(&sig));
        Ok(signing_input)
    }
}

/// Abrufer für Service-Account-Tokens (JWT-Bearer-Grant).
pub struct ServiceAccountFetcher {
    key: ServiceAccountKey,
    token_url: Uri,
    audience: String,
    scope: String,
}

impl fmt::Debug for ServiceAccountFetcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceAccountFetcher")
            .field("client_email", &self.key.client_email)
            .field("token_url", &self.token_url)
            .finish_non_exhaustive()
    }
}

/// Token-Quelle für einen Service-Account mit Cache und Hintergrund-Erneuerung.
pub type ServiceAccountTokenSource = RefreshingToken<ServiceAccountFetcher>;

impl RefreshingToken<ServiceAccountFetcher> {
    /// Neue Token-Quelle. `token_url` ist auch die JWT-Audience.
    pub fn new(key: ServiceAccountKey, token_url: Uri, scope: impl Into<String>) -> Self {
        Self::with_fetcher(ServiceAccountFetcher {
            key,
            audience: token_url.to_string(),
            token_url,
            scope: scope.into(),
        })
    }
}

impl TokenFetcher for ServiceAccountFetcher {
    fn label(&self) -> &'static str {
        "Vertex"
    }

    fn fetch<'a>(&'a self, client: &'a UpstreamClient) -> FetchFuture<'a> {
        Box::pin(async move {
            let jwt = self
                .key
                .jwt(&self.scope, &self.audience, unix_now())
                .map_err(|e| token_error(ErrorClass::Internal, e.to_string(), Some(500)))?;
            let body = format!(
                "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion={jwt}"
            );
            post_token_form(client, &self.token_url, body).await
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use aws_lc_rs::encoding::AsDer as _;
    use aws_lc_rs::rsa::KeySize;
    use aws_lc_rs::signature::{KeyPair as _, RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};

    use super::*;
    use crate::codec::base64;

    #[test]
    fn jwt_is_valid_rs256() {
        let kp = RsaKeyPair::generate(KeySize::Rsa2048).unwrap();
        let der = kp.as_der().unwrap();
        let pem = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            base64(der.as_ref())
        );
        let sa = json!({
            "type": "service_account", "project_id": "p1", "private_key_id": "kid1",
            "private_key": pem, "client_email": "sa@p1.iam.gserviceaccount.com",
            "token_uri": "https://oauth2.googleapis.com/token"
        });
        let key = ServiceAccountKey::from_json(&sa.to_string()).unwrap();
        assert_eq!(key.project_id(), Some("p1"));
        assert!(!format!("{key:?}").contains("PRIVATE"));
        let jwt = key
            .jwt(CLOUD_PLATFORM_SCOPE, DEFAULT_TOKEN_URL, 1_700_000_000)
            .unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: Value = serde_json::from_slice(&base64_decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header, json!({"alg": "RS256", "typ": "JWT", "kid": "kid1"}));
        let claims: Value = serde_json::from_slice(&base64_decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["iss"], "sa@p1.iam.gserviceaccount.com");
        assert_eq!(claims["aud"], DEFAULT_TOKEN_URL);
        assert_eq!(claims["exp"], 1_700_003_600u64);
        let sig = base64_decode(parts[2]).unwrap();
        let pk = UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, kp.public_key().as_ref());
        pk.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig)
            .unwrap();
    }

    #[test]
    fn rejects_bad_keys() {
        assert!(ServiceAccountKey::from_pem("x", "kein pem").is_err());
        assert!(
            ServiceAccountKey::from_pem(
                "x",
                "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----"
            )
            .is_err()
        );
        assert!(ServiceAccountKey::from_json(r#"{"type":"authorized_user"}"#).is_err());
    }
}
