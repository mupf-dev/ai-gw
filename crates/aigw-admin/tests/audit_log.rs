// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Integrationstests Audit-Log-Endpunkt (AIGW-66) mit echtem Server und echter Datenbank:
//! Filter und Paginierung, Bereichsfilter je Rolle, Redaktion von Geheimnissen, NDJSON-Export,
//! Erstnutzung von Admin-API-Keys, Sessions enden bei Rollenänderung und Löschen, Logout.
//!
//! Läuft gegen SQLite (Datei) und – mit `AIGW_TEST_POSTGRES_URL` bzw. `scripts/check.sh` mit
//! `AIGW_TEST_POSTGRES=docker` – gegen Postgres.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use aigw_admin::admin_keys::AdminKeyAuthenticator;
use aigw_admin::session::SessionAuthenticator;
use aigw_admin::{AdminState, AuthChain, BootstrapToken};
use aigw_core::config::SessionConfig;
use aigw_core::resource::{Id, new_id};
use aigw_store::{Keyring, MasterKey, Store, hash_api_key};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{HeaderMap, Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};
use sqlx::AssertSqlSafe;
use time::OffsetDateTime;

const UPSTREAM_1: &str = "sk-upstream-GEHEIM-eins-123";
const UPSTREAM_2: &str = "sk-upstream-GEHEIM-zwei-456";

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

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    #[track_caller]
    fn expect(self, status: StatusCode) -> Self {
        assert_eq!(self.status, status, "Antwort: {}", self.text());
        self
    }
}

struct Env {
    addr: SocketAddr,
    token: String,
    store: Store,
}

impl Env {
    async fn start(store: Store) -> Self {
        store.migrate().await.unwrap();
        let keyring = Arc::new(Keyring::new(MasterKey::generate().unwrap().0));
        let (token, secret) = BootstrapToken::generate().unwrap();
        let chain = AuthChain::new()
            .bearer(Arc::new(AdminKeyAuthenticator::new(store.clone())))
            .bearer(Arc::new(token))
            .session(Arc::new(SessionAuthenticator::new(
                store.clone(),
                SessionConfig::default(),
                Vec::new(),
            )));
        let state = AdminState::new(store.clone(), Arc::new(chain)).with_keyring(keyring);
        let app = aigw_admin::router_with(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            addr,
            token: secret,
            store,
        }
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> Resp {
        let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
        let mut req = Request::builder()
            .method(method)
            .uri(format!("http://{}{path}", self.addr));
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
            .request(with_if_match(req).body(Full::new(body)).unwrap())
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

    /// Admin-API mit Bearer-Token.
    async fn as_(&self, token: &str, method: Method, path: &str, body: Option<Value>) -> Resp {
        let auth = format!("Bearer {token}");
        self.send(
            method,
            &format!("/admin/api/v1{path}"),
            &[("authorization", &auth)],
            body,
        )
        .await
    }

    /// Admin-API mit Bootstrap-Token.
    async fn boot(&self, method: Method, path: &str, body: Option<Value>) -> Resp {
        let t = self.token.clone();
        self.as_(&t, method, path, body).await
    }

    /// Admin-API mit Session-Cookie.
    async fn sess(&self, sid: &str, method: Method, path: &str) -> Resp {
        let cookie = format!("__Host-aigw-session={sid}");
        self.send(
            method,
            &format!("/admin/api/v1{path}"),
            &[("cookie", &cookie)],
            None,
        )
        .await
    }

    /// Legt eine Session direkt im Speicher an (ohne IdP; der Login selbst ist in
    /// `oidc_login.rs` getestet).
    async fn session_for(&self, user_id: Id) -> String {
        let sid = format!("sid-{}", new_id().simple());
        self.store
            .create_admin_session(
                &hash_api_key(&sid),
                user_id,
                &[],
                OffsetDateTime::now_utc() + time::Duration::hours(1),
            )
            .await
            .unwrap();
        sid
    }

    /// Admin-API-Key mit Rollen; liefert den Klartext.
    async fn admin_key(&self, name: &str, roles: Value) -> String {
        self.boot(
            Method::POST,
            "/admin-api-keys",
            Some(json!({"name": name, "roles": roles})),
        )
        .await
        .expect(StatusCode::CREATED)
        .json()["secret"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// Alle Einträge über die Paginierung (Seitengröße `limit`).
    async fn all(&self, token: &str, query: &str, limit: u32) -> Vec<Value> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut path = format!("/audit-log?limit={limit}");
            if !query.is_empty() {
                path.push('&');
                path.push_str(query);
            }
            if let Some(c) = &cursor {
                path.push_str(&format!("&cursor={c}"));
            }
            let page = self
                .as_(token, Method::GET, &path, None)
                .await
                .expect(StatusCode::OK)
                .json();
            let items = page["items"].as_array().unwrap();
            assert!(items.len() <= limit as usize);
            out.extend(items.iter().cloned());
            match page["nextCursor"].as_str() {
                Some(c) => {
                    assert_eq!(items.len(), limit as usize, "volle Seite vor nextCursor");
                    cursor = Some(c.to_owned());
                }
                None => break,
            }
        }
        out
    }
}

fn org(name: &str) -> Value {
    json!({"metadata": {"name": name}, "spec": {}})
}

fn team(name: &str, org: &str) -> Value {
    json!({"metadata": {"name": name}, "spec": {"organization": org}})
}

/// Key im Team (F3: Organisation = die des Teams).
fn key(name: &str, team: &str) -> Value {
    let org = if team == "ops" { "other" } else { "acme" };
    json!({"metadata": {"name": name}, "spec": {"organization": org, "team": team}})
}

fn field<'a>(e: &'a Value, f: &str) -> &'a str {
    e[f].as_str().unwrap_or("")
}

async fn scenario(store: Store) {
    let env = Env::start(store).await;

    // --- Ausgangslage per Bootstrap-Token ------------------------------------------------------
    for o in ["acme", "other"] {
        env.boot(Method::POST, "/organizations", Some(org(o)))
            .await
            .expect(StatusCode::CREATED);
    }
    for (t, o) in [("platform", "acme"), ("data", "acme"), ("ops", "other")] {
        env.boot(Method::POST, "/teams", Some(team(t, o)))
            .await
            .expect(StatusCode::CREATED);
    }
    env.boot(
        Method::POST,
        "/provider-credentials",
        Some(json!({"metadata": {"name": "openai-prod"},
                    "spec": {"provider": "openai", "data": {"apiKey": {"value": UPSTREAM_1}}}})),
    )
    .await
    .expect(StatusCode::CREATED);
    let r = env
        .send(
            Method::PATCH,
            "/admin/api/v1/provider-credentials/openai-prod",
            &[("authorization", &format!("Bearer {}", env.token))],
            Some(json!({"spec": {"data": {"apiKey": {"value": UPSTREAM_2}}}})),
        )
        .await;
    r.expect(StatusCode::OK);
    let k1 = env
        .boot(Method::POST, "/api-keys", Some(key("k1", "platform")))
        .await
        .expect(StatusCode::CREATED)
        .json();
    let k1_secret = k1["secret"].as_str().unwrap().to_owned();
    let rotated = env
        .boot(
            Method::POST,
            "/api-keys/k1/rotate",
            Some(json!({"name": "k1-neu", "gracePeriodSeconds": 0})),
        )
        .await
        .expect(StatusCode::CREATED)
        .json();
    let k1_new_secret = rotated["newKey"]["secret"].as_str().unwrap().to_owned();
    env.boot(Method::POST, "/api-keys", Some(key("k2", "ops")))
        .await
        .expect(StatusCode::CREATED);
    env.boot(Method::POST, "/api-keys/k2/revoke", None)
        .await
        .expect(StatusCode::OK);
    // Key k3 wandert von acme/data nach other/ops (bleibt für acme sichtbar)
    env.boot(Method::POST, "/api-keys", Some(key("k3", "data")))
        .await
        .expect(StatusCode::CREATED);
    env.boot(Method::PUT, "/api-keys/k3", Some(key("k3", "ops")))
        .await
        .expect(StatusCode::OK);

    let auditor = env.admin_key("auditor", json!([{"role": "auditor"}])).await;
    let viewer = env.admin_key("viewer", json!([{"role": "viewer"}])).await;
    let org_admin = env
        .admin_key(
            "acme-admin",
            json!([{"role": "org-admin", "organization": "acme"}]),
        )
        .await;
    let team_admin = env
        .admin_key(
            "platform-admin",
            json!([{"role": "team-admin", "organization": "acme", "team": "platform"}]),
        )
        .await;
    let scoped_auditor = env
        .admin_key(
            "other-auditor",
            json!([{"role": "auditor", "organization": "other"}]),
        )
        .await;
    let secrets = [
        UPSTREAM_1,
        UPSTREAM_2,
        &k1_secret,
        &k1_new_secret,
        &auditor,
        &viewer,
        &org_admin,
        &team_admin,
        &scoped_auditor,
    ];

    // --- Berechtigung ---------------------------------------------------------------------------
    let r = env.as_(&viewer, Method::GET, "/audit-log", None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.text());
    assert_eq!(r.json()["code"], "forbidden");
    env.send(Method::GET, "/admin/api/v1/audit-log", &[], None)
        .await
        .expect(StatusCode::UNAUTHORIZED);

    // --- Paginierung: neueste zuerst, lückenlos, gleiche Menge bei jeder Seitengröße ----------
    let full = env.all(&env.token.clone(), "", 500).await;
    let paged = env.all(&env.token.clone(), "", 3).await;
    let ids =
        |v: &[Value]| -> Vec<String> { v.iter().map(|e| field(e, "id").to_owned()).collect() };
    assert_eq!(ids(&full), ids(&paged));
    assert_eq!(
        ids(&full).iter().collect::<HashSet<_>>().len(),
        full.len(),
        "keine Dubletten"
    );
    let times: Vec<OffsetDateTime> = full
        .iter()
        .map(|e| {
            OffsetDateTime::parse(
                field(e, "at"),
                &time::format_description::well_known::Rfc3339,
            )
            .unwrap()
        })
        .collect();
    assert!(times.windows(2).all(|w| w[0] >= w[1]), "neueste zuerst");
    let asc = env.all(&env.token.clone(), "order=asc", 4).await;
    let mut rev = ids(&full);
    rev.reverse();
    // asc enthält ggf. neuere Einträge (Erstnutzung der Keys oben ist schon geschehen) – Präfix
    assert_eq!(ids(&asc)[..rev.len()], rev[..]);
    let first = env
        .boot(Method::GET, "/audit-log?limit=2", None)
        .await
        .expect(StatusCode::OK);
    assert_eq!(
        first.headers.get("cache-control").unwrap(),
        "no-store",
        "Audit-Daten nicht cachen"
    );

    // --- Filter ---------------------------------------------------------------------------------
    let q = |query: &'static str| {
        let token = env.token.clone();
        let env = &env;
        async move { env.all(&token, query, 50).await }
    };
    assert_eq!(q("kind=Team").await.len(), 3);
    assert_eq!(q("kind=Team,Organization").await.len(), 5);
    let rot = q("action=rotate").await;
    assert_eq!(rot.len(), 1);
    assert_eq!(field(&rot[0], "objectName"), "k1");
    assert_eq!(q("action=revoke").await.len(), 1);
    let k1_entries = q("object=k1").await;
    assert_eq!(k1_entries.len(), 2, "create + rotate");
    let k1_id = field(&k1_entries[0], "objectId").to_owned();
    let by_id = env
        .all(&env.token.clone(), &format!("object={k1_id}"), 50)
        .await;
    assert_eq!(ids(&by_id), ids(&k1_entries));
    let boot_actor = q("actor=bootstrap-admin").await;
    assert_eq!(q("actorType=bootstrap").await.len(), boot_actor.len());
    assert!(
        boot_actor
            .iter()
            .all(|e| field(e, "actor") == "bootstrap-admin")
    );
    assert!(q("actorType=user,gitops").await.is_empty());
    assert!(q("from=2999-01-01T00:00:00Z").await.is_empty());
    assert!(q("to=2000-01-01T00:00:00Z").await.is_empty());
    let window = q("from=2000-01-01T00:00:00Z&to=2999-01-01T00:00:00Z&kind=Team").await;
    assert_eq!(window.len(), 3);
    for bad in [
        "/audit-log?limit=0",
        "/audit-log?limit=501",
        "/audit-log?cursor=kaputt",
        "/audit-log?from=gestern",
        "/audit-log?actorType=robot",
        "/audit-log?order=sideways",
        "/audit-log?unbekannt=1",
    ] {
        let r = env.boot(Method::GET, bad, None).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{bad}: {}", r.text());
    }

    // --- Vorher/Nachher und Redaktion -----------------------------------------------------------
    let cred = q("kind=ProviderCredential").await;
    assert_eq!(cred.len(), 2);
    assert_eq!(field(&cred[0], "action"), "update");
    assert_eq!(cred[0]["before"]["spec"]["fields"], json!(["apiKey"]));
    assert_eq!(cred[0]["after"]["spec"]["fields"], json!(["apiKey"]));
    let rotate = &rot[0];
    assert!(rotate["before"]["spec"].get("expiresAt").is_none());
    assert!(rotate["after"]["spec"]["expiresAt"].is_string());
    let moved = q("object=k3&action=update").await;
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0]["before"]["spec"]["team"], "data");
    assert_eq!(moved[0]["after"]["spec"]["team"], "ops");
    assert_eq!(
        (
            field(&moved[0], "organizationBefore"),
            field(&moved[0], "teamBefore")
        ),
        ("acme", "data")
    );
    assert_eq!(
        (field(&moved[0], "organization"), field(&moved[0], "team")),
        ("other", "ops")
    );

    // --- NDJSON-Export --------------------------------------------------------------------------
    let auth = format!("Bearer {auditor}");
    let r = env
        .send(
            Method::GET,
            "/admin/api/v1/audit-log",
            &[("authorization", &auth), ("accept", "application/x-ndjson")],
            None,
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(
        r.headers.get("content-type").unwrap(),
        "application/x-ndjson"
    );
    let text = r.text();
    assert!(text.ends_with('\n'));
    let lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let all_now = env.all(&auditor, "", 500).await;
    assert_eq!(ids(&lines), ids(&all_now));
    // Export mit Filter und Höchstzahl
    let r = env
        .send(
            Method::GET,
            "/admin/api/v1/audit-log?kind=ApiKey&limit=2",
            &[("authorization", &auth), ("accept", "application/x-ndjson")],
            None,
        )
        .await
        .expect(StatusCode::OK);
    let lines: Vec<Value> = r
        .text()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert!(lines.iter().all(|e| field(e, "kind") == "api-key"));
    // Kein Geheimnis, kein Key-Hash im gesamten Log (JSON und NDJSON)
    let dump = format!("{text}{}", serde_json::to_string(&all_now).unwrap());
    for s in secrets {
        assert!(!dump.contains(s), "Geheimnis im Audit-Log: {s}");
        assert!(
            !dump.contains(&hash_api_key(s).to_hex()),
            "Key-Hash im Audit-Log"
        );
    }
    assert!(!dump.contains("GEHEIM"));
    assert!(!dump.contains("keyHash") && !dump.contains("key_hash"));

    // --- Erstnutzung der Admin-API-Keys (genau einmal) ------------------------------------------
    let first_use = q("action=first-use").await;
    let mut actors: Vec<&str> = first_use.iter().map(|e| field(e, "actor")).collect();
    actors.sort_unstable();
    assert_eq!(actors, ["admin-key:auditor", "admin-key:viewer"]);
    assert!(
        first_use
            .iter()
            .all(|e| field(e, "kind") == "admin-api-key")
    );

    // --- Bereich je Rolle -----------------------------------------------------------------------
    let acme = env.all(&org_admin, "", 50).await;
    assert!(!acme.is_empty());
    assert!(
        acme.iter().all(|e| field(e, "organization") == "acme"
            || field(e, "organizationBefore") == "acme"),
        "{acme:#?}"
    );
    let names: HashSet<&str> = acme.iter().map(|e| field(e, "objectName")).collect();
    for want in ["acme", "platform", "data", "k1", "k1-neu", "k3"] {
        assert!(names.contains(want), "{want} fehlt: {names:?}");
    }
    for not in ["other", "ops", "k2", "openai-prod", "auditor"] {
        assert!(!names.contains(not), "{not} sichtbar: {names:?}");
    }
    let platform = env.all(&team_admin, "", 50).await;
    let names: HashSet<&str> = platform.iter().map(|e| field(e, "objectName")).collect();
    assert_eq!(
        names,
        ["platform", "k1", "k1-neu"]
            .into_iter()
            .collect::<HashSet<_>>()
    );
    // Filter wirken zusätzlich zum Bereich
    let platform_keys = env.all(&team_admin, "kind=Team", 50).await;
    assert_eq!(platform_keys.len(), 1);
    let other = env.all(&scoped_auditor, "", 50).await;
    let names: HashSet<&str> = other.iter().map(|e| field(e, "objectName")).collect();
    assert_eq!(
        names,
        ["other", "ops", "k2", "k3"]
            .into_iter()
            .collect::<HashSet<_>>()
    );
    // Export respektiert den Bereich ebenso
    let r = env
        .send(
            Method::GET,
            "/admin/api/v1/audit-log",
            &[
                ("authorization", &format!("Bearer {team_admin}")),
                ("accept", "application/x-ndjson"),
            ],
            None,
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(r.text().lines().count(), platform.len());

    // --- Sessions enden bei Rollenänderung und Löschen; Logout ---------------------------------
    let anna = env
        .boot(
            Method::POST,
            "/users",
            Some(json!({"metadata": {"name": "anna"},
                        "spec": {"email": "anna@example.org", "roles": [{"role": "viewer"}]}})),
        )
        .await
        .expect(StatusCode::CREATED)
        .json();
    let anna_id: Id = anna["id"].as_str().unwrap().parse().unwrap();
    let s1 = env.session_for(anna_id).await;
    let s2 = env.session_for(anna_id).await;
    env.sess(&s1, Method::GET, "/session")
        .await
        .expect(StatusCode::OK);
    env.sess(&s1, Method::GET, "/audit-log")
        .await
        .expect(StatusCode::FORBIDDEN);
    // Änderung ohne Rollen: Sessions bleiben
    env.boot(
        Method::PUT,
        "/users/anna",
        Some(json!({"metadata": {"name": "anna"},
                    "spec": {"email": "anna@example.net", "roles": [{"role": "viewer"}]}})),
    )
    .await
    .expect(StatusCode::OK);
    env.sess(&s2, Method::GET, "/session")
        .await
        .expect(StatusCode::OK);
    // Rollenänderung: alle Sessions beendet
    env.boot(
        Method::PUT,
        "/users/anna",
        Some(json!({"metadata": {"name": "anna"},
                    "spec": {"email": "anna@example.net", "roles": [{"role": "auditor"}]}})),
    )
    .await
    .expect(StatusCode::OK);
    for s in [&s1, &s2] {
        env.sess(s, Method::GET, "/session")
            .await
            .expect(StatusCode::UNAUTHORIZED);
    }
    let revoked = q("kind=admin-session&action=revoke").await;
    assert_eq!(revoked.len(), 1);
    assert_eq!(field(&revoked[0], "objectName"), "anna");
    assert_eq!(field(&revoked[0], "actor"), "bootstrap-admin");
    assert_eq!(revoked[0]["after"]["reason"], "roles-changed");
    assert_eq!(revoked[0]["after"]["sessions"], 2);
    // Neue Session mit Rolle auditor liest das Audit-Log; Abmelden steht im Log
    let s3 = env.session_for(anna_id).await;
    env.sess(&s3, Method::GET, "/audit-log?kind=admin-session")
        .await
        .expect(StatusCode::OK);
    let me = env.sess(&s3, Method::GET, "/session").await.json();
    let csrf = me["csrfToken"].as_str().unwrap().to_owned();
    let cookie = format!("__Host-aigw-session={s3}");
    env.send(
        Method::POST,
        "/admin/auth/logout",
        &[("cookie", &cookie), ("x-csrf-token", &csrf)],
        None,
    )
    .await
    .expect(StatusCode::OK);
    env.sess(&s3, Method::GET, "/session")
        .await
        .expect(StatusCode::UNAUTHORIZED);
    let logout = q("kind=admin-session&action=logout").await;
    assert_eq!(logout.len(), 1);
    assert_eq!(field(&logout[0], "actor"), "user:anna");
    assert!(!serde_json::to_string(&logout).unwrap().contains(&s3));
    // Löschen: Sessions beendet
    let s4 = env.session_for(anna_id).await;
    env.boot(Method::DELETE, "/users/anna", None)
        .await
        .expect(StatusCode::NO_CONTENT);
    env.sess(&s4, Method::GET, "/session")
        .await
        .expect(StatusCode::UNAUTHORIZED);
    let revoked = q("kind=admin-session&action=revoke").await;
    assert_eq!(revoked.len(), 2);
    assert_eq!(revoked[0]["after"]["reason"], "user-deleted");
    assert_eq!(revoked[0]["after"]["sessions"], 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_log_sqlite() {
    let dir = std::env::temp_dir().join(format!("aigw-audit-test-{}", new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let url = format!("sqlite://{}/aigw.db?mode=rwc", dir.display());
    scenario(Store::connect(&url).await.unwrap()).await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_log_postgres() {
    let Ok(base) = std::env::var("AIGW_TEST_POSTGRES_URL") else {
        eprintln!("AIGW_TEST_POSTGRES_URL nicht gesetzt – Postgres-Test übersprungen");
        return;
    };
    let db = format!("aigw_audit_test_{}", new_id().simple());
    let admin = sqlx::PgPool::connect(&base).await.unwrap();
    sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {db}")))
        .execute(&admin)
        .await
        .unwrap();
    let (prefix, _) = base.rsplit_once('/').expect("URL mit Datenbankpfad");
    let url = format!("{prefix}/{db}");
    let store = Store::connect(&url).await.unwrap();
    scenario(store.clone()).await;
    store.close().await;
    sqlx::query(AssertSqlSafe(format!("DROP DATABASE {db} WITH (FORCE)")))
        .execute(&admin)
        .await
        .unwrap();
}

/// F38: `If-Match` ist Pflicht – Anfragen ohne eigenen ETag schreiben hier mit `If-Match: *`
/// (eigene Tests prüfen 428/412 ausdrücklich).
fn with_if_match(req: axum::http::request::Builder) -> axum::http::request::Builder {
    use axum::http::Method as M;
    let action = req.uri_ref().is_some_and(|u| {
        let p = u.path();
        ["/revoke", "/suspend", "/resume", "/rotate"]
            .iter()
            .any(|s| p.ends_with(s))
    });
    let write = req
        .method_ref()
        .is_some_and(|m| matches!(*m, M::PUT | M::PATCH | M::DELETE) || (*m == M::POST && action));
    if write
        && !req
            .headers_ref()
            .is_some_and(|h| h.contains_key("if-match"))
    {
        req.header("if-match", "*")
    } else {
        req
    }
}
