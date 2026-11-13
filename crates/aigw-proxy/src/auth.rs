// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Auslesen des API-Keys und Schlüsselprüfung (Stub bis AIGW-28).
//!
//! Unterstützte Header (Architektur §3, Schritt 2): `Authorization: Bearer <key>` (OpenAI),
//! `api-key: <key>` (Azure OpenAI) und `x-api-key: <key>` (Anthropic). Sind mehrere gesetzt,
//! gilt diese Reihenfolge.

use std::fmt;

use aigw_core::ir::ApiError;
use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;

/// Herkunft des Schlüssels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    /// `Authorization: Bearer …`.
    Bearer,
    /// `api-key: …`.
    ApiKeyHeader,
    /// `x-api-key: …`.
    XApiKey,
}

/// Vom Client übergebener Schlüssel. `Debug` zeigt den Schlüssel nicht.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey {
    /// Herkunft.
    pub source: KeySource,
    /// Schlüssel im Klartext (nur für die Hash-Berechnung, nie loggen).
    pub secret: String,
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiKey")
            .field("source", &self.source)
            .field("secret", &"***")
            .finish()
    }
}

/// Liest den API-Key aus den Headern (`None`, wenn keiner gesetzt ist).
pub fn extract_api_key(headers: &HeaderMap) -> Option<ApiKey> {
    if let Some(v) = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        let v = v.trim();
        if let Some((scheme, token)) = v.split_once(' ')
            && scheme.eq_ignore_ascii_case("bearer")
            && !token.trim().is_empty()
        {
            return Some(ApiKey {
                source: KeySource::Bearer,
                secret: token.trim().to_owned(),
            });
        }
    }
    for (name, source) in [
        ("api-key", KeySource::ApiKeyHeader),
        ("x-api-key", KeySource::XApiKey),
    ] {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(ApiKey {
                    source,
                    secret: v.to_owned(),
                });
            }
        }
    }
    None
}

/// Ergebnis einer erfolgreichen Prüfung (wird mit AIGW-28 zum `KeyCtx`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthContext {
    /// Kennung des Schlüssels (nie der Schlüssel selbst).
    pub key_id: Option<String>,
}

/// Schlüsselprüfung. Synchron: die Prüfung läuft im RAM (Hash-Lookup, Q7).
pub trait Authenticator: Send + Sync + 'static {
    /// Prüft den (optionalen) Schlüssel; Fehler sind üblicherweise
    /// [`ErrorKind::Authentication`](aigw_core::ir::ErrorKind::Authentication).
    fn authenticate(&self, key: Option<&ApiKey>) -> Result<AuthContext, ApiError>;
}

/// Platzhalter: lässt jede Anfrage zu (mit oder ohne Schlüssel).
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAll;

impl Authenticator for AllowAll {
    fn authenticate(&self, _key: Option<&ApiKey>) -> Result<AuthContext, ApiError> {
        Ok(AuthContext::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_static(v));
        }
        h
    }

    #[test]
    fn extracts_keys_in_priority_order() {
        let k = extract_api_key(&headers(&[("authorization", "Bearer sk-1")])).unwrap();
        assert_eq!((k.source, k.secret.as_str()), (KeySource::Bearer, "sk-1"));
        let k = extract_api_key(&headers(&[("authorization", "bearer  sk-2 ")])).unwrap();
        assert_eq!(k.secret, "sk-2");
        let k = extract_api_key(&headers(&[("api-key", "az"), ("x-api-key", "an")])).unwrap();
        assert_eq!(k.source, KeySource::ApiKeyHeader);
        let k = extract_api_key(&headers(&[("x-api-key", "an")])).unwrap();
        assert_eq!((k.source, k.secret.as_str()), (KeySource::XApiKey, "an"));
        let k = extract_api_key(&headers(&[
            ("authorization", "Basic abc"),
            ("x-api-key", "an"),
        ]))
        .unwrap();
        assert_eq!(k.source, KeySource::XApiKey);
        assert!(extract_api_key(&headers(&[("authorization", "Bearer ")])).is_none());
        assert!(extract_api_key(&HeaderMap::new()).is_none());
    }

    #[test]
    fn debug_hides_secret() {
        let k = ApiKey {
            source: KeySource::Bearer,
            secret: "geheim".into(),
        };
        assert!(!format!("{k:?}").contains("geheim"));
    }
}
