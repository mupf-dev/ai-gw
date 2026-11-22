// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! GitOps: Grenzen Key ≤ Team ≤ Organisation (AIGW-67) im Plan – alle Verstöße gesammelt, an
//! welchem Dokument sie stehen, bestehende Objekte außerhalb des Manifests, bereits vorher
//! bestehende Verstöße. Gegen SQLite und – mit `AIGW_TEST_POSTGRES_URL` – Postgres.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::collections::BTreeMap;

use aigw_core::resource::{ApiKeySpec, LimitMetric, LimitSpec, LimitWindow, ObjectMeta, new_id};
use aigw_store::gitops::{
    ApiKeyMode, ApplyOptions, GitopsError, Issue, SecretSource, apply, parse_documents, plan,
};
use aigw_store::{Keyring, MasterKey, Store, generate_api_key};
use sqlx::AssertSqlSafe;

#[derive(Default)]
struct Secrets(BTreeMap<String, String>);

impl SecretSource for Secrets {
    fn env(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
    fn file(&self, path: &str) -> std::io::Result<String> {
        std::fs::read_to_string(path)
    }
}

const GROUPS: &str = r#"
apiVersion: aigw/v1alpha1
kind: ModelGroup
metadata: {name: gpt-4o}
spec: {aliases: [gpt-4o-latest]}
---
apiVersion: aigw/v1alpha1
kind: ModelGroup
metadata: {name: claude}
spec: {}
"#;

fn org(requests: u64, budget: &str) -> String {
    format!(
        r#"
---
apiVersion: aigw/v1alpha1
kind: Organization
metadata: {{name: acme}}
spec:
  limits: [{{metric: requests, limit: {requests}, window: minute}}]
  budget: {{amount: "{budget}", period: month}}
"#
    )
}

fn team(models: &str, tokens: u64, requests: u64, budget: &str) -> String {
    format!(
        r#"
---
apiVersion: aigw/v1alpha1
kind: Team
metadata: {{name: platform}}
spec:
  organization: acme
  allowedModels: {models}
  limits:
    - {{metric: tokens, limit: {tokens}, window: minute}}
    - {{metric: requests, limit: {requests}, window: minute}}
  budget: {{amount: "{budget}", period: month}}
"#
    )
}

fn key(name: &str, models: &str, tokens: u64, budget: &str) -> String {
    format!(
        r#"
---
apiVersion: aigw/v1alpha1
kind: ApiKey
metadata: {{name: {name}}}
spec:
  organization: acme
  team: platform
  allowedModels: {models}
  limits: [{{metric: tokens, limit: {tokens}, window: minute}}]
  budget: {{amount: "{budget}", period: month}}
  secret: {{valueFrom: {{env: AIGW_SECRET_{name}}}}}
"#
    )
}

async fn issues(store: &Store, ring: &Keyring, env: &Secrets, yaml: &str) -> Vec<Issue> {
    let docs = parse_documents(yaml).unwrap();
    let opts = ApplyOptions {
        api_keys: ApiKeyMode::Include,
        ..ApplyOptions::default()
    };
    match plan(store, ring, &docs, &opts, env).await {
        Err(GitopsError::Invalid(i)) => i,
        other => panic!("Verstöße erwartet, erhalten: {other:?}"),
    }
}

/// `Art Name: Meldung` je Verstoß.
fn lines(issues: &[Issue]) -> Vec<String> {
    issues
        .iter()
        .map(|i| {
            format!(
                "{} {}: {}",
                i.kind.map(|k| k.to_string()).unwrap_or_default(),
                i.name.clone().unwrap_or_default(),
                i.message
            )
        })
        .collect()
}

async fn run(store: Store) {
    store.migrate().await.unwrap();
    let ring = Keyring::new(MasterKey::generate().unwrap().0);
    let mut env = Secrets::default();
    env.0
        .insert("AIGW_SECRET_k1".into(), generate_api_key().unwrap().secret);
    env.0
        .insert("AIGW_SECRET_k2".into(), generate_api_key().unwrap().secret);
    let opts = ApplyOptions {
        api_keys: ApiKeyMode::Include,
        ..ApplyOptions::default()
    };

    // Ausgangslage: alles innerhalb der Grenzen
    let base = format!(
        "{GROUPS}{}{}{}",
        org(1000, "1000"),
        team("[gpt-4o]", 10000, 500, "100"),
        key("k1", "[gpt-4o-latest]", 5000, "50")
    );
    let docs = parse_documents(&base).unwrap();
    let r = apply(&store, &ring, &docs, &opts, &env).await.unwrap();
    assert!(r.applied);

    // Alle Verstöße gesammelt, jeweils am eigenen Dokument
    let bad = format!(
        "{GROUPS}{}{}{}{}",
        org(1000, "1000"),
        team("[gpt-4o]", 10000, 2000, "1500"),
        key("k1", "[gpt-4o-latest]", 5000, "50"),
        key("k2", "[gpt-4o, claude]", 20000, "200")
    );
    let found = issues(&store, &ring, &env, &bad).await;
    let l = lines(&found);
    assert_eq!(found.len(), 4, "{l:#?}");
    for want in [
        "ApiKey k2: limits[0]: tokens/minute = 20000 überschreitet das Limit von Team \"platform\" (10000)",
        "ApiKey k2: allowedModels[1]: Modell \"claude\" ist für Team \"platform\" nicht freigegeben",
        "Team platform: limits[1]: requests/minute = 2000 überschreitet das Limit von Organisation \"acme\" (1000)",
        "Team platform: budget: 1500 je month",
    ] {
        assert!(l.iter().any(|m| m.starts_with(want)), "{want}: {l:#?}");
    }
    assert!(found.iter().all(|i| i.document.is_some()));

    // Team senken, Key nicht im Manifest: Verstoß des bestehenden Keys am Team-Dokument
    let lower = format!(
        "{GROUPS}{}{}",
        org(1000, "1000"),
        team("[claude]", 4000, 500, "100")
    );
    let found = issues(&store, &ring, &env, &lower).await;
    let l = lines(&found);
    assert_eq!(found.len(), 2, "{l:#?}");
    assert!(
        l[0].starts_with("Team platform: bestehend: Key \"k1\": limits[0]"),
        "{l:#?}"
    );
    assert!(l[1].contains("Key \"k1\": allowedModels[0]"), "{l:#?}");

    // Organisation senken: Team und Key (nicht im Manifest) am Organisations-Dokument
    let org_low = format!("{GROUPS}{}", org(400, "40"));
    let found = issues(&store, &ring, &env, &org_low).await;
    let l = lines(&found);
    assert_eq!(found.len(), 3, "{l:#?}");
    assert!(
        l.iter()
            .all(|m| m.starts_with("Organization acme: bestehend: ")),
        "{l:#?}"
    );
    assert!(l.iter().any(|m| m.contains("Team \"platform\": limits[1]")));
    assert!(l.iter().any(|m| m.contains("Team \"platform\": budget")));
    assert!(l.iter().any(|m| m.contains("Key \"k1\": budget")));

    // Bereits bestehender Verstoß (z. B. per force angelegt) blockiert unveränderte Manifeste
    // nicht, wohl aber, wenn der Key selbst im Manifest steht
    store
        .create_api_key(
            "test",
            ObjectMeta::named("legacy"),
            ApiKeySpec {
                organization: "acme".into(),
                team: Some("platform".into()),
                limits: vec![LimitSpec {
                    metric: LimitMetric::Tokens,
                    limit: 99_999,
                    window: LimitWindow::Minute,
                    model_group: None,
                }],
                ..ApiKeySpec::default()
            },
        )
        .await
        .unwrap();
    let docs = parse_documents(&base).unwrap();
    plan(&store, &ring, &docs, &opts, &env).await.unwrap();
    let mut with_legacy = base.clone();
    with_legacy.push_str(
        r"
---
apiVersion: aigw/v1alpha1
kind: ApiKey
metadata: {name: legacy}
spec:
  organization: acme
  team: platform
  limits: [{metric: tokens, limit: 99999, window: minute}]
",
    );
    let found = issues(&store, &ring, &env, &with_legacy).await;
    assert_eq!(lines(&found).len(), 1, "{:#?}", lines(&found));
    assert_eq!(found[0].name.as_deref(), Some("legacy"));
}

/// S-09/F203: payloadModeMax/payloadModeLocked im GitOps-Plan, Rundlauf über den Store.
#[tokio::test]
async fn payload_policy() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    store.migrate().await.unwrap();
    let ring = Keyring::new(MasterKey::generate().unwrap().0);
    let mut env = Secrets::default();
    env.0
        .insert("AIGW_SECRET_k1".into(), generate_api_key().unwrap().secret);
    let opts = ApplyOptions {
        api_keys: ApiKeyMode::Include,
        ..ApplyOptions::default()
    };
    let doc = |org: &str, team: &str, key: &str| {
        format!(
            r#"
apiVersion: aigw/v1alpha1
kind: Organization
metadata: {{name: acme}}
spec: {{{org}}}
---
apiVersion: aigw/v1alpha1
kind: Team
metadata: {{name: platform}}
spec: {{organization: acme{team}}}
---
apiVersion: aigw/v1alpha1
kind: ApiKey
metadata: {{name: k1}}
spec:
  organization: acme
  team: platform
  secret: {{valueFrom: {{env: AIGW_SECRET_k1}}}}
{key}
"#
        )
    };
    // Verstöße: Organisation über eigenem Maximum, Team-Maximum über dem der Organisation,
    // Key über dem Maximum.
    let bad = doc(
        "payloadMode: full, payloadModeMax: metadata",
        ", payloadModeMax: full",
        "  payloadMode: masked",
    );
    let found = issues(&store, &ring, &env, &bad).await;
    let l = lines(&found);
    assert_eq!(found.len(), 3, "{l:#?}");
    for want in [
        "Organization acme: payloadMode: full überschreitet die eigene Obergrenze",
        "Team platform: payloadModeMax: full überschreitet die Obergrenze payloadModeMax = metadata",
        "ApiKey k1: payloadMode: masked überschreitet die Obergrenze payloadModeMax = metadata",
    ] {
        assert!(l.iter().any(|m| m.starts_with(want)), "{want}: {l:#?}");
    }
    // Gesperrt: Key darf payloadMode nicht setzen.
    let locked = doc(
        "payloadModeMax: masked",
        ", payloadMode: metadata, payloadModeLocked: true",
        "  payloadMode: \"off\"",
    );
    let l = lines(&issues(&store, &ring, &env, &locked).await);
    assert_eq!(l.len(), 1, "{l:#?}");
    assert!(
        l[0].starts_with("ApiKey k1: payloadMode: Team \"platform\" hat payloadMode gesperrt"),
        "{l:#?}"
    );
    // Gültig: angewendet, Felder kommen unverändert zurück.
    let good = doc(
        "payloadModeMax: masked",
        ", payloadMode: metadata, payloadModeLocked: true",
        "",
    );
    let docs = parse_documents(&good).unwrap();
    assert!(
        apply(&store, &ring, &docs, &opts, &env)
            .await
            .unwrap()
            .applied
    );
    let t: aigw_core::resource::Object<aigw_core::resource::TeamSpec> =
        store.get("platform").await.unwrap();
    assert!(t.spec.payload_mode_locked);
    assert_eq!(
        t.spec.payload_mode,
        Some(aigw_core::observe::PayloadMode::Metadata)
    );
    let o: aigw_core::resource::Object<aigw_core::resource::OrganizationSpec> =
        store.get("acme").await.unwrap();
    assert_eq!(
        o.spec.payload_mode_max,
        Some(aigw_core::observe::PayloadMode::Masked)
    );
    // Rundlauf: gleiches Manifest erneut – Organisation und Team unverändert
    let r = plan(&store, &ring, &docs, &opts, &env).await.unwrap();
    for c in &r.changes {
        if matches!(
            c.kind,
            aigw_core::resource::ResourceKind::Organization
                | aigw_core::resource::ResourceKind::Team
        ) {
            assert_eq!(
                c.action,
                aigw_store::gitops::ChangeAction::Unchanged,
                "{c:?}"
            );
        }
    }
}

#[tokio::test]
async fn sqlite() {
    run(Store::connect("sqlite::memory:").await.unwrap()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres() {
    let Ok(base) = std::env::var("AIGW_TEST_POSTGRES_URL") else {
        eprintln!("AIGW_TEST_POSTGRES_URL nicht gesetzt – Postgres-Test übersprungen");
        return;
    };
    let db = format!("aigw_gitops_bounds_{}", new_id().simple());
    let admin = sqlx::PgPool::connect(&base).await.unwrap();
    sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {db}")))
        .execute(&admin)
        .await
        .unwrap();
    let (prefix, _) = base.rsplit_once('/').expect("URL mit Datenbankpfad");
    let store = Store::connect(&format!("{prefix}/{db}")).await.unwrap();
    run(store.clone()).await;
    store.close().await;
    sqlx::query(AssertSqlSafe(format!("DROP DATABASE {db} WITH (FORCE)")))
        .execute(&admin)
        .await
        .unwrap();
}
