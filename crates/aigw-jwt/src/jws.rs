// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! JWS-Grundbausteine (JWS Compact, JWK/JWKS), gemeinsam für den OIDC-Login der Admin-UI
//! (AIGW-32) und Workload-Tokens der Datenebene (AIGW-115).
//!
//! Signaturprüfung **ausschließlich** über aws-lc-rs – keine eigene Kryptografie. Welche
//! Verfahren zulässig sind, entscheidet der Aufrufer (Allowlist); `none` und HMAC kennt
//! dieses Modul gar nicht.

use aws_lc_rs::signature::{
    ECDSA_P256_SHA256_FIXED, ED25519, RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents,
    UnparsedPublicKey,
};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use serde_json::Value;

/// Fehler beim Zerlegen oder Prüfen (Details nur fürs Log).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct JwsError(pub String);

pub(crate) fn err(s: impl Into<String>) -> JwsError {
    JwsError(s.into())
}

/// Ein Schlüssel aus dem JWKS (nur die benötigten Felder).
#[derive(Debug, Clone, Deserialize)]
pub struct Jwk {
    /// Schlüsseltyp (`RSA`, `EC`, `OKP`).
    pub kty: String,
    /// Schlüssel-ID.
    #[serde(default)]
    pub kid: Option<String>,
    /// Verwendung (`sig`).
    #[serde(default, rename = "use")]
    pub use_: Option<String>,
    /// Verfahren.
    #[serde(default)]
    pub alg: Option<String>,
    /// Kurve (`P-256`, `Ed25519`).
    #[serde(default)]
    pub crv: Option<String>,
    /// RSA-Modulus.
    #[serde(default)]
    pub n: Option<String>,
    /// RSA-Exponent.
    #[serde(default)]
    pub e: Option<String>,
    /// EC/OKP x.
    #[serde(default)]
    pub x: Option<String>,
    /// EC y.
    #[serde(default)]
    pub y: Option<String>,
}

/// JWK-Set.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Jwks {
    /// Schlüssel.
    pub keys: Vec<Jwk>,
}

impl Jwks {
    /// Enthält das Set einen Schlüssel mit dieser `kid`?
    pub fn has_kid(&self, kid: &str) -> bool {
        self.keys.iter().any(|k| k.kid.as_deref() == Some(kid))
    }

    /// Prüft die Signatur mit allen passenden Schlüsseln (`kid`, `use`, `alg`).
    pub fn verify(&self, header: &JwsHeader, input: &[u8], sig: &[u8]) -> Result<bool, JwsError> {
        let candidates = self.keys.iter().filter(|k| {
            header
                .kid
                .as_ref()
                .is_none_or(|kid| k.kid.as_ref() == Some(kid))
                && k.use_.as_deref().is_none_or(|u| u == "sig")
                && k.alg.as_deref().is_none_or(|a| a == header.alg)
        });
        for k in candidates {
            if verify_jws(&header.alg, k, input, sig)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// JWS-Header (benötigte Felder).
#[derive(Debug, Clone, Deserialize)]
pub struct JwsHeader {
    /// Verfahren.
    pub alg: String,
    /// Schlüssel-ID.
    #[serde(default)]
    pub kid: Option<String>,
    /// Kritische Erweiterungen (werden nicht unterstützt → Ablehnung).
    #[serde(default)]
    pub crit: Option<Value>,
}

/// Zerlegtes JWS Compact (Signatur noch **nicht** geprüft).
#[derive(Debug)]
pub struct Compact<'a> {
    /// Header.
    pub header: JwsHeader,
    /// Payload (JSON, ungeprüft).
    pub claims: Value,
    /// Signierte Eingabe (`<header>.<payload>`).
    pub signing_input: &'a [u8],
    /// Signatur.
    pub signature: Vec<u8>,
}

/// Zerlegt ein JWS Compact (drei Teile, Base64url). `crit` wird abgelehnt.
pub fn parse_compact(token: &str) -> Result<Compact<'_>, JwsError> {
    let mut parts = token.split('.');
    let (Some(h64), Some(p64), Some(s64), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(err("kein JWS (3 Teile erwartet)"));
    };
    let header: JwsHeader =
        serde_json::from_slice(&b64(h64)?).map_err(|e| err(format!("JWS-Header: {e}")))?;
    if header.crit.is_some() {
        return Err(err("JWS-Header mit crit wird nicht unterstützt"));
    }
    let claims: Value =
        serde_json::from_slice(&b64(p64)?).map_err(|e| err(format!("JWS-Payload: {e}")))?;
    if !claims.is_object() {
        return Err(err("JWS-Payload ist kein JSON-Objekt"));
    }
    Ok(Compact {
        header,
        claims,
        signing_input: &token.as_bytes()[..h64.len() + 1 + p64.len()],
        signature: b64(s64)?,
    })
}

/// Base64url ohne Auffüllung dekodieren.
pub fn b64(s: &str) -> Result<Vec<u8>, JwsError> {
    URL_SAFE_NO_PAD
        .decode(s)
        .map_err(|e| err(format!("Base64url: {e}")))
}

/// Base64url ohne Auffüllung kodieren.
pub fn b64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Prüft eine JWS-Signatur mit einem JWK (RS256, ES256, EdDSA/Ed25519). `Ok(false)` =
/// Signatur passt nicht oder Schlüssel passt nicht zum Verfahren; `Err` = Schlüssel kaputt.
pub fn verify_jws(alg: &str, k: &Jwk, input: &[u8], sig: &[u8]) -> Result<bool, JwsError> {
    let field = |v: &Option<String>, name: &str| -> Result<Vec<u8>, JwsError> {
        b64(v
            .as_deref()
            .ok_or_else(|| err(format!("JWK ohne {name}")))?)
    };
    match (alg, k.kty.as_str()) {
        ("RS256", "RSA") => {
            let key = RsaPublicKeyComponents {
                n: field(&k.n, "n")?,
                e: field(&k.e, "e")?,
            };
            Ok(key.verify(&RSA_PKCS1_2048_8192_SHA256, input, sig).is_ok())
        }
        ("ES256", "EC") if k.crv.as_deref() == Some("P-256") => {
            let (x, y) = (field(&k.x, "x")?, field(&k.y, "y")?);
            if x.len() != 32 || y.len() != 32 {
                return Err(err("JWK P-256: x/y nicht 32 Byte"));
            }
            let mut point = Vec::with_capacity(65);
            point.push(0x04);
            point.extend_from_slice(&x);
            point.extend_from_slice(&y);
            Ok(UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, point)
                .verify(input, sig)
                .is_ok())
        }
        ("EdDSA", "OKP") if k.crv.as_deref() == Some("Ed25519") => {
            let x = field(&k.x, "x")?;
            Ok(UnparsedPublicKey::new(&ED25519, x)
                .verify(input, sig)
                .is_ok())
        }
        // Schlüssel passt nicht zum Verfahren: nicht verwenden.
        _ => Ok(false),
    }
}

/// Liest einen Claim (Punkte = verschachtelt, voller Name zuerst) als Liste von
/// Zeichenketten (Array oder einzelne Zeichenkette).
pub fn claim_strings(c: &Value, path: &str) -> Vec<String> {
    if path.is_empty() {
        return Vec::new();
    }
    let mut v = c;
    if let Some(x) = c.get(path) {
        v = x;
    } else {
        for part in path.split('.') {
            match v.get(part) {
                Some(x) => v = x,
                None => return Vec::new(),
            }
        }
    }
    match v {
        Value::Array(a) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Value::String(s) => vec![s.clone()],
        _ => Vec::new(),
    }
}

/// Sieht der Wert wie ein JWS Compact aus (`eyJ…` mit genau zwei Punkten, nur
/// Base64url-Zeichen)? Grundlage der Unterscheidung API-Key ↔ Access-Token: virtuelle Keys
/// (`sk-aigw-…`) enthalten nie Punkte.
pub fn looks_like_jwt(s: &str) -> bool {
    s.starts_with("eyJ")
        && s.bytes().filter(|b| *b == b'.').count() == 2
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jwt_detection() {
        assert!(looks_like_jwt("eyJhbGciOiJSUzI1NiJ9.eyJ4IjoxfQ.c2ln"));
        assert!(!looks_like_jwt("sk-aigw-abc_def"));
        assert!(!looks_like_jwt("eyJhbGciOiJSUzI1NiJ9.eyJ4IjoxfQ"));
        assert!(!looks_like_jwt("eyJ.a.b.c"));
        assert!(!looks_like_jwt("eyJ a.b.c"));
    }

    #[test]
    fn compact_parsing() {
        assert!(parse_compact("a.b").is_err());
        assert!(parse_compact("a.b.c.d").is_err());
        let h = b64url(br#"{"alg":"RS256","crit":["x"]}"#);
        let p = b64url(br#"{"a":1}"#);
        assert!(parse_compact(&format!("{h}.{p}.")).is_err());
        let h = b64url(br#"{"alg":"RS256","kid":"k"}"#);
        let tok = format!("{h}.{p}.c2ln");
        let c = parse_compact(&tok).unwrap();
        assert_eq!(c.header.kid.as_deref(), Some("k"));
        assert_eq!(c.signature, b"sig");
        assert_eq!(c.signing_input, format!("{h}.{p}").as_bytes());
        let arr = b64url(b"[1]");
        assert!(parse_compact(&format!("{h}.{arr}.c2ln")).is_err());
    }

    #[test]
    fn jws_verification_with_aws_lc() {
        use aws_lc_rs::rand::SystemRandom;
        use aws_lc_rs::signature::{
            ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, Ed25519KeyPair, KeyPair as _,
        };
        let rng = SystemRandom::new();
        let input = b"header.payload";

        let ed = Ed25519KeyPair::generate().unwrap();
        let jwk = Jwk {
            kty: "OKP".into(),
            kid: None,
            use_: None,
            alg: None,
            crv: Some("Ed25519".into()),
            n: None,
            e: None,
            x: Some(b64url(ed.public_key().as_ref())),
            y: None,
        };
        let sig = ed.sign(input);
        assert!(verify_jws("EdDSA", &jwk, input, sig.as_ref()).unwrap());
        assert!(!verify_jws("EdDSA", &jwk, b"other", sig.as_ref()).unwrap());
        // falsches Verfahren für den Schlüssel
        assert!(!verify_jws("RS256", &jwk, input, sig.as_ref()).unwrap());

        let ec = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_FIXED_SIGNING).unwrap();
        let pk = ec.public_key().as_ref().to_vec();
        let jwk = Jwk {
            kty: "EC".into(),
            kid: None,
            use_: None,
            alg: None,
            crv: Some("P-256".into()),
            n: None,
            e: None,
            x: Some(b64url(&pk[1..33])),
            y: Some(b64url(&pk[33..65])),
        };
        let sig = ec.sign(&rng, input).unwrap();
        assert!(verify_jws("ES256", &jwk, input, sig.as_ref()).unwrap());
        assert!(!verify_jws("ES256", &jwk, b"x", sig.as_ref()).unwrap());
    }

    #[test]
    fn claim_lists() {
        let c = serde_json::json!({"a": {"b": ["x", "y"]}, "c.d": "z", "n": 1});
        assert_eq!(claim_strings(&c, "a.b"), vec!["x", "y"]);
        assert_eq!(claim_strings(&c, "c.d"), vec!["z"]);
        assert!(claim_strings(&c, "n").is_empty());
        assert!(claim_strings(&c, "").is_empty());
    }
}
