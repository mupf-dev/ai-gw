// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Grenzen Key ≤ Team ≤ Organisation in der Admin-API (AIGW-67): Limits je Metrik/Fenster,
//! Budgets je Zeitraum, Modellfreigaben; Rollen (`team-admin`, `org-admin`, globaler und
//! eingeschränkter `admin`), `?force=true`, `?cascade=clamp`, Audit-Einträge.
//!
//! Läuft gegen SQLite (In-Memory) und – mit `AIGW_TEST_POSTGRES_URL` bzw. `scripts/check.sh`
//! mit `AIGW_TEST_POSTGRES=docker` – gegen Postgres.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::sync::Arc;

use aigw_admin::auth::{AuthMethod, BoxFuture};
use aigw_admin::{AdminAuthenticator, AdminState, Principal};
use aigw_core::resource::{ModelGroupSpec, ObjectMeta, Role, RoleBinding, new_id};
use aigw_store::Store;
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::AssertSqlSafe;
use tower::ServiceExt;

/// Bearer-Token → Principal mit fester Rolle.
struct Roles;

fn binding(role: Role, org: Option<&str>, team: Option<&str>) -> RoleBinding {
    RoleBinding {
        role,
        organization: org.map(str::to_owned),
        team: team.map(str::to_owned),
    }
}

impl AdminAuthenticator for Roles {
    fn authenticate<'a>(&'a self, headers: &'a HeaderMap) -> BoxFuture<'a, Option<Principal>> {
        let token = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or_default()
            .to_owned();
        Box::pin(async move {
            let roles = match token.as_str() {
                "admin" => vec![binding(Role::Admin, None, None)],
                "acme-admin" => vec![binding(Role::Admin, Some("acme"), None)],
                "org" => vec![binding(Role::OrgAdmin, Some("acme"), None)],
                "team" => vec![binding(Role::TeamAdmin, Some("acme"), Some("platform"))],
                _ => return None,
            };
            Some(Principal {
                subject: format!("test:{token}"),
                roles,
                method: AuthMethod::AdminApiKey,
                display_name: None,
                email: None,
                expires_at: None,
                csrf: None,
            })
        })
    }
}

struct Env {
    app: Router,
    store: Store,
}

struct Resp {
    status: StatusCode,
    body: Value,
}

impl Resp {
    #[track_caller]
    fn ok(self, status: StatusCode) -> Value {
        assert_eq!(self.status, status, "{}", self.body);
        self.body
    }

    /// Erwartet ein Problem mit Code; liefert die Felder der Verstöße als `Name/Feld`.
    #[track_caller]
    fn problem(self, status: StatusCode, code: &str) -> (String, Vec<String>) {
        assert_eq!(self.status, status, "{}", self.body);
        assert_eq!(self.body["code"], code, "{}", self.body);
        let v = self.body["violations"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|v| {
                        format!(
                            "{}/{}",
                            v["name"].as_str().unwrap(),
                            v["field"].as_str().unwrap()
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        (
            self.body["detail"].as_str().unwrap_or_default().to_owned(),
            v,
        )
    }
}

impl Env {
    async fn start(store: Store) -> Self {
        store.migrate().await.unwrap();
        for (name, aliases) in [
            ("gpt-4o", vec!["gpt-4o-latest".to_owned()]),
            ("claude", vec![]),
        ] {
            store
                .create(
                    "test",
                    ObjectMeta::named(name),
                    ModelGroupSpec {
                        aliases,
                        ..ModelGroupSpec::default()
                    },
                )
                .await
                .unwrap();
        }
        let app = aigw_admin::router_with(AdminState::new(store.clone(), Arc::new(Roles)));
        Self { app, store }
    }

    async fn call(&self, who: &str, method: Method, path: &str, body: Option<Value>) -> Resp {
        let mut req = Request::builder()
            .method(method)
            .uri(format!("/admin/api/v1{path}"))
            .header("authorization", format!("Bearer {who}"));
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Body::from(serde_json::to_vec(&b).unwrap())
            }
            None => Body::empty(),
        };
        let res = self
            .app
            .clone()
            .oneshot(with_if_match(req).body(body).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        Resp { status, body }
    }

    async fn post(&self, who: &str, path: &str, body: Value) -> Resp {
        self.call(who, Method::POST, path, Some(body)).await
    }

    async fn put(&self, who: &str, path: &str, body: Value) -> Resp {
        self.call(who, Method::PUT, path, Some(body)).await
    }

    async fn get(&self, path: &str) -> Value {
        self.call("admin", Method::GET, path, None)
            .await
            .ok(StatusCode::OK)
    }

    /// Audit-Einträge (Aktion, Objektname) des Akteurs, neueste zuerst.
    async fn audit(&self, action: &str) -> Vec<(String, Value)> {
        self.store
            .audit_log(500)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.action == action)
            .map(|e| {
                (
                    e.object_name.unwrap_or_default(),
                    e.after.unwrap_or_default(),
                )
            })
            .collect()
    }
}

fn lim(metric: &str, limit: u64, window: &str) -> Value {
    json!({"metric": metric, "limit": limit, "window": window})
}

fn team(name: &str, spec: Value) -> Value {
    let mut spec = spec;
    spec["organization"] = json!("acme");
    json!({"metadata": {"name": name}, "spec": spec})
}

fn key(name: &str, team: &str, spec: Value) -> Value {
    let mut spec = spec;
    spec["team"] = json!(team);
    spec["organization"] = json!("acme"); // F3: Organisation des Teams
    json!({"metadata": {"name": name}, "spec": spec})
}

async fn scenario(store: Store) {
    let env = Env::start(store).await;
    let created = StatusCode::CREATED;
    let ok = StatusCode::OK;
    let invalid = StatusCode::UNPROCESSABLE_ENTITY;
    let conflict = StatusCode::CONFLICT;

    // --- Organisation und Team -----------------------------------------------------------------
    env.post(
        "admin",
        "/organizations",
        json!({"metadata": {"name": "acme"}, "spec": {
            "limits": [lim("requests", 1000, "minute")],
            "budget": {"amount": "1000", "period": "month"}}}),
    )
    .await
    .ok(created);
    let platform = json!({
        "allowedModels": ["gpt-4o"],
        "limits": [lim("tokens", 10000, "minute"), lim("requests", 500, "minute")],
        "budget": {"amount": "100", "period": "month"}});
    env.post("org", "/teams", team("platform", platform.clone()))
        .await
        .ok(created);

    // Team über der Organisation: org-admin → 422 mit einem Verstoß je Feld
    let big = json!({
        "limits": [lim("requests", 2000, "minute"), lim("requests", 9999, "hour")],
        "budget": {"amount": "1500", "period": "month"}});
    let (detail, v) = env
        .post("org", "/teams", team("big", big.clone()))
        .await
        .problem(invalid, "validation-failed");
    assert_eq!(v, ["big/limits[0]", "big/budget"], "{detail}");
    assert!(detail.contains("Organisation \"acme\" (1000)"), "{detail}");
    // force nur für globale Admins (auch nicht für den Admin der Organisation)
    env.post("org", "/teams?force=true", team("big", big.clone()))
        .await
        .problem(StatusCode::FORBIDDEN, "forbidden");
    env.post("acme-admin", "/teams?force=true", team("big", big.clone()))
        .await
        .problem(StatusCode::FORBIDDEN, "forbidden");
    env.post("acme-admin", "/teams", team("big", big.clone()))
        .await
        .problem(invalid, "validation-failed");
    // globaler Admin ohne force: ebenfalls hart; mit force: angelegt + Audit `force`
    env.post("admin", "/teams", team("big", big.clone()))
        .await
        .problem(invalid, "validation-failed");
    env.post("admin", "/teams?force=true", team("big", big.clone()))
        .await
        .ok(created);
    let forced = env.audit("force").await;
    assert_eq!(forced.len(), 1);
    assert_eq!(forced[0].0, "big");
    assert_eq!(forced[0].1["force"], true);
    assert_eq!(forced[0].1["violations"].as_array().unwrap().len(), 2);
    // Ungültige Optionen
    env.post("admin", "/teams?cascade=wild", team("x", json!({})))
        .await
        .problem(StatusCode::BAD_REQUEST, "bad-request");
    env.post("admin", "/teams?typo=1", team("x", json!({})))
        .await
        .problem(StatusCode::BAD_REQUEST, "bad-request");

    // --- Keys im Team (team-admin) -------------------------------------------------------------
    env.post(
        "team",
        "/api-keys",
        key(
            "k1",
            "platform",
            json!({"allowedModels": ["gpt-4o-latest"],
                   "limits": [lim("tokens", 5000, "minute"), lim("tokens", 999999, "day")],
                   "budget": {"amount": "50", "period": "month"}}),
        ),
    )
    .await
    .ok(created);
    let bad = json!({"allowedModels": ["gpt-4o", "claude"],
                     "limits": [lim("tokens", 20000, "minute"), lim("requests", 1001, "minute")],
                     "budget": {"amount": "100.01", "period": "month"}});
    let (detail, v) = env
        .post("team", "/api-keys", key("k2", "platform", bad.clone()))
        .await
        .problem(invalid, "validation-failed");
    assert_eq!(
        v,
        [
            "k2/limits[0]",
            "k2/limits[1]",
            "k2/budget",
            "k2/allowedModels[1]"
        ],
        "{detail}"
    );
    assert!(detail.contains("Team \"platform\" (10000)"), "{detail}");
    assert!(detail.contains("Team \"platform\" (500)"), "{detail}");
    assert!(detail.contains("\"claude\""), "{detail}");
    env.post(
        "team",
        "/api-keys?force=true",
        key("k2", "platform", bad.clone()),
    )
    .await
    .problem(StatusCode::FORBIDDEN, "forbidden");
    env.post(
        "team",
        "/api-keys?cascade=clamp",
        key("k2", "platform", json!({})),
    )
    .await
    .problem(StatusCode::BAD_REQUEST, "bad-request");
    // Anderer Zeitraum: nicht vergleichbar → erlaubt
    env.post(
        "team",
        "/api-keys",
        key(
            "k2",
            "platform",
            json!({"budget": {"amount": "999", "period": "week"}}),
        ),
    )
    .await
    .ok(created);
    // Ändern: dieselben Regeln (team-admin hart, globaler Admin mit force)
    env.put("team", "/api-keys/k2", key("k2", "platform", bad.clone()))
        .await
        .problem(invalid, "validation-failed");
    env.put("org", "/api-keys/k2", key("k2", "platform", bad.clone()))
        .await
        .problem(invalid, "validation-failed");
    // Key ohne Team (F3): direkt unter den Grenzen der Organisation
    let mut free = json!({"metadata": {"name": "free"}, "spec": bad});
    free["spec"]["organization"] = json!("acme");
    let (detail, v) = env
        .post("admin", "/api-keys", free.clone())
        .await
        .problem(invalid, "validation-failed");
    assert_eq!(v, ["free/limits[1]"], "{detail}");
    assert!(detail.contains("Organisation \"acme\" (1000)"), "{detail}");
    env.post("admin", "/api-keys?force=true", free)
        .await
        .ok(created);

    // Key im Team „big“ per force über den Grenzen
    env.post(
        "admin",
        "/api-keys?force=true",
        key(
            "kbig",
            "big",
            json!({"limits": [lim("requests", 5000, "minute")]}),
        ),
    )
    .await
    .ok(created);
    assert!(env.audit("force").await.iter().any(|(n, _)| n == "kbig"));
    // Team mit eigenen Verstößen ändern: weiterhin nur mit force
    let mut big2 = big.clone();
    big2["displayName"] = json!("Groß");
    env.put("admin", "/teams/big", team("big", big2.clone()))
        .await
        .problem(invalid, "validation-failed");
    env.put("admin", "/teams/big?force=true", team("big", big2))
        .await
        .ok(ok);

    // --- Team-Limit senken: 409 mit betroffenen Keys, cascade=clamp ----------------------------
    let mut lower = platform.clone();
    lower["limits"][0]["limit"] = json!(4000);
    lower["budget"]["amount"] = json!("40");
    let (detail, v) = env
        .put("org", "/teams/platform", team("platform", lower.clone()))
        .await
        .problem(conflict, "conflict");
    assert_eq!(v, ["k1/limits[0]", "k1/budget"], "{detail}");
    assert!(detail.contains("cascade=clamp"), "{detail}");
    assert_eq!(
        env.get("/teams/platform").await["spec"]["limits"][0]["limit"],
        10000
    );
    env.put(
        "org",
        "/teams/platform?cascade=clamp",
        team("platform", lower.clone()),
    )
    .await
    .ok(ok);
    let k1 = env.get("/api-keys/k1").await;
    assert_eq!(k1["spec"]["limits"][0]["limit"], 4000);
    assert_eq!(k1["spec"]["limits"][1]["limit"], 999_999); // Tagesfenster: oben kein Limit
    assert_eq!(k1["spec"]["budget"]["amount"], "40");
    let clamped = env.audit("clamp").await;
    assert_eq!(clamped.len(), 1);
    assert_eq!(clamped[0].0, "k1");
    assert_eq!(clamped[0].1["spec"]["limits"][0]["limit"], 4000);

    // Modellfreigaben des Teams einschränken
    let mut claude_only = lower.clone();
    claude_only["allowedModels"] = json!(["claude"]);
    let (_, v) = env
        .put(
            "org",
            "/teams/platform",
            team("platform", claude_only.clone()),
        )
        .await
        .problem(conflict, "conflict");
    assert_eq!(v, ["k1/allowedModels[0]"]);
    // k1 hätte keine Freigabe mehr (leer hieße „alle“) → auch mit clamp nicht automatisch
    let (_, v) = env
        .put(
            "org",
            "/teams/platform?cascade=clamp",
            team("platform", claude_only.clone()),
        )
        .await
        .problem(conflict, "conflict");
    assert_eq!(v, ["k1/allowedModels"]);
    // force (globaler Admin) lässt es zu und hält die Betroffenen fest
    env.put(
        "admin",
        "/teams/platform?force=true",
        team("platform", claude_only),
    )
    .await
    .ok(ok);
    let f = env.audit("force").await;
    let note = &f.iter().find(|(n, _)| n == "platform").unwrap().1;
    assert_eq!(note["affected"][0]["name"], "k1");
    env.put("admin", "/teams/platform", team("platform", lower.clone()))
        .await
        .ok(ok);

    // --- Organisationsgrenze senken: Teams und Keys --------------------------------------------
    let org_low = json!({"metadata": {"name": "acme"}, "spec": {
        "limits": [lim("requests", 400, "minute")],
        "budget": {"amount": "30", "period": "month"}}});
    let (detail, v) = env
        .put("admin", "/organizations/acme", org_low.clone())
        .await
        .problem(conflict, "conflict");
    // „big“ lag schon vorher darüber (force) und wird nur mit neuem Verstoß gemeldet
    assert!(v.contains(&"platform/limits[1]".to_owned()), "{detail}");
    assert!(v.contains(&"platform/budget".to_owned()), "{detail}");
    assert!(v.contains(&"k1/budget".to_owned()), "{detail}");
    assert!(!v.contains(&"big/limits[0]".to_owned()), "{detail}");
    env.put("org", "/organizations/acme", org_low.clone())
        .await
        .problem(StatusCode::FORBIDDEN, "forbidden");
    env.put("admin", "/organizations/acme?cascade=clamp", org_low)
        .await
        .ok(ok);
    let t = env.get("/teams/platform").await;
    assert_eq!(t["spec"]["limits"][1]["limit"], 400);
    assert_eq!(t["spec"]["budget"]["amount"], "30");
    assert_eq!(
        env.get("/teams/big").await["spec"]["budget"]["amount"],
        "30"
    );
    assert_eq!(
        env.get("/api-keys/k1").await["spec"]["budget"]["amount"],
        "30"
    );
    assert_eq!(
        env.get("/api-keys/kbig").await["spec"]["limits"][0]["limit"],
        400
    );
    let clamped: Vec<String> = env
        .audit("clamp")
        .await
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    for n in ["platform", "big", "k1", "kbig", "free"] {
        assert!(clamped.contains(&n.to_owned()), "{n}: {clamped:?}");
    }
    // Danach ist alles konsistent: Änderungen ohne Optionen gehen wieder durch
    let mut t2 = t["spec"].clone();
    t2["displayName"] = json!("Plattform");
    env.put("org", "/teams/platform", team("platform", t2))
        .await
        .ok(ok);
}

/// S-09/F203: `payloadModeMax`/`payloadModeLocked` an Organisation und Team; ein
/// `team-admin` kann Payload-Logging nicht über die Vorgaben hinaus ändern.
async fn payload_scenario(store: Store) {
    let env = Env::start(store).await;
    let created = StatusCode::CREATED;
    let ok = StatusCode::OK;
    let invalid = StatusCode::UNPROCESSABLE_ENTITY;
    let forbidden = StatusCode::FORBIDDEN;
    let org = |spec: Value| json!({"metadata": {"name": "acme"}, "spec": spec});

    // Organisation: payloadMode über dem eigenen Maximum → 422, feldgenau
    let (detail, v) = env
        .post(
            "admin",
            "/organizations",
            org(json!({"payloadMode": "full", "payloadModeMax": "masked"})),
        )
        .await
        .problem(invalid, "validation-failed");
    assert_eq!(v, ["acme/payloadMode"], "{detail}");
    assert!(
        detail.contains("payloadMode: full überschreitet die eigene Obergrenze"),
        "{detail}"
    );
    env.post(
        "admin",
        "/organizations",
        org(json!({"payloadModeMax": "masked"})),
    )
    .await
    .ok(created);
    // org-admin darf die Organisation (und damit ihre Payload-Richtlinie) nicht ändern
    env.put(
        "org",
        "/organizations/acme",
        org(json!({"payloadModeMax": "full"})),
    )
    .await
    .problem(forbidden, "forbidden");

    // Team: über dem Maximum der Organisation → 422
    let (detail, v) = env
        .post("org", "/teams", team("t2", json!({"payloadMode": "full"})))
        .await
        .problem(invalid, "validation-failed");
    assert_eq!(v, ["t2/payloadMode"], "{detail}");
    let (detail, v) = env
        .post(
            "org",
            "/teams",
            team("t2", json!({"payloadModeMax": "full"})),
        )
        .await
        .problem(invalid, "validation-failed");
    assert_eq!(v, ["t2/payloadModeMax"], "{detail}");
    env.post("org", "/teams", team("platform", json!({})))
        .await
        .ok(created);

    // team-admin: Key über dem Maximum → 422; darunter erlaubt
    let (detail, v) = env
        .post(
            "team",
            "/api-keys",
            key("k1", "platform", json!({"payloadMode": "full"})),
        )
        .await
        .problem(invalid, "validation-failed");
    assert_eq!(v, ["k1/payloadMode"], "{detail}");
    assert!(
        detail.contains(
            "payloadMode: full überschreitet die Obergrenze payloadModeMax = masked von \
             Organisation \"acme\""
        ),
        "{detail}"
    );
    env.post(
        "team",
        "/api-keys",
        key("k1", "platform", json!({"payloadMode": "off"})),
    )
    .await
    .ok(created);
    // team-admin kann die Team-Richtlinie nicht ändern
    env.put(
        "team",
        "/teams/platform",
        team("platform", json!({"payloadModeMax": "off"})),
    )
    .await
    .problem(forbidden, "forbidden");

    // org-admin sperrt das Team: bestehender Key mit payloadMode → 409, mit clamp entfernt
    let locked = json!({"payloadMode": "metadata", "payloadModeLocked": true});
    let (detail, v) = env
        .put("org", "/teams/platform", team("platform", locked.clone()))
        .await
        .problem(StatusCode::CONFLICT, "conflict");
    assert_eq!(v, ["k1/payloadMode"], "{detail}");
    env.put(
        "org",
        "/teams/platform?cascade=clamp",
        team("platform", locked.clone()),
    )
    .await
    .ok(ok);
    let k1 = env.get("/api-keys/k1").await;
    assert!(k1["spec"].get("payloadMode").is_none(), "{k1}");
    let t = env.get("/teams/platform").await;
    assert_eq!(t["spec"]["payloadModeLocked"], true);
    let clamped = env.audit("clamp").await;
    assert!(clamped.iter().any(|(n, _)| n == "k1"), "{clamped:?}");

    // Gesperrt: team-admin kann payloadMode weder setzen noch ändern (auch nicht senken)
    for mode in ["off", "full"] {
        let (detail, v) = env
            .post(
                "team",
                "/api-keys",
                key("k2", "platform", json!({"payloadMode": mode})),
            )
            .await
            .problem(invalid, "validation-failed");
        assert_eq!(v, ["k2/payloadMode"], "{detail}");
        assert!(detail.contains("gesperrt"), "{detail}");
    }
    let mut k1_spec = k1["spec"].clone();
    k1_spec["payloadMode"] = json!("off");
    env.put(
        "team",
        "/api-keys/k1",
        json!({"metadata": {"name": "k1"}, "spec": k1_spec}),
    )
    .await
    .problem(invalid, "validation-failed");

    // Organisation senkt das Maximum: Team darüber → 409, mit clamp gekürzt (+ Audit)
    let (detail, v) = env
        .put(
            "admin",
            "/organizations/acme",
            org(json!({"payloadModeMax": "off"})),
        )
        .await
        .problem(StatusCode::CONFLICT, "conflict");
    assert_eq!(v, ["platform/payloadMode"], "{detail}");
    env.put(
        "admin",
        "/organizations/acme?cascade=clamp",
        org(json!({"payloadModeMax": "off"})),
    )
    .await
    .ok(ok);
    assert_eq!(
        env.get("/teams/platform").await["spec"]["payloadMode"],
        "off"
    );
    assert!(
        env.audit("clamp")
            .await
            .iter()
            .any(|(n, a)| n == "platform" && a["spec"]["payloadMode"] == "off"),
    );
}

#[tokio::test]
async fn payload_policy_sqlite() {
    payload_scenario(Store::connect("sqlite::memory:").await.unwrap()).await;
}

#[tokio::test]
async fn sqlite() {
    scenario(Store::connect("sqlite::memory:").await.unwrap()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres() {
    let Ok(base) = std::env::var("AIGW_TEST_POSTGRES_URL") else {
        eprintln!("AIGW_TEST_POSTGRES_URL nicht gesetzt – Postgres-Test übersprungen");
        return;
    };
    let db = format!("aigw_bounds_test_{}", new_id().simple());
    let admin = sqlx::PgPool::connect(&base).await.unwrap();
    sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {db}")))
        .execute(&admin)
        .await
        .unwrap();
    let (prefix, _) = base.rsplit_once('/').expect("URL mit Datenbankpfad");
    let store = Store::connect(&format!("{prefix}/{db}")).await.unwrap();
    scenario(store.clone()).await;
    store.close().await;
    // S-09 in eigener Datenbank
    let db2 = format!("{db}_payload");
    sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {db2}")))
        .execute(&admin)
        .await
        .unwrap();
    let store2 = Store::connect(&format!("{prefix}/{db2}")).await.unwrap();
    payload_scenario(store2.clone()).await;
    store2.close().await;
    sqlx::query(AssertSqlSafe(format!("DROP DATABASE {db2} WITH (FORCE)")))
        .execute(&admin)
        .await
        .unwrap();
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
