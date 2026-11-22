// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Integrationstests des Workload-Token-Prüfers (AIGW-115) gegen einen lokalen JWKS-Server:
//! Signatur RS256/ES256, Verfahrens-Allowlist (`none`/HS256), Aussteller/Zielgruppe/Ablauf,
//! JWKS-Cache, Schlüsselwechsel mit gedrosseltem Nachladen, Discovery, Token-Cache,
//! Größenlimit, Profil `entra` mit Beispiel-Claims aus der Microsoft-Doku.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use aigw_core::workload::{JwtAuthConfig, JwtIssuerConfig, JwtProfile, WorkloadTokenKind};
use aigw_jwt::testkit::{JwksServer, TestIssuer, entra_agent_claims, now};
use aigw_jwt::{TokenError, WorkloadVerifier};
use serde_json::json;

const TID: &str = "aaaabbbb-0000-cccc-1111-dddd2222eeee";
const AGENT: &str = "11112222-bbbb-3333-cccc-4444dddd5555";

fn generic(srv: &JwksServer, discovery: bool) -> JwtIssuerConfig {
    JwtIssuerConfig {
        name: "test".into(),
        profile: JwtProfile::Generic,
        issuers: vec![srv.issuer()],
        jwks_uri: (!discovery).then(|| srv.jwks_uri()),
        audiences: vec!["api://ai-gw".into()],
        ..JwtIssuerConfig::default()
    }
}

fn verifier(issuers: Vec<JwtIssuerConfig>, refresh_min: u64) -> WorkloadVerifier {
    WorkloadVerifier::new(&JwtAuthConfig {
        issuers,
        jwks_refresh_min_secs: refresh_min,
        ..JwtAuthConfig::default()
    })
    .unwrap()
    .unwrap()
}

fn claims(srv: &JwksServer, sub: &str) -> serde_json::Value {
    let now = now();
    json!({"iss": srv.issuer(), "aud": "api://ai-gw", "sub": sub, "iat": now, "exp": now + 600})
}

async fn check(v: &WorkloadVerifier, token: &str) -> Result<String, TokenError> {
    v.prepare(token).await;
    v.verify(token, now()).map(|p| p.subject.clone())
}

#[tokio::test]
async fn disabled_without_issuers() {
    assert!(
        WorkloadVerifier::new(&JwtAuthConfig::default())
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn valid_tokens_rs256_es256_and_cache() {
    let rsa = TestIssuer::rsa("k1");
    let ec = TestIssuer::ec("k2");
    let srv = JwksServer::start(json!({"keys": [rsa.jwk(), ec.jwk()]})).await;
    let v = verifier(vec![generic(&srv, false)], 30);
    let t1 = rsa.sign(&claims(&srv, "agent-1"));
    assert_eq!(check(&v, &t1).await.unwrap(), "agent-1");
    let t2 = ec.sign(&claims(&srv, "agent-2"));
    assert_eq!(check(&v, &t2).await.unwrap(), "agent-2");
    // JWKS einmal geladen, geprüfte Tokens im Cache.
    assert_eq!(srv.hits(), 1);
    assert_eq!(v.cached_tokens(), 2);
    assert_eq!(check(&v, &t1).await.unwrap(), "agent-1");
    assert_eq!(srv.hits(), 1);
}

#[tokio::test]
async fn rejects_forged_and_disallowed_algorithms() {
    let good = TestIssuer::rsa("k1");
    let evil = TestIssuer::rsa("k1");
    let srv = JwksServer::start(good.jwks()).await;
    let v = verifier(vec![generic(&srv, false)], 30);
    let c = claims(&srv, "agent");
    // Fremder Schlüssel mit gleicher kid.
    assert!(matches!(
        check(&v, &evil.sign(&c)).await,
        Err(TokenError::Invalid(_))
    ));
    // alg none / HS256 / RS512 werden nie angenommen.
    for alg in ["none", "HS256", "RS512"] {
        let t = good.sign_with_header(&json!({"alg": alg, "kid": "k1"}), &c);
        assert!(check(&v, &t).await.is_err(), "{alg}");
    }
    // crit-Header.
    let t = good.sign_with_header(&json!({"alg": "RS256", "kid": "k1", "crit": ["x"]}), &c);
    assert!(check(&v, &t).await.is_err());
    // Manipulierte Claims bei gültiger Signatur des Originals.
    let t = good.sign(&c);
    let mut parts: Vec<&str> = t.split('.').collect();
    let other = aigw_jwt::jws::b64url(
        serde_json::to_string(&claims(&srv, "admin"))
            .unwrap()
            .as_bytes(),
    );
    parts[1] = &other;
    assert!(check(&v, &parts.join(".")).await.is_err());
    // ES256-Schlüssel, aber Konfiguration erlaubt nur RS256.
    let ec = TestIssuer::ec("e1");
    srv.set_jwks(json!({"keys": [good.jwk(), ec.jwk()]}));
    let mut cfg = generic(&srv, false);
    cfg.algorithms = vec!["RS256".into()];
    let v = verifier(vec![cfg], 0);
    assert!(check(&v, &ec.sign(&c)).await.is_err());
    assert!(check(&v, &good.sign(&c)).await.is_ok());
}

#[tokio::test]
async fn rejects_wrong_issuer_audience_and_time() {
    let k = TestIssuer::rsa("k1");
    let srv = JwksServer::start(k.jwks()).await;
    let v = verifier(vec![generic(&srv, false)], 30);
    let mut c = claims(&srv, "a");
    c["aud"] = json!("api://anders");
    assert!(check(&v, &k.sign(&c)).await.is_err());
    let mut c = claims(&srv, "a");
    c["iss"] = json!("https://idp.example");
    assert!(check(&v, &k.sign(&c)).await.is_err());
    let mut c = claims(&srv, "a");
    c["exp"] = json!(now() - 3600);
    assert!(check(&v, &k.sign(&c)).await.is_err());
    let mut c = claims(&srv, "a");
    c["nbf"] = json!(now() + 3600);
    assert!(check(&v, &k.sign(&c)).await.is_err());
    // Größenlimit: ohne Prüfung und ohne Abruf abgelehnt.
    let mut c = claims(&srv, "a");
    c["pad"] = json!("x".repeat(20_000));
    let before = srv.hits();
    assert!(check(&v, &k.sign(&c)).await.is_err());
    assert_eq!(srv.hits(), before);
    // Kein JWT.
    assert!(matches!(
        check(&v, "sk-aigw-abc").await,
        Err(TokenError::Invalid(_))
    ));
}

#[tokio::test]
async fn key_rotation_with_throttled_refresh() {
    let old = TestIssuer::rsa("old");
    let new = TestIssuer::rsa("new");
    let srv = JwksServer::start(old.jwks()).await;
    // Drosselung 30 s: unbekannte kid lädt höchstens einmal nach.
    let v = verifier(vec![generic(&srv, false)], 30);
    assert!(check(&v, &old.sign(&claims(&srv, "a"))).await.is_ok());
    assert_eq!(srv.hits(), 1);
    // Aussteller wechselt den Schlüssel: erster Versuch lädt sofort? Nein – der letzte Abruf
    // liegt weniger als 30 s zurück, also gedrosselt.
    srv.set_jwks(json!({"keys": [old.jwk(), new.jwk()]}));
    assert!(check(&v, &new.sign(&claims(&srv, "b"))).await.is_err());
    assert!(check(&v, &new.sign(&claims(&srv, "c"))).await.is_err());
    assert_eq!(srv.hits(), 1, "gedrosselt");

    // Ohne Drosselung: unbekannte kid lädt das JWKS nach.
    let v = verifier(vec![generic(&srv, false)], 0);
    srv.set_jwks(old.jwks());
    assert!(check(&v, &old.sign(&claims(&srv, "a"))).await.is_ok());
    let hits = srv.hits();
    srv.set_jwks(json!({"keys": [new.jwk()]}));
    assert!(check(&v, &new.sign(&claims(&srv, "b"))).await.is_ok());
    assert_eq!(srv.hits(), hits + 1);
    // Unbekannte kid, die es auch nach dem Nachladen nicht gibt.
    let ghost = TestIssuer::rsa("ghost");
    assert!(check(&v, &ghost.sign(&claims(&srv, "g"))).await.is_err());
}

#[tokio::test]
async fn discovery_and_issuer_unreachable() {
    let k = TestIssuer::ec("k1");
    let srv = JwksServer::start(k.jwks()).await;
    let v = verifier(vec![generic(&srv, true)], 30);
    assert_eq!(check(&v, &k.sign(&claims(&srv, "a"))).await.unwrap(), "a");
    // Aussteller nicht erreichbar: Prüfung schlägt fehl (401), kein Absturz.
    let cfg = JwtIssuerConfig {
        name: "weg".into(),
        issuers: vec!["http://127.0.0.1:9".into()],
        jwks_uri: Some("http://127.0.0.1:9/keys".into()),
        audiences: vec!["api://ai-gw".into()],
        ..JwtIssuerConfig::default()
    };
    let v = verifier(vec![cfg], 30);
    let mut c = claims(&srv, "a");
    c["iss"] = json!("http://127.0.0.1:9");
    assert!(matches!(
        check(&v, &k.sign(&c)).await,
        Err(TokenError::Invalid(_))
    ));
}

#[tokio::test]
async fn entra_profile_with_documented_agent_claims() {
    let k = TestIssuer::rsa("entra-kid");
    let srv = JwksServer::start(k.jwks()).await;
    let cfg = JwtIssuerConfig {
        name: "entra".into(),
        profile: JwtProfile::Entra,
        tenant_id: Some(TID.into()),
        // Test: JWKS lokal statt login.microsoftonline.com.
        jwks_uri: Some(srv.jwks_uri()),
        audiences: vec!["api://ai-gw".into()],
        required_roles: vec!["Gateway.Invoke".into()],
        require_agent: true,
        ..JwtIssuerConfig::default()
    };
    let v = verifier(vec![cfg], 30);
    let mut c = entra_agent_claims(TID, AGENT, "api://ai-gw");
    // Ohne App-Rolle: gültig, aber nicht berechtigt (403).
    assert!(matches!(
        check(&v, &k.sign(&c)).await,
        Err(TokenError::Forbidden(_))
    ));
    c["roles"] = json!(["Gateway.Invoke"]);
    let t = k.sign(&c);
    v.prepare(&t).await;
    let p = v.verify(&t, now()).unwrap();
    assert_eq!(p.issuer, "entra");
    assert_eq!(p.subject, AGENT);
    assert_eq!(p.kind, WorkloadTokenKind::App);
    assert!(p.agent);
    assert_eq!(
        p.blueprint.as_deref(),
        Some("30cf4c22-9985-4ef7-8756-91cc888176bd")
    );
    // Fremder Tenant.
    let mut c2 = c.clone();
    c2["tid"] = json!("99999999-0000-cccc-1111-dddd2222eeee");
    assert!(check(&v, &k.sign(&c2)).await.is_err());
    // Kein Agent (klassischer Service Principal) bei require_agent.
    let mut c3 = c.clone();
    c3["xms_act_fct"] = json!("3 9");
    assert!(matches!(
        check(&v, &k.sign(&c3)).await,
        Err(TokenError::Forbidden(_))
    ));
}
