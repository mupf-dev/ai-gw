// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Integrationstests AIGW-32: OIDC-Login (Authorization Code + PKCE) gegen einen Mini-IdP im
//! Test (eigener axum-Server mit Discovery, JWKS, Authorize, Token; signiert mit Testschlüsseln
//! RS256/ES256/EdDSA), Sessions mit CSRF, Rollen und Geltungsbereiche, Ablauf, Abmeldung,
//! Admin-API-Keys und Abschaltung des Bootstrap-Tokens. Kein externer IdP.
//!
//! Läuft gegen SQLite (Datei) und – mit `AIGW_TEST_POSTGRES_URL` – gegen Postgres.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use aigw_admin::AdminState;
use aigw_core::config::{AdminAuthConfig, BootstrapMode, GroupRoleMapping, OidcConfig};
use aigw_core::resource::{ModelGroupSpec, ObjectMeta, Role, new_id};
use aigw_store::{AuditQuery, Store};
use aws_lc_rs::digest;
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{
    ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, Ed25519KeyPair, KeyPair as _, RSA_PKCS1_SHA256,
    RsaKeyPair,
};
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{HeaderMap, Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};
use sqlx::AssertSqlSafe;

const CLIENT_ID: &str = "aigw-test";
const CLIENT_SECRET: &str = "test-secret";

fn b64(b: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(b)
}

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

// =============================================================================================
// Mini-IdP
// =============================================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Alg {
    Rs256,
    Es256,
    EdDsa,
}

enum Signer {
    Rs(RsaKeyPair),
    Es(EcdsaKeyPair),
    Ed(Ed25519KeyPair),
}

impl Signer {
    fn new(alg: Alg) -> Self {
        match alg {
            Alg::Rs256 => Self::Rs(RsaKeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap()),
            Alg::Es256 => {
                Self::Es(EcdsaKeyPair::generate(&ECDSA_P256_SHA256_FIXED_SIGNING).unwrap())
            }
            Alg::EdDsa => Self::Ed(Ed25519KeyPair::generate().unwrap()),
        }
    }

    fn alg(&self) -> &'static str {
        match self {
            Self::Rs(_) => "RS256",
            Self::Es(_) => "ES256",
            Self::Ed(_) => "EdDSA",
        }
    }

    fn jwk(&self, kid: &str) -> Value {
        match self {
            Self::Rs(k) => {
                let (n, e) = rsa_components(k.public_key().as_ref());
                json!({"kty": "RSA", "kid": kid, "use": "sig", "alg": "RS256",
                       "n": b64(n), "e": b64(e)})
            }
            Self::Es(k) => {
                let p = k.public_key().as_ref();
                json!({"kty": "EC", "kid": kid, "use": "sig", "crv": "P-256",
                       "x": b64(&p[1..33]), "y": b64(&p[33..65])})
            }
            Self::Ed(k) => json!({"kty": "OKP", "kid": kid, "crv": "Ed25519",
                                  "x": b64(k.public_key().as_ref())}),
        }
    }

    fn sign(&self, msg: &[u8]) -> Vec<u8> {
        let rng = SystemRandom::new();
        match self {
            Self::Rs(k) => {
                let mut sig = vec![0u8; k.public_modulus_len()];
                k.sign(&RSA_PKCS1_SHA256, &rng, msg, &mut sig).unwrap();
                sig
            }
            Self::Es(k) => k.sign(&rng, msg).unwrap().as_ref().to_vec(),
            Self::Ed(k) => k.sign(msg).as_ref().to_vec(),
        }
    }
}

/// Liest n und e aus einem PKCS#1-`RSAPublicKey` (DER: SEQUENCE { INTEGER n, INTEGER e }).
fn rsa_components(der: &[u8]) -> (&[u8], &[u8]) {
    fn tlv(b: &[u8]) -> (u8, &[u8], &[u8]) {
        let tag = b[0];
        let (len, hdr) = if b[1] < 0x80 {
            (usize::from(b[1]), 2)
        } else {
            let n = usize::from(b[1] & 0x7f);
            let len = b[2..2 + n]
                .iter()
                .fold(0usize, |a, x| (a << 8) | usize::from(*x));
            (len, 2 + n)
        };
        (tag, &b[hdr..hdr + len], &b[hdr + len..])
    }
    let (t, seq, _) = tlv(der);
    assert_eq!(t, 0x30);
    let (t, n, rest) = tlv(seq);
    assert_eq!(t, 0x02);
    let (t, e, _) = tlv(rest);
    assert_eq!(t, 0x02);
    let n = n.strip_prefix(&[0]).unwrap_or(n);
    (n, e)
}

/// Nächster Nutzer, der sich beim IdP „anmeldet“.
#[derive(Debug, Clone, Default)]
struct IdpUser {
    sub: String,
    email: Option<String>,
    email_verified: bool,
    name: Option<String>,
    username: Option<String>,
    groups: Vec<String>,
}

fn user(sub: &str, groups: &[&str]) -> IdpUser {
    IdpUser {
        sub: sub.into(),
        email: Some(format!("{sub}@example.org")),
        email_verified: true,
        name: Some(sub.to_uppercase()),
        username: Some(sub.into()),
        groups: groups.iter().map(|g| (*g).to_owned()).collect(),
    }
}

/// Manipulationen am nächsten ID-Token.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Tamper {
    #[default]
    None,
    WrongAud,
    Expired,
    WrongNonce,
    WrongIss,
    BadSignature,
}

struct Grant {
    nonce: String,
    challenge: String,
    redirect_uri: String,
    user: IdpUser,
}

struct Idp {
    issuer: String,
    signer: Signer,
    /// Fremder Schlüssel mit gleichem `kid` (für „Signatur ungültig“).
    rogue: Signer,
    next_user: Mutex<IdpUser>,
    tamper: Mutex<Tamper>,
    codes: Mutex<HashMap<String, Grant>>,
}

impl Idp {
    async fn start(alg: Alg) -> Arc<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let idp = Arc::new(Self {
            issuer: format!("http://{addr}"),
            signer: Signer::new(alg),
            rogue: Signer::new(alg),
            next_user: Mutex::new(IdpUser::default()),
            tamper: Mutex::new(Tamper::None),
            codes: Mutex::new(HashMap::new()),
        });
        let app = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/jwks", get(jwks))
            .route("/authorize", get(authorize))
            .route("/token", post(token))
            .with_state(Arc::clone(&idp));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        idp
    }

    fn set_user(&self, u: IdpUser) {
        *self.next_user.lock().unwrap() = u;
    }

    fn set_tamper(&self, t: Tamper) {
        *self.tamper.lock().unwrap() = t;
    }

    fn id_token(&self, g: &Grant) -> String {
        let tamper = *self.tamper.lock().unwrap();
        let header = json!({"alg": self.signer.alg(), "kid": "k1", "typ": "JWT"});
        let t = now();
        let mut c = json!({
            "iss": self.issuer, "sub": g.user.sub, "aud": CLIENT_ID, "azp": CLIENT_ID,
            "iat": t, "exp": t + 300, "nonce": g.nonce,
            "groups": g.user.groups, "email_verified": g.user.email_verified,
        });
        if let Some(e) = &g.user.email {
            c["email"] = json!(e);
        }
        if let Some(n) = &g.user.name {
            c["name"] = json!(n);
        }
        if let Some(n) = &g.user.username {
            c["preferred_username"] = json!(n);
        }
        match tamper {
            Tamper::WrongAud => c["aud"] = json!("someone-else"),
            Tamper::Expired => {
                c["iat"] = json!(t - 7200);
                c["exp"] = json!(t - 3600);
            }
            Tamper::WrongNonce => c["nonce"] = json!("other-nonce"),
            Tamper::WrongIss => c["iss"] = json!("http://evil.example"),
            Tamper::None | Tamper::BadSignature => {}
        }
        let input = format!(
            "{}.{}",
            b64(header.to_string().as_bytes()),
            b64(c.to_string().as_bytes())
        );
        let sig = if tamper == Tamper::BadSignature {
            self.rogue.sign(input.as_bytes())
        } else {
            self.signer.sign(input.as_bytes())
        };
        format!("{input}.{}", b64(&sig))
    }
}

async fn discovery(State(idp): State<Arc<Idp>>) -> Json<Value> {
    let i = &idp.issuer;
    Json(json!({
        "issuer": i,
        "authorization_endpoint": format!("{i}/authorize"),
        "token_endpoint": format!("{i}/token"),
        "jwks_uri": format!("{i}/jwks"),
        "end_session_endpoint": format!("{i}/logout"),
        "response_types_supported": ["code"],
        "code_challenge_methods_supported": ["S256"],
        "id_token_signing_alg_values_supported": [idp.signer.alg()],
    }))
}

async fn jwks(State(idp): State<Arc<Idp>>) -> Json<Value> {
    Json(json!({"keys": [idp.signer.jwk("k1")]}))
}

async fn authorize(
    State(idp): State<Arc<Idp>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["client_id"], CLIENT_ID);
    assert_eq!(q["code_challenge_method"], "S256");
    assert!(q["scope"].split(' ').any(|s| s == "openid"));
    let code = b64(&new_id().into_bytes());
    let redirect_uri = q["redirect_uri"].clone();
    idp.codes.lock().unwrap().insert(
        code.clone(),
        Grant {
            nonce: q["nonce"].clone(),
            challenge: q["code_challenge"].clone(),
            redirect_uri: redirect_uri.clone(),
            user: idp.next_user.lock().unwrap().clone(),
        },
    );
    let mut url = url::Url::parse(&redirect_uri).unwrap();
    url.query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", &q["state"]);
    Redirect::to(url.as_str()).into_response()
}

async fn token(State(idp): State<Arc<Idp>>, headers: HeaderMap, body: Bytes) -> Response {
    let form: HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
    let bad = |e: &str| (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
    // client_secret_basic
    let want = format!(
        "Basic {}",
        STANDARD.encode(format!("{CLIENT_ID}:{CLIENT_SECRET}"))
    );
    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some(want.as_str()) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid_client"})),
        )
            .into_response();
    }
    if form.get("grant_type").map(String::as_str) != Some("authorization_code") {
        return bad("unsupported_grant_type");
    }
    let Some(g) = form
        .get("code")
        .and_then(|c| idp.codes.lock().unwrap().remove(c))
    else {
        return bad("invalid_grant");
    };
    if form.get("redirect_uri") != Some(&g.redirect_uri) {
        return bad("invalid_grant");
    }
    // PKCE S256
    let verifier = form.get("code_verifier").cloned().unwrap_or_default();
    let challenge = b64(digest::digest(&digest::SHA256, verifier.as_bytes()).as_ref());
    if challenge != g.challenge {
        return bad("invalid_grant");
    }
    Json(
        json!({"access_token": "at", "token_type": "Bearer", "expires_in": 300,
                "id_token": idp.id_token(&g)}),
    )
    .into_response()
}

// =============================================================================================
// HTTP-Hilfen
// =============================================================================================

struct Resp {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| {
            panic!(
                "kein JSON ({e}), Status {}: {}",
                self.status,
                String::from_utf8_lossy(&self.body)
            )
        })
    }

    fn location(&self) -> String {
        self.headers
            .get("location")
            .unwrap_or_else(|| panic!("Location fehlt (Status {})", self.status))
            .to_str()
            .unwrap()
            .to_owned()
    }

    /// Wert eines gesetzten Cookies (`None`, wenn nicht gesetzt).
    fn cookie(&self, name: &str) -> Option<(String, String)> {
        self.headers
            .get_all("set-cookie")
            .iter()
            .map(|v| v.to_str().unwrap().to_owned())
            .find(|v| v.starts_with(&format!("{name}=")))
            .map(|full| {
                let value = full
                    .split(';')
                    .next()
                    .unwrap()
                    .split_once('=')
                    .unwrap()
                    .1
                    .to_owned();
                (value, full)
            })
    }

    #[track_caller]
    fn expect(self, status: StatusCode) -> Self {
        assert_eq!(
            self.status,
            status,
            "Antwort: {}",
            String::from_utf8_lossy(&self.body)
        );
        self
    }
}

async fn send(method: Method, url: &str, headers: &[(&str, &str)], body: Option<Value>) -> Resp {
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let mut req = Request::builder().method(method).uri(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let body = match body {
        Some(b) => {
            req = req.header("content-type", "application/json");
            Bytes::from(serde_json::to_vec(&b).unwrap())
        }
        None => Bytes::new(),
    };
    let res = client
        .request(req.body(Full::new(body)).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    Resp {
        status,
        headers,
        body,
    }
}

// =============================================================================================
// Gateway (Admin-API) mit OIDC
// =============================================================================================

struct Gw {
    addr: SocketAddr,
    bootstrap: String,
    store: Store,
    db: sqlx::AnyPool,
}

/// Angemeldete Browser-Session.
#[derive(Debug, Clone)]
struct Session {
    cookie: String,
    csrf: String,
    info: Value,
}

fn group_roles() -> Vec<GroupRoleMapping> {
    let m = |group: &str, role, org: Option<&str>, team: Option<&str>| GroupRoleMapping {
        group: group.into(),
        role,
        organization: org.map(Into::into),
        team: team.map(Into::into),
    };
    vec![
        m("aigw-admins", Role::Admin, None, None),
        m("acme-admins", Role::OrgAdmin, Some("acme"), None),
        m(
            "platform-leads",
            Role::TeamAdmin,
            Some("acme"),
            Some("platform"),
        ),
        m("readers", Role::Viewer, None, None),
    ]
}

impl Gw {
    async fn start(store: Store, db_url: &str, idp: &Idp, mode: BootstrapMode) -> Self {
        store.migrate().await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let cfg = AdminAuthConfig {
            bootstrap: mode,
            oidc: OidcConfig {
                issuer: Some(idp.issuer.clone()),
                client_id: CLIENT_ID.into(),
                redirect_url: Some(format!("http://{addr}/admin/auth/callback")),
                group_roles: group_roles(),
                ..OidcConfig::default()
            },
            ..AdminAuthConfig::default()
        };
        let (token, secret) = aigw_admin::BootstrapToken::generate().unwrap();
        let state =
            AdminState::from_config(store.clone(), &cfg, Some(token), Some(CLIENT_SECRET.into()))
                .unwrap();
        assert!(state.oidc_enabled());
        let app = aigw_admin::router_with(state);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        sqlx::any::install_default_drivers();
        let db = sqlx::AnyPool::connect(db_url).await.unwrap();
        Self {
            addr,
            bootstrap: secret,
            store,
            db,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// Anfrage mit Bearer-Token.
    async fn bearer(&self, token: &str, method: Method, path: &str, body: Option<Value>) -> Resp {
        let auth = format!("Bearer {token}");
        send(
            method,
            &self.url(&format!("/admin/api/v1{path}")),
            &[("authorization", &auth)],
            body,
        )
        .await
    }

    /// Anfrage mit Bootstrap-Token.
    async fn boot(&self, method: Method, path: &str, body: Option<Value>) -> Resp {
        self.bearer(&self.bootstrap.clone(), method, path, body)
            .await
    }

    /// Anfrage mit Session-Cookie (und CSRF-Token, falls `csrf`).
    async fn sess(
        &self,
        s: &Session,
        method: Method,
        path: &str,
        body: Option<Value>,
        csrf: bool,
    ) -> Resp {
        let cookie = format!("__Secure-aigw-session={}", s.cookie);
        let mut h = vec![("cookie", cookie.as_str())];
        if csrf {
            h.push(("x-csrf-token", s.csrf.as_str()));
        }
        send(method, &self.url(&format!("/admin/api/v1{path}")), &h, body).await
    }

    /// Kompletter Login über den IdP. `Err(auth_error)` bei Ablehnung.
    async fn login(&self, idp: &Idp, u: IdpUser) -> Result<Session, String> {
        self.login_to(idp, u, "/admin/keys").await.map(|(s, _)| s)
    }

    async fn login_to(
        &self,
        idp: &Idp,
        u: IdpUser,
        return_to: &str,
    ) -> Result<(Session, String), String> {
        idp.set_user(u);
        let r = send(
            Method::GET,
            &self.url(&format!(
                "/admin/auth/login?returnTo={}",
                url::form_urlencoded::byte_serialize(return_to.as_bytes()).collect::<String>()
            )),
            &[],
            None,
        )
        .await
        .expect(StatusCode::SEE_OTHER);
        let (login_cookie, full) = r.cookie("__Secure-aigw-login").expect("Login-Cookie");
        for part in ["HttpOnly", "Secure", "SameSite=Lax", "Path=/admin/auth"] {
            assert!(full.contains(part), "{full}");
        }
        let to_idp = r.location();
        assert!(to_idp.starts_with(&idp.issuer), "{to_idp}");
        assert!(to_idp.contains("code_challenge="));
        // Browser beim IdP → Rücksprung zum Gateway
        let back = send(Method::GET, &to_idp, &[], None).await.location();
        assert!(
            back.starts_with(&self.url("/admin/auth/callback?")),
            "{back}"
        );
        let cb = send(
            Method::GET,
            &back,
            &[("cookie", &format!("__Secure-aigw-login={login_cookie}"))],
            None,
        )
        .await
        .expect(StatusCode::SEE_OTHER);
        // Login-Cookie wird immer gelöscht
        assert!(
            cb.cookie("__Secure-aigw-login")
                .unwrap()
                .1
                .contains("Max-Age=0")
        );
        let loc = cb.location();
        if let Some(code) = loc.strip_prefix("/admin/?auth_error=") {
            assert!(cb.cookie("__Secure-aigw-session").is_none());
            return Err(code.to_owned());
        }
        let (sid, full) = cb.cookie("__Secure-aigw-session").expect("Session-Cookie");
        for part in [
            "HttpOnly",
            "Secure",
            "SameSite=Strict",
            "Path=/admin;",
            "Max-Age=28800",
        ] {
            assert!(full.contains(part), "{full}");
        }
        let mut s = Session {
            cookie: sid,
            csrf: String::new(),
            info: Value::Null,
        };
        let info = self
            .sess(&s, Method::GET, "/session", None, false)
            .await
            .expect(StatusCode::OK)
            .json();
        s.csrf = info["csrfToken"].as_str().unwrap().to_owned();
        s.info = info;
        Ok((s, loc))
    }

    async fn sql(&self, sql: &str) {
        sqlx::query(AssertSqlSafe(sql.to_owned()))
            .execute(&self.db)
            .await
            .unwrap();
    }
}

fn names(v: &Value) -> Vec<String> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            i["metadata"]["name"]
                .as_str()
                .or_else(|| i["name"].as_str())
                .unwrap()
                .to_owned()
        })
        .collect()
}

fn org(name: &str) -> Value {
    json!({"metadata": {"name": name}, "spec": {}})
}

fn team(name: &str, org: &str) -> Value {
    json!({"metadata": {"name": name}, "spec": {"organization": org}})
}

fn key(name: &str, team: Option<&str>) -> Value {
    match team {
        Some(t) => json!({"metadata": {"name": name}, "spec": {"team": t}}),
        None => json!({"metadata": {"name": name}, "spec": {}}),
    }
}

// =============================================================================================
// Szenario
// =============================================================================================

async fn scenario(store: Store, db_url: &str) {
    let idp = Idp::start(Alg::EdDsa).await;
    let gw = Gw::start(store, db_url, &idp, BootstrapMode::Auto).await;

    // --- Grunddaten per Bootstrap-Token ------------------------------------------------------
    for o in ["acme", "other"] {
        gw.boot(Method::POST, "/organizations", Some(org(o)))
            .await
            .expect(StatusCode::CREATED);
    }
    for (t, o) in [("platform", "acme"), ("data", "acme"), ("ext", "other")] {
        gw.boot(Method::POST, "/teams", Some(team(t, o)))
            .await
            .expect(StatusCode::CREATED);
    }
    gw.store
        .create(
            "test",
            ObjectMeta::named("gpt-4o"),
            ModelGroupSpec::default(),
        )
        .await
        .unwrap();
    gw.boot(
        Method::POST,
        "/api-keys",
        Some(key("data-key", Some("data"))),
    )
    .await
    .expect(StatusCode::CREATED);

    // --- Öffentliche Infos, nicht angemeldet --------------------------------------------------
    let info = send(Method::GET, &gw.url("/admin/api/v1/auth/info"), &[], None)
        .await
        .expect(StatusCode::OK)
        .json();
    assert_eq!(info, json!({"oidc": true, "loginUrl": "/admin/auth/login"}));
    send(Method::GET, &gw.url("/admin/api/v1/session"), &[], None)
        .await
        .expect(StatusCode::UNAUTHORIZED);
    let fake = Session {
        cookie: "erfunden".into(),
        csrf: String::new(),
        info: Value::Null,
    };
    gw.sess(&fake, Method::GET, "/teams", None, false)
        .await
        .expect(StatusCode::UNAUTHORIZED);

    // --- Kein Zugang: unbekannter Nutzer ohne Gruppenrolle -----------------------------------
    assert_eq!(
        gw.login(&idp, user("nobody", &["unmapped"]))
            .await
            .unwrap_err(),
        "no-access"
    );
    let users = gw.boot(Method::GET, "/users", None).await.json();
    assert!(names(&users).is_empty(), "kein Nutzer angelegt: {users}");

    // --- team-admin über Gruppe (Nutzer wird beim ersten Login angelegt) ----------------------
    let tina = gw
        .login(&idp, user("tina", &["platform-leads"]))
        .await
        .unwrap();
    assert_eq!(tina.info["subject"], "user:tina");
    let info = send(
        Method::GET,
        &gw.url("/admin/api/v1/auth/info"),
        &[("cookie", &format!("__Secure-aigw-session={}", tina.cookie))],
        None,
    )
    .await
    .expect(StatusCode::OK)
    .json();
    assert_eq!(info["session"], tina.info);
    assert_eq!(tina.info["method"], "session");
    assert_eq!(tina.info["email"], "tina@example.org");
    assert_eq!(
        tina.info["roles"],
        json!([{"role": "team-admin", "organization": "acme", "team": "platform"}])
    );
    let u = gw
        .boot(Method::GET, "/users/tina", None)
        .await
        .expect(StatusCode::OK)
        .json();
    assert_eq!(u["spec"]["oidcIssuer"], idp.issuer.as_str());
    assert_eq!(u["spec"]["oidcSubject"], "tina");
    // Eigenes Team: Keys anlegen (mit CSRF-Token)
    let created = gw
        .sess(
            &tina,
            Method::POST,
            "/api-keys",
            Some(key("tina-key", Some("platform"))),
            true,
        )
        .await
        .expect(StatusCode::CREATED)
        .json();
    assert!(created["secret"].as_str().unwrap().starts_with("sk-aigw-"));
    // Ohne CSRF-Token, mit falschem Token, fremdem Origin oder Cross-Site → 403
    gw.sess(
        &tina,
        Method::POST,
        "/api-keys",
        Some(key("x1", Some("platform"))),
        false,
    )
    .await
    .expect(StatusCode::FORBIDDEN);
    let wrong = Session {
        csrf: "falsch".into(),
        ..tina.clone()
    };
    gw.sess(
        &wrong,
        Method::POST,
        "/api-keys",
        Some(key("x1", Some("platform"))),
        true,
    )
    .await
    .expect(StatusCode::FORBIDDEN);
    let cookie = format!("__Secure-aigw-session={}", tina.cookie);
    for extra in [
        ("origin", "http://evil.example"),
        ("sec-fetch-site", "cross-site"),
    ] {
        send(
            Method::POST,
            &gw.url("/admin/api/v1/api-keys"),
            &[("cookie", &cookie), ("x-csrf-token", &tina.csrf), extra],
            Some(key("x1", Some("platform"))),
        )
        .await
        .expect(StatusCode::FORBIDDEN);
    }
    // Passender Origin ist erlaubt
    let origin = format!("http://{}", gw.addr);
    send(
        Method::POST,
        &gw.url("/admin/api/v1/api-keys/tina-key/revoke"),
        &[
            ("cookie", &cookie),
            ("x-csrf-token", &tina.csrf),
            ("origin", &origin),
            ("sec-fetch-site", "same-origin"),
        ],
        None,
    )
    .await
    .expect(StatusCode::OK);
    // Lesen braucht kein CSRF-Token
    gw.sess(&tina, Method::GET, "/api-keys/tina-key", None, false)
        .await
        .expect(StatusCode::OK);
    // Fremde Teams und Keys ohne Team: 403
    for t in [Some("data"), Some("ext"), None] {
        gw.sess(&tina, Method::POST, "/api-keys", Some(key("x2", t)), true)
            .await
            .expect(StatusCode::FORBIDDEN);
    }
    gw.sess(&tina, Method::GET, "/api-keys/data-key", None, false)
        .await
        .expect(StatusCode::FORBIDDEN);
    for p in ["/api-keys/data-key/revoke", "/api-keys/data-key/rotate"] {
        gw.sess(&tina, Method::POST, p, None, true)
            .await
            .expect(StatusCode::FORBIDDEN);
    }
    // Listen zeigen nur den eigenen Bereich
    let keys = gw
        .sess(&tina, Method::GET, "/api-keys", None, false)
        .await
        .json();
    assert_eq!(names(&keys), ["tina-key"]);
    let teams = gw
        .sess(&tina, Method::GET, "/teams", None, false)
        .await
        .json();
    assert_eq!(names(&teams), ["platform"]);
    let orgs = gw
        .sess(&tina, Method::GET, "/organizations", None, false)
        .await
        .json();
    assert_eq!(names(&orgs), ["acme"]);
    let users = gw
        .sess(&tina, Method::GET, "/users", None, false)
        .await
        .json();
    assert!(names(&users).is_empty());
    gw.sess(&tina, Method::GET, "/model-groups", None, false)
        .await
        .expect(StatusCode::OK);
    // Team selbst, Organisationen, Nutzer, Admin-Keys: kein Schreibrecht
    gw.sess(
        &tina,
        Method::PUT,
        "/teams/platform",
        Some(team("platform", "acme")),
        true,
    )
    .await
    .expect(StatusCode::FORBIDDEN);
    gw.sess(
        &tina,
        Method::POST,
        "/organizations",
        Some(org("neu")),
        true,
    )
    .await
    .expect(StatusCode::FORBIDDEN);
    gw.sess(&tina, Method::GET, "/admin-api-keys", None, false)
        .await
        .expect(StatusCode::FORBIDDEN);
    gw.sess(
        &tina,
        Method::POST,
        "/admin-api-keys",
        Some(json!({"name": "x", "roles": [{"role": "admin"}]})),
        true,
    )
    .await
    .expect(StatusCode::FORBIDDEN);

    // --- org-admin über Gruppe ----------------------------------------------------------------
    let olga = gw
        .login(&idp, user("olga", &["acme-admins"]))
        .await
        .unwrap();
    gw.sess(
        &olga,
        Method::POST,
        "/teams",
        Some(team("ml", "acme")),
        true,
    )
    .await
    .expect(StatusCode::CREATED);
    gw.sess(
        &olga,
        Method::POST,
        "/teams",
        Some(team("ml2", "other")),
        true,
    )
    .await
    .expect(StatusCode::FORBIDDEN);
    // Team nicht in fremde Organisation verschieben
    gw.sess(
        &olga,
        Method::PUT,
        "/teams/ml",
        Some(team("ml", "other")),
        true,
    )
    .await
    .expect(StatusCode::FORBIDDEN);
    gw.sess(
        &olga,
        Method::PUT,
        "/organizations/acme",
        Some(org("acme")),
        true,
    )
    .await
    .expect(StatusCode::FORBIDDEN);
    gw.sess(
        &olga,
        Method::POST,
        "/api-keys",
        Some(key("olga-key", Some("data"))),
        true,
    )
    .await
    .expect(StatusCode::CREATED);
    gw.sess(&olga, Method::DELETE, "/teams/ml", None, true)
        .await
        .expect(StatusCode::NO_CONTENT);
    let keys = gw
        .sess(&olga, Method::GET, "/api-keys", None, false)
        .await
        .json();
    assert_eq!(names(&keys), ["data-key", "olga-key", "tina-key"]);

    // Noch kein globaler Admin per OIDC → Bootstrap-Token gilt weiter
    gw.boot(Method::GET, "/teams", None)
        .await
        .expect(StatusCode::OK);

    // --- direkte Rolle in der DB, Verknüpfung über bestätigte E-Mail --------------------------
    gw.boot(
        Method::POST,
        "/users",
        Some(json!({"metadata": {"name": "vera"},
                    "spec": {"email": "Vera@Example.org", "roles": [{"role": "viewer"}]}})),
    )
    .await
    .expect(StatusCode::CREATED);
    // unbestätigte E-Mail verknüpft nicht
    let mut v = user("vera-idp", &[]);
    v.email = Some("vera@example.org".into());
    v.email_verified = false;
    assert_eq!(gw.login(&idp, v.clone()).await.unwrap_err(), "no-access");
    v.email_verified = true;
    let vera = gw.login(&idp, v).await.unwrap();
    assert_eq!(vera.info["subject"], "user:vera");
    assert_eq!(vera.info["roles"], json!([{"role": "viewer"}]));
    let u = gw.boot(Method::GET, "/users/vera", None).await.json();
    assert_eq!(u["spec"]["oidcSubject"], "vera-idp");
    let teams = gw
        .sess(&vera, Method::GET, "/teams", None, false)
        .await
        .json();
    assert_eq!(names(&teams), ["data", "ext", "platform"]);
    gw.sess(
        &vera,
        Method::POST,
        "/organizations",
        Some(org("neu")),
        true,
    )
    .await
    .expect(StatusCode::FORBIDDEN);
    // Rollenänderung in der DB beendet die laufenden Sessions des Nutzers (AIGW-66)
    let vera_spec = |roles: Value| {
        json!({"metadata": {"name": "vera"},
               "spec": {"email": "Vera@Example.org", "oidcIssuer": idp.issuer,
                        "oidcSubject": "vera-idp", "roles": roles}})
    };
    gw.boot(Method::PUT, "/users/vera", Some(vera_spec(json!([]))))
        .await
        .expect(StatusCode::OK);
    gw.sess(&vera, Method::GET, "/teams", None, false)
        .await
        .expect(StatusCode::UNAUTHORIZED);
    // ohne Rolle kein neuer Login; mit Rolle wieder
    let mut v = user("vera-idp", &[]);
    v.email = Some("vera@example.org".into());
    assert_eq!(gw.login(&idp, v.clone()).await.unwrap_err(), "no-access");
    gw.boot(
        Method::PUT,
        "/users/vera",
        Some(vera_spec(json!([{"role": "viewer"}]))),
    )
    .await
    .expect(StatusCode::OK);
    let vera = gw.login(&idp, v).await.unwrap();
    assert_eq!(vera.info["roles"], json!([{"role": "viewer"}]));
    // Änderung ohne Rollen/Identität (E-Mail) lässt die Session bestehen
    let mut same = vera_spec(json!([{"role": "viewer"}]));
    same["spec"]["email"] = json!("vera@example.org");
    gw.boot(Method::PUT, "/users/vera", Some(same))
        .await
        .expect(StatusCode::OK);
    gw.sess(&vera, Method::GET, "/session", None, false)
        .await
        .expect(StatusCode::OK);

    // --- globaler Admin per OIDC schaltet das Bootstrap-Token ab ------------------------------
    let (adam, loc) = gw
        .login_to(
            &idp,
            user("adam", &["aigw-admins", "readers"]),
            "https://evil.example/",
        )
        .await
        .unwrap();
    assert_eq!(loc, "/admin/", "kein offener Redirect");
    gw.boot(Method::GET, "/teams", None)
        .await
        .expect(StatusCode::UNAUTHORIZED);
    gw.sess(
        &adam,
        Method::POST,
        "/organizations",
        Some(org("neu")),
        true,
    )
    .await
    .expect(StatusCode::CREATED);
    // Audit-Akteur = Nutzer
    let log = gw.store.audit_log(50).await.unwrap();
    assert!(
        log.iter()
            .any(|e| e.actor == "user:adam" && e.object_name.as_deref() == Some("neu"))
    );
    assert!(log.iter().any(|e| e.actor == "user:tina"));
    assert!(
        log.iter()
            .any(|e| e.actor == "oidc-login" && e.object_name.as_deref() == Some("tina"))
    );

    // --- Admin-API-Keys -------------------------------------------------------------------------
    let ak = |name: &str, roles: Value| json!({"name": name, "description": "CI", "roles": roles});
    for (body, status) in [
        (ak("bad", json!([])), StatusCode::UNPROCESSABLE_ENTITY),
        (
            ak("bad", json!([{"role": "team-admin"}])),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            ak("bad", json!([{"role": "team-admin", "team": "nope"}])),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            ak(
                "bad",
                json!([{"role": "team-admin", "organization": "other", "team": "platform"}]),
            ),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            ak("Bad Name", json!([{"role": "viewer"}])),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            json!({"name": "bad", "roles": [{"role": "viewer"}], "expiresAt": "2001-01-01T00:00:00Z"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            json!({"name": "bad", "roles": [{"role": "viewer"}], "typo": 1}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        let r = gw
            .sess(
                &adam,
                Method::POST,
                "/admin-api-keys",
                Some(body.clone()),
                true,
            )
            .await;
        assert!(
            r.status == status || r.status == StatusCode::BAD_REQUEST,
            "{body}: {} {}",
            r.status,
            String::from_utf8_lossy(&r.body)
        );
    }
    let r = gw
        .sess(
            &adam,
            Method::POST,
            "/admin-api-keys",
            Some(ak("ci", json!([{"role": "viewer"}]))),
            true,
        )
        .await
        .expect(StatusCode::CREATED);
    assert_eq!(r.headers["cache-control"], "no-store");
    let ci = r.json();
    let ci_secret = ci["secret"].as_str().unwrap().to_owned();
    assert!(ci_secret.starts_with("sk-aigw-admin-"));
    assert_eq!(ci["key"]["createdBy"], "user:adam");
    assert!(ci["key"].get("keyHash").is_none());
    gw.sess(
        &adam,
        Method::POST,
        "/admin-api-keys",
        Some(ak("ci", json!([{"role": "viewer"}]))),
        true,
    )
    .await
    .expect(StatusCode::CONFLICT);
    // Viewer-Key: lesen ja, schreiben nein
    gw.bearer(&ci_secret, Method::GET, "/teams", None)
        .await
        .expect(StatusCode::OK);
    let me = gw
        .bearer(&ci_secret, Method::GET, "/session", None)
        .await
        .json();
    assert_eq!(me["subject"], "admin-key:ci");
    assert_eq!(me["method"], "admin-api-key");
    assert!(me.get("csrfToken").is_none());
    gw.bearer(&ci_secret, Method::POST, "/organizations", Some(org("x")))
        .await
        .expect(StatusCode::FORBIDDEN);
    let list = gw
        .bearer(&ci_secret, Method::GET, "/admin-api-keys", None)
        .await
        .json();
    assert_eq!(names(&list), ["ci"]);
    assert!(list["items"][0]["lastUsedAt"].is_string());
    // Team-Key: Schreibrecht im eigenen Team, ohne CSRF (Bearer)
    let tk = gw
        .sess(
            &adam,
            Method::POST,
            "/admin-api-keys",
            Some(ak(
                "ci-platform",
                json!([{"role": "team-admin", "organization": "acme", "team": "platform"}]),
            )),
            true,
        )
        .await
        .expect(StatusCode::CREATED)
        .json();
    let tk_secret = tk["secret"].as_str().unwrap().to_owned();
    gw.bearer(
        &tk_secret,
        Method::POST,
        "/api-keys",
        Some(key("auto-1", Some("platform"))),
    )
    .await
    .expect(StatusCode::CREATED);
    gw.bearer(
        &tk_secret,
        Method::POST,
        "/api-keys",
        Some(key("auto-2", Some("ext"))),
    )
    .await
    .expect(StatusCode::FORBIDDEN);
    // Ändern (Rollen ersetzen) und Ablauf
    gw.sess(
        &adam,
        Method::PUT,
        "/admin-api-keys/ci-platform",
        Some(json!({"roles": [{"role": "viewer"}], "expiresAt": "2099-01-01T00:00:00Z"})),
        true,
    )
    .await
    .expect(StatusCode::OK);
    gw.bearer(
        &tk_secret,
        Method::POST,
        "/api-keys",
        Some(key("auto-3", Some("platform"))),
    )
    .await
    .expect(StatusCode::FORBIDDEN);
    gw.sql("UPDATE admin_api_key SET expires_at = 1000 WHERE name = 'ci-platform'")
        .await;
    gw.bearer(&tk_secret, Method::GET, "/teams", None)
        .await
        .expect(StatusCode::UNAUTHORIZED);
    // Löschen wirkt sofort
    gw.sess(&adam, Method::DELETE, "/admin-api-keys/ci", None, true)
        .await
        .expect(StatusCode::NO_CONTENT);
    gw.bearer(&ci_secret, Method::GET, "/teams", None)
        .await
        .expect(StatusCode::UNAUTHORIZED);
    gw.sess(&adam, Method::DELETE, "/admin-api-keys/ci", None, true)
        .await
        .expect(StatusCode::NOT_FOUND);
    // Ein Admin-Key ist kein Key der Datenebene und umgekehrt
    gw.bearer(
        created["secret"].as_str().unwrap(),
        Method::GET,
        "/teams",
        None,
    )
    .await
    .expect(StatusCode::UNAUTHORIZED);
    let log = gw.store.audit_log(100).await.unwrap();
    assert_eq!(
        log.iter()
            .filter(|e| e.kind == "admin-api-key" && e.action != "first-use")
            .count(),
        4,
        "create ci, create ci-platform, update ci-platform, delete ci"
    );
    // Erstnutzung genau einmal je Key (AIGW-66)
    let mut first: Vec<&str> = log
        .iter()
        .filter(|e| e.kind == "admin-api-key" && e.action == "first-use")
        .map(|e| e.actor.as_str())
        .collect();
    first.sort_unstable();
    assert_eq!(first, ["admin-key:ci", "admin-key:ci-platform"]);

    // --- Ablauf und Leerlauf der Sessions -------------------------------------------------------
    gw.sql("UPDATE admin_session SET last_seen_at = 1000 WHERE user_id = (SELECT id FROM app_user WHERE name = 'tina')")
        .await;
    gw.sess(&tina, Method::GET, "/teams", None, false)
        .await
        .expect(StatusCode::UNAUTHORIZED);
    gw.sql("UPDATE admin_session SET expires_at = 1000 WHERE user_id = (SELECT id FROM app_user WHERE name = 'olga')")
        .await;
    gw.sess(&olga, Method::GET, "/teams", None, false)
        .await
        .expect(StatusCode::UNAUTHORIZED);
    // abgelaufene Sessions sind gelöscht
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM admin_session s JOIN app_user u ON u.id = s.user_id WHERE u.name IN ('tina', 'olga')",
    )
    .fetch_one(&gw.db)
    .await
    .unwrap();
    assert_eq!(n, 0);
    // Nutzer löschen beendet seine Sessions
    gw.sess(&vera, Method::GET, "/session", None, false)
        .await
        .expect(StatusCode::OK);
    gw.sess(&adam, Method::DELETE, "/users/vera", None, true)
        .await
        .expect(StatusCode::NO_CONTENT);
    gw.sess(&vera, Method::GET, "/session", None, false)
        .await
        .expect(StatusCode::UNAUTHORIZED);

    // --- Abmeldung (lokal + RP-initiated) -------------------------------------------------------
    let adam_cookie = format!("__Secure-aigw-session={}", adam.cookie);
    send(
        Method::POST,
        &gw.url("/admin/auth/logout"),
        &[("cookie", &adam_cookie)],
        None,
    )
    .await
    .expect(StatusCode::FORBIDDEN);
    let r = send(
        Method::POST,
        &gw.url("/admin/auth/logout"),
        &[("cookie", &adam_cookie), ("x-csrf-token", &adam.csrf)],
        None,
    )
    .await
    .expect(StatusCode::OK);
    assert!(
        r.cookie("__Secure-aigw-session")
            .unwrap()
            .1
            .contains("Max-Age=0")
    );
    let redirect = r.json()["redirectUrl"].as_str().unwrap().to_owned();
    assert!(
        redirect.starts_with(&format!("{}/logout?", idp.issuer)),
        "{redirect}"
    );
    assert!(redirect.contains(&format!("client_id={CLIENT_ID}")));
    assert!(redirect.contains("post_logout_redirect_uri="));
    gw.sess(&adam, Method::GET, "/session", None, false)
        .await
        .expect(StatusCode::UNAUTHORIZED);

    // --- Abgelehnte Logins ----------------------------------------------------------------------
    for (t, why) in [
        (Tamper::WrongAud, "aud"),
        (Tamper::Expired, "exp"),
        (Tamper::WrongNonce, "nonce"),
        (Tamper::WrongIss, "iss"),
        (Tamper::BadSignature, "Signatur"),
    ] {
        idp.set_tamper(t);
        assert_eq!(
            gw.login(&idp, user("adam", &["aigw-admins"]))
                .await
                .unwrap_err(),
            "token",
            "{why}"
        );
    }
    idp.set_tamper(Tamper::None);
    // state: ohne bzw. mit fremdem Login-Cookie, Wiederholung des Rücksprungs
    idp.set_user(user("adam", &["aigw-admins"]));
    let r = send(Method::GET, &gw.url("/admin/auth/login"), &[], None).await;
    let (lc, _) = r.cookie("__Secure-aigw-login").unwrap();
    let back = send(Method::GET, &r.location(), &[], None).await.location();
    for cookie in [String::new(), "__Secure-aigw-login=fremd".to_owned()] {
        let h: Vec<(&str, &str)> = if cookie.is_empty() {
            vec![]
        } else {
            vec![("cookie", cookie.as_str())]
        };
        let cb = send(Method::GET, &back, &h, None)
            .await
            .expect(StatusCode::SEE_OTHER);
        assert_eq!(cb.location(), "/admin/?auth_error=state");
    }
    let good_cookie = format!("__Secure-aigw-login={lc}");
    let cb = send(Method::GET, &back, &[("cookie", &good_cookie)], None).await;
    assert_eq!(cb.location(), "/admin/", "Standardziel");
    let cb = send(Method::GET, &back, &[("cookie", &good_cookie)], None).await;
    assert_eq!(
        cb.location(),
        "/admin/?auth_error=state",
        "state nur einmal gültig"
    );
    // Fehler vom IdP
    let cb = send(
        Method::GET,
        &gw.url("/admin/auth/callback?error=access_denied&state=x"),
        &[],
        None,
    )
    .await;
    assert_eq!(cb.location(), "/admin/?auth_error=idp");

    // --- Audit-Ereignisse der Anmeldung (AIGW-66) ---------------------------------------------
    let events = gw
        .store
        .query_audit_log(&AuditQuery {
            kinds: vec!["admin-session".into()],
            limit: 1000,
            ..AuditQuery::default()
        })
        .await
        .unwrap()
        .items;
    let count = |action: &str, actor: &str| {
        events
            .iter()
            .filter(|e| e.action == action && e.actor == actor)
            .count()
    };
    assert!(count("login", "user:adam") >= 2, "{events:#?}");
    assert_eq!(count("logout", "user:adam"), 1);
    assert!(count("login", "user:tina") >= 1);
    let login = events
        .iter()
        .find(|e| e.action == "login" && e.actor == "user:adam")
        .unwrap();
    assert_eq!(
        login.after.as_ref().unwrap()["roles"],
        json!([{"role": "admin"}])
    );
    // abgelehnte Logins nach gültigem state (Nutzer ohne Rolle, ungültige Tokens) stehen im Log,
    // solche ohne gültigen state (fremdes Cookie, Wiederholung, IdP-Fehler) nicht
    let failed: Vec<_> = events
        .iter()
        .filter(|e| e.action == "login-failed")
        .map(|e| e.after.clone().unwrap())
        .collect();
    let codes = |c: &str| failed.iter().filter(|f| f["code"] == c).count();
    assert_eq!(codes("token"), 5, "{failed:#?}");
    assert_eq!(codes("no-access"), 3, "{failed:#?}");
    assert_eq!(codes("state") + codes("idp"), 0, "{failed:#?}");
    assert!(events.iter().any(|e| e.action == "login-failed"
        && e.actor == "oidc-login"
        && e.object_name.as_deref() == Some("vera")));
    // Rollenänderung und Löschen beenden Sessions – mit Grund im Log
    let revoked: Vec<_> = events
        .iter()
        .filter(|e| e.action == "revoke" && e.object_name.as_deref() == Some("vera"))
        .map(|e| e.after.clone().unwrap()["reason"].clone())
        .collect();
    assert_eq!(revoked, vec![json!("user-deleted"), json!("roles-changed")]);
    // Bootstrap-Abschaltung
    let boot = gw
        .store
        .query_audit_log(&AuditQuery {
            kinds: vec!["bootstrap-token".into()],
            limit: 10,
            ..AuditQuery::default()
        })
        .await
        .unwrap()
        .items;
    assert_eq!(boot.len(), 1);
    assert_eq!(
        (boot[0].action.as_str(), boot[0].actor.as_str()),
        ("disable", "system")
    );
    assert_eq!(boot[0].after.as_ref().unwrap()["reason"], "oidc-admin:adam");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oidc_login_sqlite() {
    let dir = std::env::temp_dir().join(format!("aigw-oidc-test-{}", new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let url = format!("sqlite://{}/aigw.db?mode=rwc", dir.display());
    scenario(Store::connect(&url).await.unwrap(), &url).await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oidc_login_postgres() {
    let Ok(base) = std::env::var("AIGW_TEST_POSTGRES_URL") else {
        eprintln!("AIGW_TEST_POSTGRES_URL nicht gesetzt – Postgres-Test übersprungen");
        return;
    };
    let db = format!("aigw_oidc_test_{}", new_id().simple());
    let admin = sqlx::PgPool::connect(&base).await.unwrap();
    sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {db}")))
        .execute(&admin)
        .await
        .unwrap();
    let (prefix, _) = base.rsplit_once('/').expect("URL mit Datenbankpfad");
    let url = format!("{prefix}/{db}");
    let store = Store::connect(&url).await.unwrap();
    scenario(store.clone(), &url).await;
    store.close().await;
    sqlx::query(AssertSqlSafe(format!("DROP DATABASE {db} WITH (FORCE)")))
        .execute(&admin)
        .await
        .unwrap();
}

/// Alle drei Signaturverfahren mit vollständigem Login.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_signature_algorithms() {
    for alg in [Alg::Rs256, Alg::Es256, Alg::EdDsa] {
        let dir = std::env::temp_dir().join(format!("aigw-oidc-alg-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("sqlite://{}/aigw.db?mode=rwc", dir.display());
        let idp = Idp::start(alg).await;
        let gw = Gw::start(
            Store::connect(&url).await.unwrap(),
            &url,
            &idp,
            BootstrapMode::On,
        )
        .await;
        let s = gw
            .login(&idp, user("anna", &["readers"]))
            .await
            .unwrap_or_else(|e| panic!("{alg:?}: {e}"));
        assert_eq!(s.info["roles"], json!([{"role": "viewer"}]), "{alg:?}");
        idp.set_tamper(Tamper::BadSignature);
        assert_eq!(
            gw.login(&idp, user("anna", &["readers"]))
                .await
                .unwrap_err(),
            "token",
            "{alg:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Bootstrap-Modi: `off` sofort aus, `on` bleibt an, `auto` aus nach Admin-API-Key mit `admin`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_modes() {
    let idp = Idp::start(Alg::EdDsa).await;
    for mode in [BootstrapMode::Off, BootstrapMode::On, BootstrapMode::Auto] {
        let dir = std::env::temp_dir().join(format!("aigw-oidc-boot-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("sqlite://{}/aigw.db?mode=rwc", dir.display());
        let gw = Gw::start(Store::connect(&url).await.unwrap(), &url, &idp, mode).await;
        let r = gw.boot(Method::GET, "/teams", None).await;
        if mode == BootstrapMode::Off {
            r.expect(StatusCode::UNAUTHORIZED);
            continue;
        }
        r.expect(StatusCode::OK);
        // Viewer-Key schaltet nicht ab …
        gw.boot(
            Method::POST,
            "/admin-api-keys",
            Some(json!({"name": "viewer", "roles": [{"role": "viewer"}]})),
        )
        .await
        .expect(StatusCode::CREATED);
        gw.boot(Method::GET, "/teams", None)
            .await
            .expect(StatusCode::OK);
        // … ein Admin-Key schon (nur im Modus auto).
        let k = gw
            .boot(
                Method::POST,
                "/admin-api-keys",
                Some(json!({"name": "root", "roles": [{"role": "admin"}]})),
            )
            .await
            .expect(StatusCode::CREATED)
            .json();
        let want = if mode == BootstrapMode::Auto {
            StatusCode::UNAUTHORIZED
        } else {
            StatusCode::OK
        };
        gw.boot(Method::GET, "/teams", None).await.expect(want);
        assert!(gw.store.bootstrap_disabled().await.unwrap());
        // Der Admin-Key arbeitet weiter.
        gw.bearer(
            k["secret"].as_str().unwrap(),
            Method::POST,
            "/organizations",
            Some(org("o1")),
        )
        .await
        .expect(StatusCode::CREATED);
        let _ = std::fs::remove_dir_all(dir);
    }
}
