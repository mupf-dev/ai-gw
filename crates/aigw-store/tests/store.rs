// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Migrationen, CRUD, Audit, Verschlüsselung und Snapshot gegen SQLite (in-memory) und –
//! falls `AIGW_TEST_POSTGRES_URL` gesetzt ist – gegen Postgres (eigene Wegwerf-Datenbank).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::collections::BTreeMap;

use aigw_core::resource::{
    ApiKeySpec, ApiKeyState, Budget, BudgetPeriod, DeploymentSpec, LimitMetric, LimitSpec,
    LimitWindow, McpGrantSpec, McpServerSpec, ModelGroupSpec, ObjectMeta, OrganizationSpec,
    PluginBindingSpec, PriceSpec, ProviderCredentialSpec, ResourceKind, RoutingStrategy, Scope,
    ScopeLevel, SecretRef, TeamSpec, TokenPrices, UserSpec, new_id,
};
use aigw_core::types::ProviderKind;
use aigw_store::{Keyring, MasterKey, Store, StoreError, hash_api_key, load_snapshot};
use sqlx::AssertSqlSafe;
use time::macros::datetime;

const ACTOR: &str = "test";

fn meta(name: &str) -> ObjectMeta {
    ObjectMeta::named(name)
}

fn deployment(group: &str, model: &str, priority: u32) -> DeploymentSpec {
    DeploymentSpec {
        model_group: group.into(),
        provider: ProviderKind::OpenAi,
        endpoint: None,
        provider_model: model.into(),
        credential: Some("openai-prod".into()),
        weight: 1,
        priority,
        capacity: Default::default(),
        enabled: true,
        settings: Default::default(),
    }
}

/// Gemeinsamer Ablauf für beide Datenbanken.
async fn run(store: Store) {
    store.migrate().await.unwrap();
    store.migrate().await.unwrap();
    let v0 = store.config_version().await.unwrap();
    let (master, master_hex) = MasterKey::generate().unwrap();
    let ring = Keyring::new(master);

    // --- Mandanten -----------------------------------------------------------------------
    let org = store
        .create(
            ACTOR,
            meta("acme"),
            OrganizationSpec {
                display_name: Some("ACME".into()),
                // F30/F185: vererbbare Einstellungen
                payload_mode: Some(aigw_core::observe::PayloadMode::Full),
                expose_reasoning: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(org.id.get_version_num(), 7);
    assert_eq!(
        org.spec.payload_mode,
        Some(aigw_core::observe::PayloadMode::Full)
    );
    assert_eq!(org.spec.expose_reasoning, Some(true));
    assert!(org.metadata.created_at.is_some());

    let team_spec = TeamSpec {
        organization: "acme".into(),
        allowed_models: vec!["gpt-4o".into()],
        limits: vec![
            LimitSpec {
                metric: LimitMetric::Tokens,
                limit: 100_000,
                window: LimitWindow::Minute,
                model_group: None,
            },
            // F25: Kalender- und 30-Tage-Fenster
            LimitSpec {
                metric: LimitMetric::Requests,
                limit: 10_000,
                window: LimitWindow::CalendarMonth,
                model_group: None,
            },
            LimitSpec {
                metric: LimitMetric::Requests,
                limit: 9_000,
                window: LimitWindow::Days30,
                model_group: None,
            },
        ],
        budget: Some(Budget {
            amount: "100.50".parse().unwrap(),
            period: BudgetPeriod::Month,
            warn_at_percent: vec![80],
            hard_limit: true,
            enforcement: Default::default(),
        }),
        payload_mode: Some(aigw_core::observe::PayloadMode::Metadata),
        expose_reasoning: Some(false),
        ..Default::default()
    };
    let team = store
        .create(ACTOR, meta("platform"), team_spec.clone())
        .await
        .unwrap();
    assert_eq!(
        team.spec, team_spec,
        "Verweise werden als Namen zurückgegeben"
    );

    let err = store
        .create(
            ACTOR,
            meta("ghost"),
            TeamSpec {
                organization: "nope".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::InvalidReference {
                kind: ResourceKind::Organization,
                ..
            }
        ),
        "{err}"
    );

    let err = store
        .create(ACTOR, meta("acme"), OrganizationSpec::default())
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Conflict(_)), "{err}");
    assert!(matches!(
        store
            .create(ACTOR, meta("Bad Name"), OrganizationSpec::default())
            .await,
        Err(StoreError::Invalid(_))
    ));

    store
        .create(
            ACTOR,
            meta("alice"),
            UserSpec {
                oidc_issuer: Some("https://idp".into()),
                oidc_subject: Some("123".into()),
                email: Some("alice@example.org".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // --- Zugangsdaten (verschlüsselt) ----------------------------------------------------
    let cred = store
        .create_credential(
            ACTOR,
            &ring,
            meta("openai-prod"),
            ProviderCredentialSpec {
                provider: ProviderKind::OpenAi,
                entra: None,
                data: BTreeMap::from([(
                    "apiKey".to_owned(),
                    SecretRef::value("sk-upstream-geheim"),
                )]),
            },
        )
        .await
        .unwrap();
    assert_eq!(cred.spec.fields, vec!["apiKey".to_owned()]);
    assert_eq!(cred.spec.key_id, ring.primary_id());
    assert!(!serde_json::to_string(&cred).unwrap().contains("geheim"));

    // --- Modelle ---------------------------------------------------------------------------
    store
        .create(
            ACTOR,
            meta("gpt-4o"),
            ModelGroupSpec {
                aliases: vec!["gpt-4o-latest".into()],
                routing: RoutingStrategy::LeastLatency,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    // Alias kollidiert mit Name einer anderen Gruppe
    let err = store
        .create(
            ACTOR,
            meta("other"),
            ModelGroupSpec {
                aliases: vec!["gpt-4o".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Conflict(_)), "{err}");
    assert!(
        matches!(
            store.get::<ModelGroupSpec>("other").await,
            Err(StoreError::NotFound { .. })
        ),
        "Transaktion zurückgerollt"
    );

    store
        .create(
            ACTOR,
            meta("gpt-4o-ptu"),
            deployment("gpt-4o", "gpt-4o-2024-08-06", 0),
        )
        .await
        .unwrap();
    store
        .create(
            ACTOR,
            meta("gpt-4o-payg"),
            deployment("gpt-4o", "gpt-4o-2024-08-06", 1),
        )
        .await
        .unwrap();
    let mut disabled = deployment("gpt-4o", "gpt-4o-mini", 2);
    disabled.enabled = false;
    store
        .create(ACTOR, meta("gpt-4o-off"), disabled)
        .await
        .unwrap();

    for (name, from, input) in [
        ("gpt-4o-2024", datetime!(2024-08-06 0:00 UTC), "2.50"),
        ("gpt-4o-2026", datetime!(2026-01-01 0:00 UTC), "2.00"),
    ] {
        store
            .create(
                ACTOR,
                meta(name),
                PriceSpec {
                    provider: Some(ProviderKind::OpenAi),
                    model: "gpt-4o-2024-08-06".into(),
                    valid_from: from,
                    per_million_tokens: TokenPrices {
                        input: Some(input.parse().unwrap()),
                        output: Some("10".parse().unwrap()),
                        ..Default::default()
                    },
                    per_request: None,
                    per_image: None,
                    per_audio_minute: None,
                    per_million_characters: None,
                },
            )
            .await
            .unwrap();
    }

    // --- Keys ------------------------------------------------------------------------------
    let created = store
        .create_api_key(
            ACTOR,
            meta("ci-key"),
            ApiKeySpec {
                organization: "acme".into(),
                team: Some("platform".into()),
                user: Some("alice".into()),
                metadata: BTreeMap::from([("cost-center".to_owned(), "4711".to_owned())]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(created.secret.starts_with("sk-aigw-"));
    assert_eq!(created.key.status.state, ApiKeyState::Active);
    assert!(created.secret.contains(&created.key.status.prefix));
    let other_key = store
        .create_api_key(
            ACTOR,
            meta("revoked-key"),
            ApiKeySpec {
                organization: "acme".into(),
                ..ApiKeySpec::default()
            },
        )
        .await
        .unwrap();
    store.revoke_api_key(ACTOR, "revoked-key").await.unwrap();

    // --- Plugins & MCP ---------------------------------------------------------------------
    store
        .create(
            ACTOR,
            meta("pii-team"),
            PluginBindingSpec {
                inherit: None,
                locked: false,
                fail_mode: None,
                timeout_ms: None,
                output_reject_mode: None,
                scope: Scope {
                    level: ScopeLevel::Team,
                    target: Some("platform".into()),
                },
                plugin: "pii".into(),
                order: 10,
                enabled: true,
                config: serde_json::json!({"mode": "mask"})
                    .as_object()
                    .cloned()
                    .unwrap(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        store
            .create(
                ACTOR,
                meta("bad-scope"),
                PluginBindingSpec {
                    inherit: None,
                    locked: false,
                    fail_mode: None,
                    timeout_ms: None,
                    output_reject_mode: None,
                    scope: Scope {
                        level: ScopeLevel::Global,
                        target: Some("x".into())
                    },
                    plugin: "pii".into(),
                    order: 0,
                    enabled: true,
                    config: Default::default(),
                }
            )
            .await,
        Err(StoreError::Invalid(_))
    ));
    store
        .create(
            ACTOR,
            meta("github"),
            McpServerSpec {
                secret: None,
                url: "https://mcp.example.org/mcp".into(),
                transport: Default::default(),
                credential: None,
                enabled: true,
                settings: Default::default(),
            },
        )
        .await
        .unwrap();
    let grant = store
        .create(
            ACTOR,
            meta("github-ci"),
            McpGrantSpec {
                server: "github".into(),
                subject: Scope {
                    level: ScopeLevel::ApiKey,
                    target: Some("ci-key".into()),
                },
                tools: vec!["search_code".into()],
            },
        )
        .await
        .unwrap();
    assert_eq!(grant.spec.subject.target.as_deref(), Some("ci-key"));
    let binding = store.get::<PluginBindingSpec>("pii-team").await.unwrap();
    assert_eq!(binding.spec.scope.target.as_deref(), Some("platform"));

    // --- Update + Audit --------------------------------------------------------------------
    let mut updated = team_spec.clone();
    updated.display_name = Some("Plattform".into());
    let mut m = meta("platform");
    m.labels.insert("env".into(), "prod".into());
    let t2 = store.update(ACTOR, m, updated.clone()).await.unwrap();
    assert_eq!(t2.spec, updated);
    assert_eq!(t2.id, team.id);
    assert_eq!(t2.metadata.labels["env"], "prod");

    let v1 = store.config_version().await.unwrap();
    let audit = store.audit_log(1000).await.unwrap();
    // jede erfolgreiche Änderung = genau eine Version (fehlgeschlagene zählen nicht)
    let writes = i64::try_from(audit.len()).unwrap();
    assert_eq!(v1 - v0, writes);
    assert!(audit.iter().all(|e| e.actor == ACTOR));
    let all_audit = serde_json::to_string(&audit).unwrap();
    assert!(
        !all_audit.contains("sk-upstream-geheim"),
        "kein Klartext im Audit-Log"
    );
    assert!(
        !all_audit.contains(&created.secret),
        "kein Key-Klartext im Audit-Log"
    );
    let last = &audit[0];
    assert_eq!(last.action, "update");
    assert_eq!(last.config_version, v1);
    assert!(last.before.is_some() && last.after.is_some());

    // --- Snapshot --------------------------------------------------------------------------
    let snap = load_snapshot(&store, &ring).await.unwrap();
    assert_eq!(snap.version, v1);
    let ctx = snap
        .key(&hash_api_key(&created.secret))
        .expect("Key im Snapshot");
    assert_eq!(ctx.key.metadata.name, "ci-key");
    assert_eq!(ctx.team.as_ref().unwrap().metadata.name, "platform");
    assert_eq!(ctx.organization.as_ref().unwrap().metadata.name, "acme");
    assert_eq!(snap.keys.len(), 1, "gesperrte Keys fehlen");
    assert!(!snap.keys.values().any(|k| k.key.id == other_key.key.id));

    let g = snap.model_group("gpt-4o-latest").expect("Alias aufgelöst");
    assert_eq!(g.group.metadata.name, "gpt-4o");
    assert!(ctx.allows_model(g));
    let names: Vec<_> = g
        .deployments
        .iter()
        .map(|d| d.deployment.metadata.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["gpt-4o-ptu", "gpt-4o-payg"],
        "nach Priorität, ohne deaktivierte"
    );
    let api_key = g.deployments[0]
        .credential
        .as_ref()
        .unwrap()
        .get("apiKey")
        .unwrap();
    assert_eq!(api_key.expose(), "sk-upstream-geheim");

    let p = snap
        .price_at(
            ProviderKind::OpenAi,
            "gpt-4o-2024-08-06",
            datetime!(2025-06-01 0:00 UTC),
        )
        .unwrap();
    assert_eq!(p.metadata.name, "gpt-4o-2024");
    let p = snap
        .price_at(
            ProviderKind::OpenAi,
            "gpt-4o-2024-08-06",
            datetime!(2026-06-01 0:00 UTC),
        )
        .unwrap();
    assert_eq!(p.spec.per_million_tokens.input.unwrap().to_string(), "2");
    assert_eq!(snap.plugin_bindings.len(), 1);
    assert_eq!(snap.mcp_grants.len(), 1);

    // --- Rotation --------------------------------------------------------------------------
    let (new_key, _) = MasterKey::generate().unwrap();
    let old_id = ring.primary_id().to_owned();
    let rotated = Keyring::new(new_key).with_previous(MasterKey::from_hex(&master_hex).unwrap());
    assert_eq!(store.rotate_credentials(ACTOR, &rotated).await.unwrap(), 1);
    assert_eq!(store.rotate_credentials(ACTOR, &rotated).await.unwrap(), 0);
    let c = store.get_credential("openai-prod").await.unwrap();
    assert_ne!(c.spec.key_id, old_id);
    let data = store.decrypt_credentials(&rotated).await.unwrap();
    assert_eq!(
        data["openai-prod"].1.get("apiKey").unwrap().expose(),
        "sk-upstream-geheim"
    );

    // --- Löschen ---------------------------------------------------------------------------
    let err = store
        .delete(ACTOR, ResourceKind::ModelGroup, "gpt-4o")
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Conflict(_)),
        "Deployments verweisen noch: {err}"
    );
    for d in ["gpt-4o-ptu", "gpt-4o-payg", "gpt-4o-off"] {
        store
            .delete(ACTOR, ResourceKind::Deployment, d)
            .await
            .unwrap();
    }
    store
        .delete(ACTOR, ResourceKind::ModelGroup, "gpt-4o")
        .await
        .unwrap();
    // Name/Alias wieder frei
    store
        .create(ACTOR, meta("gpt-4o-latest"), ModelGroupSpec::default())
        .await
        .unwrap();

    // Team löschen: Key verweist noch → Konflikt; nach Löschen des Keys geht es und die
    // Plugin-Bindung auf das Team verschwindet mit.
    assert!(
        store
            .delete(ACTOR, ResourceKind::Team, "platform")
            .await
            .is_err()
    );
    store
        .delete(ACTOR, ResourceKind::ApiKey, "ci-key")
        .await
        .unwrap();
    assert!(
        store.list::<McpGrantSpec>().await.unwrap().is_empty(),
        "Freigabe des Keys mitgelöscht"
    );
    store
        .delete(ACTOR, ResourceKind::Team, "platform")
        .await
        .unwrap();
    assert!(store.list::<PluginBindingSpec>().await.unwrap().is_empty());
    assert!(matches!(
        store.delete(ACTOR, ResourceKind::Team, "platform").await,
        Err(StoreError::NotFound { .. })
    ));

    let snap = load_snapshot(&store, &rotated).await.unwrap();
    assert!(snap.keys.is_empty());
    assert!(snap.model_group("gpt-4o").is_none());
    assert!(snap.model_group("gpt-4o-latest").is_some());

    store.close().await;
}

#[tokio::test]
async fn sqlite_in_memory() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    run(store).await;
}

#[tokio::test]
async fn sqlite_file() {
    let dir = std::env::temp_dir().join(format!("aigw-store-test-{}", new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let url = format!("sqlite://{}/aigw.db?mode=rwc", dir.display());
    let store = Store::connect(&url).await.unwrap();
    run(store).await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn postgres() {
    let Ok(base) = std::env::var("AIGW_TEST_POSTGRES_URL") else {
        eprintln!("AIGW_TEST_POSTGRES_URL nicht gesetzt – Postgres-Test übersprungen");
        return;
    };
    // Eigene Wegwerf-Datenbank je Lauf.
    let db = format!("aigw_test_{}", new_id().simple());
    let admin = sqlx::PgPool::connect(&base).await.unwrap();
    sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {db}")))
        .execute(&admin)
        .await
        .unwrap();
    let (prefix, _) = base.rsplit_once('/').expect("URL mit Datenbankpfad");
    let store = Store::connect(&format!("{prefix}/{db}")).await.unwrap();
    assert_eq!(store.backend(), aigw_store::Backend::Postgres);
    run(store).await;
    sqlx::query(AssertSqlSafe(format!("DROP DATABASE {db} WITH (FORCE)")))
        .execute(&admin)
        .await
        .unwrap();
}
