// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Testhilfen (Feature `testkit`): eigener Aussteller mit RSA-Schlüssel (RS256), lokaler
//! JWKS-/Discovery-Server und Beispiel-Claims nach der Microsoft-Doku für Agent-Tokens.
//! Nur für Tests – keine echten Tokens, keine externen Aufrufe.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::missing_panics_doc,
    missing_debug_implementations
)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::rsa::{KeyPair, KeySize};
use aws_lc_rs::signature::{
    ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair as _, RSA_PKCS1_SHA256,
};
use axum::Json;
use axum::extract::State;
use axum::routing::get;
use serde_json::{Value, json};

use crate::jws::b64url;

enum Key {
    Rsa(KeyPair),
    Ec(EcdsaKeyPair),
}

/// Test-Aussteller mit einem Signaturschlüssel.
pub struct TestIssuer {
    /// Schlüssel-ID.
    pub kid: String,
    key: Key,
}

impl TestIssuer {
    /// RSA-2048 (RS256), wie Entra ID.
    pub fn rsa(kid: &str) -> Self {
        Self {
            kid: kid.to_owned(),
            key: Key::Rsa(KeyPair::generate(KeySize::Rsa2048).unwrap()),
        }
    }

    /// P-256 (ES256).
    pub fn ec(kid: &str) -> Self {
        Self {
            kid: kid.to_owned(),
            key: Key::Ec(EcdsaKeyPair::generate(&ECDSA_P256_SHA256_FIXED_SIGNING).unwrap()),
        }
    }

    /// Verfahren (`RS256` bzw. `ES256`).
    pub fn alg(&self) -> &'static str {
        match self.key {
            Key::Rsa(_) => "RS256",
            Key::Ec(_) => "ES256",
        }
    }

    /// Öffentlicher Schlüssel als JWK.
    pub fn jwk(&self) -> Value {
        match &self.key {
            Key::Rsa(kp) => {
                let (n, e) = rsa_components(kp.public_key().as_ref());
                json!({
                    "kty": "RSA", "use": "sig", "kid": self.kid,
                    "n": b64url(n), "e": b64url(e),
                })
            }
            Key::Ec(kp) => {
                let pk = kp.public_key().as_ref();
                json!({
                    "kty": "EC", "use": "sig", "crv": "P-256", "kid": self.kid,
                    "x": b64url(&pk[1..33]), "y": b64url(&pk[33..65]),
                })
            }
        }
    }

    /// JWKS mit genau diesem Schlüssel.
    pub fn jwks(&self) -> Value {
        json!({"keys": [self.jwk()]})
    }

    /// Signiert `claims` (Header `alg`, `typ: JWT`, `kid`).
    pub fn sign(&self, claims: &Value) -> String {
        self.sign_with_header(
            &json!({"alg": self.alg(), "typ": "JWT", "kid": self.kid}),
            claims,
        )
    }

    /// Signiert mit beliebigem Header (für Negativtests, z. B. `alg: none`).
    pub fn sign_with_header(&self, header: &Value, claims: &Value) -> String {
        let input = format!(
            "{}.{}",
            b64url(&serde_json::to_vec(header).unwrap()),
            b64url(&serde_json::to_vec(claims).unwrap())
        );
        let rng = SystemRandom::new();
        let sig = match &self.key {
            Key::Rsa(kp) => {
                let mut sig = vec![0u8; kp.public_modulus_len()];
                kp.sign(&RSA_PKCS1_SHA256, &rng, input.as_bytes(), &mut sig)
                    .unwrap();
                sig
            }
            Key::Ec(kp) => kp.sign(&rng, input.as_bytes()).unwrap().as_ref().to_vec(),
        };
        format!("{input}.{}", b64url(&sig))
    }
}

/// Zerlegt einen PKCS#1-`RSAPublicKey` (DER: `SEQUENCE { INTEGER n, INTEGER e }`) in Modulus
/// und Exponent ohne führende Nullbytes.
fn rsa_components(der: &[u8]) -> (&[u8], &[u8]) {
    fn tlv(b: &[u8], tag: u8) -> (&[u8], &[u8]) {
        assert_eq!(b[0], tag, "DER-Tag");
        let (len, off) = if b[1] & 0x80 == 0 {
            (usize::from(b[1]), 2)
        } else {
            let k = usize::from(b[1] & 0x7f);
            (
                b[2..2 + k]
                    .iter()
                    .fold(0usize, |a, x| (a << 8) | usize::from(*x)),
                2 + k,
            )
        };
        (&b[off..off + len], &b[off + len..])
    }
    let strip = |x: &'_ [u8]| -> usize { x.iter().position(|v| *v != 0).unwrap_or(x.len()) };
    let (seq, _) = tlv(der, 0x30);
    let (n, rest) = tlv(seq, 0x02);
    let (e, _) = tlv(rest, 0x02);
    (&n[strip(n)..], &e[strip(e)..])
}

/// Aktuelle Unix-Zeit (Sekunden).
pub fn now() -> i64 {
    crate::now_unix()
}

/// Claims eines Entra-Agent-Tokens „Agent handelt autonom“ (Beispiel aus
/// <https://learn.microsoft.com/en-us/entra/agent-id/agent-token-claims>, v1-Format),
/// gültig ab jetzt für eine Stunde.
pub fn entra_agent_claims(tenant: &str, agent_app_id: &str, audience: &str) -> Value {
    let now = now();
    json!({
        "aud": audience,
        "iss": format!("https://sts.windows.net/{tenant}/"),
        "iat": now, "nbf": now, "exp": now + 3600,
        "appid": agent_app_id, "appidacr": "2", "idtyp": "app",
        "oid": "aaaaaaaa-0000-1111-2222-bbbbbbbbbbbb",
        "sub": "bbbbbbbb-1111-2222-3333-cccccccccccc",
        "tid": tenant, "ver": "1.0",
        "xms_act_fct": "3 9 11", "xms_idrel": "7 10", "xms_sub_fct": "9 3 11",
        "xms_tnt_fct": "3 9",
        "xms_par_app_azp": "30cf4c22-9985-4ef7-8756-91cc888176bd"
    })
}

struct ServerState {
    jwks: Mutex<Value>,
    hits: AtomicUsize,
    issuer: Mutex<String>,
}

/// Lokaler JWKS-/Discovery-Server (`/keys`, `/.well-known/openid-configuration`).
#[derive(Clone)]
pub struct JwksServer {
    /// Adresse.
    pub addr: SocketAddr,
    state: Arc<ServerState>,
}

async fn keys(State(s): State<Arc<ServerState>>) -> Json<Value> {
    s.hits.fetch_add(1, Ordering::SeqCst);
    Json(s.jwks.lock().unwrap().clone())
}

async fn discovery(State(s): State<Arc<ServerState>>) -> Json<Value> {
    let issuer = s.issuer.lock().unwrap().clone();
    Json(json!({"issuer": issuer, "jwks_uri": format!("{issuer}/keys")}))
}

impl JwksServer {
    /// Startet den Server mit diesem JWKS.
    pub async fn start(jwks: Value) -> Self {
        let state = Arc::new(ServerState {
            jwks: Mutex::new(jwks),
            hits: AtomicUsize::new(0),
            issuer: Mutex::new(String::new()),
        });
        let app = axum::Router::new()
            .route("/keys", get(keys))
            .route("/.well-known/openid-configuration", get(discovery))
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        *state.issuer.lock().unwrap() = format!("http://{addr}");
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { addr, state }
    }

    /// Aussteller-URL (`http://127.0.0.1:<port>`; zugleich Discovery-Basis).
    pub fn issuer(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// JWKS-URL.
    pub fn jwks_uri(&self) -> String {
        format!("http://{}/keys", self.addr)
    }

    /// Tauscht das ausgelieferte JWKS (Schlüsselwechsel).
    pub fn set_jwks(&self, jwks: Value) {
        *self.state.jwks.lock().unwrap() = jwks;
    }

    /// Bisherige Abrufe von `/keys`.
    pub fn hits(&self) -> usize {
        self.state.hits.load(Ordering::SeqCst)
    }
}
