// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! AIGW-74 (Admin/Auth) über HTTP gegen SQLite und Postgres:
//! F55 (Rolle `gitops`), F56 (`auth/info`), F87 (Höchstlaufzeit von Admin-Keys), F38
//! (`If-Match` bei Admin-Keys und Zugangsdaten), F4 (GitOps-Keys Standard `ignore`), F52
//! (`gitops.secret_dir`), F193 (Keys ohne Team für Org-Rollen), F183 (Plugin-Bindungen je
//! Ebene, `locked` von oben), F65 (Kompatibilitätsliste Zugangsdaten), F177
//! (`upstream.ca_dir`), F66 (Alias entfernen mit Verweisen).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr,
    clippy::too_many_lines
)]

use std::sync::Arc;

use aigw_admin::gitops::{GitopsState, router as gitops_router};
use aigw_admin::{AdminState, BootstrapToken, UpstreamPolicy, router_with};
use aigw_core::config::{AdminAuthConfig, BootstrapMode};
use aigw_core::resource::new_id;
use aigw_store::{Keyring, MasterKey, Store};
use axum::Router;
use axum::body::Body;
use http_body_util::BodyExt as _;
use hyper::{Method, Request, StatusCode};
use serde_json::{Value, json};
use sqlx::AssertSqlSafe;
use time::OffsetDateTime;
use tower::ServiceExt as _;

const BOOT: &str = "bootstrap-token-0123456789-0123456789-abcdef";

struct Env {
    app: Router,
    dir: std::path::PathBuf,
}

struct Resp {
    status: StatusCode,
    etag: String,
    body: String,
}

impl Resp {
    #[track_caller]
    fn expect(self, s: StatusCode) -> Self {
        assert_eq!(self.status, s, "Antwort: {}", self.body);
        self
    }
    fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|e| panic!("kein JSON ({e}): {}", self.body))
    }
}

impl Env {
    async fn start(store: Store) -> Self {
        store.migrate().await.unwrap();
        let dir = std::env::temp_dir().join(format!("aigw-admin-auth-api-{}", new_id()));
        for sub in ["secrets", "ca"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        std::fs::write(dir.join("secrets/openai"), "sk-aus-datei\n").unwrap();
        std::fs::write(dir.join("outside"), "geheim-ausserhalb").unwrap();
        std::fs::write(dir.join("ca/intern.pem"), "-----BEGIN CERTIFICATE-----\n").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.join("outside"), dir.join("secrets/link")).unwrap();
            std::os::unix::fs::symlink(dir.join("outside"), dir.join("ca/link.pem")).unwrap();
        }
        let cfg = AdminAuthConfig {
            bootstrap: BootstrapMode::On,
            max_admin_key_lifetime: Some("30d".into()),
            ..AdminAuthConfig::default()
        };
        let keyring = Arc::new(Keyring::new(MasterKey::generate().unwrap().0));
        let upstreams = UpstreamPolicy::strict().with_ca_dir(Some(dir.join("ca")));
        let state = AdminState::from_config(
            store.clone(),
            &cfg,
            Some(BootstrapToken::new(BOOT).unwrap()),
            None,
        )
        .unwrap()
        .with_keyring(Arc::clone(&keyring))
        .with_upstream_policy(upstreams.clone());
        let auth = Arc::clone(&state.auth);
        let app = router_with(state).merge(gitops_router(GitopsState {
            store: store.clone(),
            keyring,
            auth,
            upstreams,
            secret_dir: Some(dir.join("secrets")),
        }));
        Self { app, dir }
    }

    async fn raw(
        &self,
        token: &str,
        method: Method,
        path: &str,
        if_match: Option<&str>,
        body: Option<(&str, String)>,
    ) -> Resp {
        let mut req = Request::builder()
            .method(method)
            .uri(format!("/admin/api/v1{path}"))
            .header("authorization", format!("Bearer {token}"));
        if let Some(t) = if_match {
            req = req.header("if-match", t);
        }
        let body = match body {
            Some((ct, b)) => {
                req = req.header("content-type", ct);
                Body::from(b)
            }
            None => Body::empty(),
        };
        let res = self
            .app
            .clone()
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let etag = res
            .headers()
            .get("etag")
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        Resp {
            status,
            etag,
            body: String::from_utf8(bytes.to_vec()).unwrap(),
        }
    }

    /// JSON-Anfrage; schreibende Anfragen auf Einzelobjekte mit `If-Match: *`.
    async fn call(&self, token: &str, method: Method, path: &str, body: Option<Value>) -> Resp {
        let item_write = matches!(method, Method::PUT | Method::PATCH | Method::DELETE)
            || (method == Method::POST
                && ["/suspend", "/resume", "/revoke", "/rotate"]
                    .iter()
                    .any(|s| path.split('?').next().unwrap_or("").ends_with(s)));
        let im = item_write.then_some("*");
        self.raw(
            token,
            method,
            path,
            im,
            body.map(|b| ("application/json", b.to_string())),
        )
        .await
    }

    async fn boot(&self, method: Method, path: &str, body: Option<Value>) -> Resp {
        self.call(BOOT, method, path, body).await
    }

    async fn yaml(&self, token: &str, path: &str, yaml: &str) -> Resp {
        self.raw(
            token,
            Method::POST,
            path,
            None,
            Some(("application/yaml", yaml.to_owned())),
        )
        .await
    }

    /// Admin-API-Key mit diesen Rollen; liefert den Klartext.
    async fn admin_key(&self, name: &str, roles: Value) -> String {
        let r = self
            .boot(
                Method::POST,
                "/admin-api-keys",
                Some(json!({"name": name, "roles": roles})),
            )
            .await
            .expect(StatusCode::CREATED)
            .json();
        r["secret"].as_str().unwrap().to_owned()
    }
}

fn names(v: &Value) -> Vec<String> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["metadata"]["name"].as_str().unwrap().to_owned())
        .collect()
}

async fn scenario(store: Store) {
    let env = Env::start(store.clone()).await;
    let ok = StatusCode::OK;
    let created = StatusCode::CREATED;

    // --- Grunddaten ---------------------------------------------------------------------------
    for o in ["acme", "other"] {
        env.boot(
            Method::POST,
            "/organizations",
            Some(json!({"metadata": {"name": o}, "spec": {}})),
        )
        .await
        .expect(created);
    }
    env.boot(
        Method::POST,
        "/teams",
        Some(json!({"metadata": {"name": "platform"}, "spec": {"organization": "acme"}})),
    )
    .await
    .expect(created);
    for (g, aliases) in [("gpt-4o", json!(["gpt-4o-latest"])), ("claude", json!([]))] {
        env.boot(
            Method::POST,
            "/model-groups",
            Some(json!({"metadata": {"name": g}, "spec": {"aliases": aliases}})),
        )
        .await
        .expect(created);
    }

    // --- F56: auth/info -----------------------------------------------------------------------
    let info = env
        .boot(Method::GET, "/auth/info", None)
        .await
        .expect(ok)
        .json();
    assert_eq!(info["readOnly"], false);
    assert_eq!(info["bootstrap"], true);
    // F135/F201: Mock-Schalter (Standard an) und Höchstlaufzeit (nur angemeldet).
    assert_eq!(info["uiMock"], true);
    assert_eq!(info["maxAdminKeyLifetimeSecs"], 30 * 86_400);

    // --- F87: Höchstlaufzeit; F55: Rolle gitops nur global ------------------------------------
    let r = env
        .boot(
            Method::POST,
            "/admin-api-keys",
            Some(json!({"name": "zu-lang", "roles": [{"role": "gitops"}],
                        "expiresAt": "2099-01-01T00:00:00Z"})),
        )
        .await
        .expect(StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains("max_admin_key_lifetime"), "{}", r.body);
    env.boot(
        Method::POST,
        "/admin-api-keys",
        Some(
            json!({"name": "scoped-gitops", "roles": [{"role": "gitops", "organization": "acme"}]}),
        ),
    )
    .await
    .expect(StatusCode::UNPROCESSABLE_ENTITY);
    let ci = env.admin_key("ci", json!([{"role": "gitops"}])).await;
    let k = env
        .boot(Method::GET, "/admin-api-keys/ci", None)
        .await
        .expect(ok);
    assert!(!k.etag.is_empty());
    let exp = OffsetDateTime::parse(
        k.json()["expiresAt"].as_str().unwrap(),
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    let days = (exp - OffsetDateTime::now_utc()).whole_days();
    assert!(
        (29..=30).contains(&days),
        "Ablauf automatisch ≈ 30 Tage: {days}"
    );
    // F38 bei Admin-Keys: ohne If-Match 428, alter ETag 412, aktueller 200.
    let upd = json!({"description": "CI", "roles": [{"role": "gitops"}]});
    let body = Some(("application/json", upd.to_string()));
    env.raw(BOOT, Method::PUT, "/admin-api-keys/ci", None, body.clone())
        .await
        .expect(StatusCode::PRECONDITION_REQUIRED);
    let r = env
        .raw(
            BOOT,
            Method::PUT,
            "/admin-api-keys/ci",
            Some(&k.etag),
            body.clone(),
        )
        .await
        .expect(ok);
    assert_ne!(r.etag, k.etag);
    env.raw(BOOT, Method::PUT, "/admin-api-keys/ci", Some(&k.etag), body)
        .await
        .expect(StatusCode::PRECONDITION_FAILED);
    env.raw(BOOT, Method::DELETE, "/admin-api-keys/ci", None, None)
        .await
        .expect(StatusCode::PRECONDITION_REQUIRED);

    // gitops: nur config:export/apply (auch Trockenlauf)
    let manifest = "apiVersion: aigw/v1alpha1\nkind: Organization\nmetadata: {name: gitops-org}\n";
    env.raw(&ci, Method::GET, "/config:export", None, None)
        .await
        .expect(ok);
    env.yaml(&ci, "/config:apply?dryRun=true", manifest)
        .await
        .expect(ok);
    let r = env.yaml(&ci, "/config:apply", manifest).await.expect(ok);
    assert_eq!(r.json()["applied"], true);
    for (m, p) in [
        (Method::GET, "/organizations/acme"),
        (Method::GET, "/model-groups/gpt-4o"),
        (Method::GET, "/admin-api-keys"),
        (Method::GET, "/audit-log"),
    ] {
        env.call(&ci, m, p, None)
            .await
            .expect(StatusCode::FORBIDDEN);
    }
    env.call(
        &ci,
        Method::POST,
        "/organizations",
        Some(json!({"metadata": {"name": "x"}, "spec": {}})),
    )
    .await
    .expect(StatusCode::FORBIDDEN);
    // Listen bleiben für gitops leer (kein Leserecht).
    let v = env
        .call(&ci, Method::GET, "/teams", None)
        .await
        .expect(ok)
        .json();
    assert!(names(&v).is_empty());
    // viewer: export ja, apply nein
    let viewer = env.admin_key("viewer", json!([{"role": "viewer"}])).await;
    env.raw(&viewer, Method::GET, "/config:export", None, None)
        .await
        .expect(ok);
    env.yaml(&viewer, "/config:apply?dryRun=true", manifest)
        .await
        .expect(StatusCode::FORBIDDEN);

    // --- F193: Keys ohne Team gehören Org-Rollen ----------------------------------------------
    let acme_admin = env
        .admin_key(
            "acme-admin",
            json!([{"role": "org-admin", "organization": "acme"}]),
        )
        .await;
    let plat_admin = env
        .admin_key(
            "plat-admin",
            json!([{"role": "team-admin", "organization": "acme", "team": "platform"}]),
        )
        .await;
    env.call(
        &acme_admin,
        Method::POST,
        "/api-keys",
        Some(json!({"metadata": {"name": "acme-solo"}, "spec": {"organization": "acme"}})),
    )
    .await
    .expect(created);
    env.call(
        &acme_admin,
        Method::POST,
        "/api-keys",
        Some(json!({"metadata": {"name": "other-solo"}, "spec": {"organization": "other"}})),
    )
    .await
    .expect(StatusCode::FORBIDDEN);
    env.boot(
        Method::POST,
        "/api-keys",
        Some(json!({"metadata": {"name": "plat-key"},
                    "spec": {"organization": "acme", "team": "platform"}})),
    )
    .await
    .expect(created);
    let v = env
        .call(&acme_admin, Method::GET, "/api-keys", None)
        .await
        .expect(ok)
        .json();
    assert_eq!(names(&v), ["acme-solo", "plat-key"]);
    env.call(
        &acme_admin,
        Method::POST,
        "/api-keys/acme-solo/suspend",
        None,
    )
    .await
    .expect(ok);
    env.call(
        &acme_admin,
        Method::POST,
        "/api-keys/acme-solo/resume",
        None,
    )
    .await
    .expect(ok);
    // team-admin: Keys ohne Team sind nicht seine
    let v = env
        .call(&plat_admin, Method::GET, "/api-keys", None)
        .await
        .expect(ok)
        .json();
    assert_eq!(names(&v), ["plat-key"]);
    env.call(&plat_admin, Method::GET, "/api-keys/acme-solo", None)
        .await
        .expect(StatusCode::FORBIDDEN);
    env.call(
        &plat_admin,
        Method::POST,
        "/api-keys",
        Some(json!({"metadata": {"name": "nope"}, "spec": {"organization": "acme"}})),
    )
    .await
    .expect(StatusCode::FORBIDDEN);

    // --- F183: Plugin-Bindungen je Ebene, locked von oben -------------------------------------
    let binding = |name: &str, level: &str, target: Option<&str>, config: Value| {
        let mut scope = json!({"level": level});
        if let Some(t) = target {
            scope["target"] = json!(t);
        }
        json!({"metadata": {"name": name},
               "spec": {"scope": scope, "plugin": "pii", "order": 10, "config": config}})
    };
    env.boot(
        Method::POST,
        "/plugin-bindings",
        Some(binding(
            "pii-global",
            "global",
            None,
            json!({"locked": true}),
        )),
    )
    .await
    .expect(created);
    env.call(
        &acme_admin,
        Method::POST,
        "/plugin-bindings",
        Some(binding("pii-acme", "organization", Some("acme"), json!({}))),
    )
    .await
    .expect(created);
    let r = env
        .call(
            &acme_admin,
            Method::POST,
            "/plugin-bindings",
            Some(binding(
                "pii-acme-off",
                "organization",
                Some("acme"),
                json!({"inherit": "disable"}),
            )),
        )
        .await
        .expect(StatusCode::FORBIDDEN);
    assert!(r.body.contains("pii-global"), "{}", r.body);
    for (who, b) in [
        (&acme_admin, binding("g2", "global", None, json!({}))),
        (
            &acme_admin,
            binding("o2", "organization", Some("other"), json!({})),
        ),
        (
            &plat_admin,
            binding("o3", "organization", Some("acme"), json!({})),
        ),
        (
            &acme_admin,
            binding("m1", "model-group", Some("gpt-4o"), json!({})),
        ),
    ] {
        env.call(who, Method::POST, "/plugin-bindings", Some(b))
            .await
            .expect(StatusCode::FORBIDDEN);
    }
    env.call(
        &plat_admin,
        Method::POST,
        "/plugin-bindings",
        Some(binding("pii-plat", "team", Some("platform"), json!({}))),
    )
    .await
    .expect(created);
    env.call(
        &acme_admin,
        Method::POST,
        "/plugin-bindings",
        Some(binding("pii-solo", "api-key", Some("acme-solo"), json!({}))),
    )
    .await
    .expect(created);
    // AIGW-58: Verweise auf globale Secrets/Zugangsdaten nur durch globale admins –
    // sonst ließe sich ein globaler Schlüssel per eigenem endpoint abziehen.
    for cfg in [
        json!({"provider": "openai", "credential": "oai", "endpoint": "https://attacker.example"}),
        json!({"provider": "openai", "secret": "oai"}),
    ] {
        let r = env
            .call(
                &acme_admin,
                Method::POST,
                "/plugin-bindings",
                Some(json!({"metadata": {"name": "cs-acme"},
                            "spec": {"scope": {"level": "organization", "target": "acme"},
                                     "plugin": "content-safety", "order": 5, "config": cfg}})),
            )
            .await
            .expect(StatusCode::FORBIDDEN);
        assert!(r.body.contains("global"), "{}", r.body);
    }
    env.call(
        &plat_admin,
        Method::POST,
        "/plugin-bindings",
        Some(json!({"metadata": {"name": "wh-plat"},
                    "spec": {"scope": {"level": "team", "target": "platform"},
                             "plugin": "webhook", "order": 5,
                             "config": {"url": "https://hooks.example.com/x",
                                        "secret": {"env": "PATH"}}}})),
    )
    .await
    .expect(StatusCode::FORBIDDEN);
    env.call(
        &plat_admin,
        Method::DELETE,
        "/plugin-bindings/pii-acme",
        None,
    )
    .await
    .expect(StatusCode::FORBIDDEN);
    // Lesen bleibt für jede wirksame Rolle möglich (wie die Liste).
    env.call(&plat_admin, Method::GET, "/plugin-bindings/pii-acme", None)
        .await
        .expect(ok);
    env.call(
        &acme_admin,
        Method::DELETE,
        "/plugin-bindings/pii-plat",
        None,
    )
    .await
    .expect(StatusCode::NO_CONTENT);
    // globaler Admin darf (Sperre gehört ihm), Bindung wäre zur Laufzeit wirkungslos
    env.boot(
        Method::POST,
        "/plugin-bindings",
        Some(binding(
            "pii-plat-off",
            "team",
            Some("platform"),
            json!({"inherit": "disable"}),
        )),
    )
    .await
    .expect(created);

    // --- F65: Kompatibilitätsliste ------------------------------------------------------------
    env.boot(
        Method::POST,
        "/provider-credentials",
        Some(json!({"metadata": {"name": "oa"},
                    "spec": {"provider": "openai", "data": {"apiKey": {"value": "sk-1"}}}})),
    )
    .await
    .expect(created);
    env.boot(
        Method::POST,
        "/provider-credentials",
        Some(json!({"metadata": {"name": "an"},
                    "spec": {"provider": "anthropic", "data": {"apiKey": {"value": "sk-2"}}}})),
    )
    .await
    .expect(created);
    let dep = |name: &str, provider: &str, cred: &str, settings: Value| {
        json!({"metadata": {"name": name},
               "spec": {"modelGroup": "gpt-4o", "provider": provider, "providerModel": "m",
                        "endpoint": "https://llm.example.org/v1", "credential": cred,
                        "settings": settings}})
    };
    env.boot(
        Method::POST,
        "/deployments",
        Some(dep("vllm-oa", "vllm", "oa", json!({}))),
    )
    .await
    .expect(created);
    env.boot(
        Method::POST,
        "/deployments",
        Some(dep("compat-oa", "openai-compatible", "oa", json!({}))),
    )
    .await
    .expect(created);
    let r = env
        .boot(
            Method::POST,
            "/deployments",
            Some(dep("oa-an", "openai", "an", json!({}))),
        )
        .await
        .expect(StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains("passt nicht"), "{}", r.body);
    // Anbieterwechsel der Zugangsdaten nur, solange alle Deployments kompatibel bleiben.
    env.boot(
        Method::PATCH,
        "/provider-credentials/oa",
        Some(json!({"spec": {"provider": "vllm"}})),
    )
    .await
    .expect(StatusCode::CONFLICT);
    // F38 auch bei PATCH
    env.raw(
        BOOT,
        Method::PATCH,
        "/provider-credentials/oa",
        None,
        Some((
            "application/json",
            json!({"metadata": {"labels": {"a": "b"}}}).to_string(),
        )),
    )
    .await
    .expect(StatusCode::PRECONDITION_REQUIRED);

    // --- F177: settings.caFile nur aus upstream.ca_dir ----------------------------------------
    let ca_abs = env.dir.join("ca/intern.pem").to_string_lossy().into_owned();
    for (n, ca, status) in [
        ("ca-rel", "intern.pem".to_owned(), created),
        ("ca-abs", ca_abs, created),
        (
            "ca-out",
            "/etc/hostname".to_owned(),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "ca-up",
            "../outside".to_owned(),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "ca-missing",
            "fehlt.pem".to_owned(),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        #[cfg(unix)]
        (
            "ca-link",
            "link.pem".to_owned(),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        let r = env
            .boot(
                Method::POST,
                "/deployments",
                Some(dep(n, "openai", "oa", json!({"caFile": ca}))),
            )
            .await;
        assert_eq!(r.status, status, "{n}: {}", r.body);
    }

    // --- F52: gitops.secret_dir ---------------------------------------------------------------
    let cred = |file: &str| {
        format!(
            "apiVersion: aigw/v1alpha1\nkind: ProviderCredential\nmetadata: {{name: from-file}}\n\
             spec:\n  provider: openai\n  data:\n    apiKey: {{valueFrom: {{file: {file}}}}}\n"
        )
    };
    env.yaml(BOOT, "/config:apply", &cred("openai"))
        .await
        .expect(ok);
    let outside = env.dir.join("outside").to_string_lossy().into_owned();
    for bad in [outside.as_str(), "../outside", "link"] {
        let r = env
            .yaml(BOOT, "/config:apply", &cred(bad))
            .await
            .expect(StatusCode::UNPROCESSABLE_ENTITY);
        assert!(!r.body.contains("geheim-ausserhalb"), "{}", r.body);
    }

    // --- F4: API-Keys in GitOps standardmäßig ignoriert ---------------------------------------
    let (s, yaml) = {
        let r = env
            .raw(BOOT, Method::GET, "/config:export", None, None)
            .await;
        (r.status, r.body)
    };
    assert_eq!(s, ok);
    assert!(!yaml.contains("kind: ApiKey"), "{yaml}");
    let r = env
        .raw(
            BOOT,
            Method::GET,
            "/config:export?apiKeys=include",
            None,
            None,
        )
        .await
        .expect(ok);
    assert!(r.body.contains("kind: ApiKey"));
    let key_doc = "apiVersion: aigw/v1alpha1\nkind: ApiKey\nmetadata: {name: acme-solo}\n\
                   spec: {organization: acme, metadata: {neu: ja}}\n";
    let r = env
        .yaml(BOOT, "/config:apply?dryRun=true", key_doc)
        .await
        .expect(ok)
        .json();
    let actions: Vec<&str> = r["plan"]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["kind"] == "ApiKey" && c["name"] == "acme-solo")
        .map(|c| c["action"].as_str().unwrap())
        .collect();
    assert_eq!(actions, ["skip"], "{r}");

    // --- F66: Alias entfernen mit Verweisen ---------------------------------------------------
    let team = env
        .boot(Method::GET, "/teams/platform", None)
        .await
        .expect(ok)
        .json();
    let mut spec = team["spec"].clone();
    spec["allowedModels"] = json!(["gpt-4o-latest", "claude"]);
    env.boot(
        Method::PUT,
        "/teams/platform",
        Some(json!({"metadata": {"name": "platform"}, "spec": spec})),
    )
    .await
    .expect(ok);
    let no_alias = json!({"metadata": {"name": "gpt-4o"}, "spec": {"aliases": []}});
    let r = env
        .boot(Method::PUT, "/model-groups/gpt-4o", Some(no_alias.clone()))
        .await
        .expect(StatusCode::CONFLICT);
    assert_eq!(r.json()["violations"][0]["name"], "platform");
    env.boot(
        Method::PUT,
        "/model-groups/gpt-4o?cascade=remove",
        Some(no_alias),
    )
    .await
    .expect(ok);
    let team = env
        .boot(Method::GET, "/teams/platform", None)
        .await
        .expect(ok)
        .json();
    assert_eq!(team["spec"]["allowedModels"], json!(["claude"]));

    let _ = std::fs::remove_dir_all(&env.dir);
}

#[tokio::test]
async fn sqlite() {
    scenario(Store::connect("sqlite::memory:").await.unwrap()).await;
}

#[tokio::test]
async fn postgres() {
    let Ok(base) = std::env::var("AIGW_TEST_POSTGRES_URL") else {
        eprintln!("AIGW_TEST_POSTGRES_URL nicht gesetzt – Postgres-Test übersprungen");
        return;
    };
    let db = format!("aigw_admin_auth_{}", new_id().simple());
    let admin = sqlx::PgPool::connect(&base).await.unwrap();
    sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {db}")))
        .execute(&admin)
        .await
        .unwrap();
    let (prefix, _) = base.rsplit_once('/').expect("URL mit Datenbankpfad");
    let store = Store::connect(&format!("{prefix}/{db}")).await.unwrap();
    scenario(store.clone()).await;
    store.close().await;
    sqlx::query(AssertSqlSafe(format!("DROP DATABASE {db} WITH (FORCE)")))
        .execute(&admin)
        .await
        .unwrap();
}
