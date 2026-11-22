// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Store-Umbau AIGW-73: gemeinsame Tests für **beide** Backends (SQLite in-memory, SQLite-Datei
//! und – mit `AIGW_TEST_POSTGRES_URL` – Postgres in einer Wegwerf-Datenbank).
//!
//! Abgedeckt: native Typen (UUID, Zeitpunkte, JSON, Wahrheitswerte, Bytes) in Rundreisen,
//! F3 (Organisation der Keys), F42 (SQL-Pagination mit Filtern und Geltungsbereich), F54
//! (`valueFrom`-Verweise), F34 (idempotente Batches), Analytics und Audit-Filter.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr,
    clippy::too_many_lines
)]

use std::collections::BTreeMap;

use aigw_billing::{BudgetKey, BudgetLevel};
use aigw_core::resource::{
    ApiKeySpec, ApiKeyState, BudgetPeriod, DeploymentSpec, ModelGroupSpec, ObjectMeta,
    OrganizationSpec, PriceSpec, ProviderCredentialSpec, ResourceKind, SecretRef, SecretString,
    TeamSpec, UserSpec, ValueFrom, new_id,
};
use aigw_core::types::ProviderKind;
use aigw_store::analytics::{Grouping, KeyFilter, LogQuery, RollupDim, RollupQuery};
use aigw_store::gitops::{ApiKeyMode, export_yaml};
use aigw_store::observe::{
    AttemptRow, HOUR_MS, MINUTE_MS, PayloadRow, RequestLogRow, RollupCounts, RollupKey,
};
use aigw_store::{
    AuditQuery, AuditScope, Keyring, ListFilter, ListQuery, MasterKey, ScopeRule, Store,
    StoreError, load_snapshot,
};
use sqlx::AssertSqlSafe;
use time::macros::datetime;

const ACTOR: &str = "test";

fn meta(name: &str) -> ObjectMeta {
    ObjectMeta::named(name)
}

fn labeled(name: &str, labels: &[(&str, &str)]) -> ObjectMeta {
    let mut m = ObjectMeta::named(name);
    m.labels = labels
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    m
}

fn q(limit: u32) -> ListQuery {
    ListQuery {
        limit,
        ..ListQuery::default()
    }
}

fn names<T>(items: &[aigw_core::resource::Object<T>]) -> Vec<String> {
    items.iter().map(|o| o.metadata.name.clone()).collect()
}

fn key(org: &str, team: Option<&str>) -> ApiKeySpec {
    ApiKeySpec {
        organization: org.into(),
        team: team.map(Into::into),
        ..ApiKeySpec::default()
    }
}

async fn run(store: Store) {
    store.migrate().await.unwrap();
    store.migrate().await.unwrap();
    assert_eq!(store.config_version().await.unwrap(), 0);
    let (master, _) = MasterKey::generate().unwrap();
    let ring = Keyring::new(master);

    // --- Mandanten ------------------------------------------------------------------------
    for (o, plan) in [("acme", "gold"), ("beta", "free"), ("gamma", "gold")] {
        store
            .create(
                ACTOR,
                labeled(o, &[("plan", plan)]),
                OrganizationSpec::default(),
            )
            .await
            .unwrap();
    }
    for (t, o, env) in [
        ("ops", "beta", "prod"),
        ("platform", "acme", "prod"),
        ("research", "acme", "dev"),
        ("web", "acme", "prod"),
    ] {
        store
            .create(
                ACTOR,
                labeled(t, &[("env", env)]),
                TeamSpec {
                    organization: o.into(),
                    ..TeamSpec::default()
                },
            )
            .await
            .unwrap();
    }
    store
        .create(
            ACTOR,
            meta("alice"),
            UserSpec {
                email: Some("Alice@Example.org".into()),
                ..UserSpec::default()
            },
        )
        .await
        .unwrap();

    // --- F3: Organisation der Keys -----------------------------------------------------------
    let err = store
        .create_api_key(ACTOR, meta("no-org"), key("", None))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)), "{err}");
    let err = store
        .create_api_key(ACTOR, meta("wrong-org"), key("beta", Some("platform")))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)), "{err}");
    let err = store
        .create_api_key(ACTOR, meta("ghost-org"), key("nope", None))
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
    let v_before = store.config_version().await.unwrap();
    for (n, org, team) in [
        ("k-ops", "beta", Some("ops")),
        ("k-plat-1", "acme", Some("platform")),
        ("k-plat-2", "acme", Some("platform")),
        ("k-res", "acme", Some("research")),
        ("k-solo", "gamma", None),
    ] {
        let c = store
            .create_api_key(ACTOR, meta(n), key(org, team))
            .await
            .unwrap();
        assert_eq!(c.key.spec.organization, org);
        assert_eq!(c.key.spec.team.as_deref(), team);
    }
    assert_eq!(store.config_version().await.unwrap(), v_before + 5);
    store.revoke_api_key(ACTOR, "k-plat-2").await.unwrap();

    // Snapshot: Keys ohne Team erben die Organisation.
    let snap = load_snapshot(&store, &ring).await.unwrap();
    let solo = snap
        .keys
        .values()
        .find(|k| k.key.metadata.name == "k-solo")
        .unwrap();
    assert!(solo.team.is_none());
    assert_eq!(
        solo.organization.as_ref().map(|o| o.metadata.name.as_str()),
        Some("gamma")
    );
    assert_eq!(snap.keys.len(), 4, "gesperrter Key fehlt im Index");

    // Team zieht um → seine Keys ziehen mit (ON UPDATE CASCADE).
    let research = store.get::<TeamSpec>("research").await.unwrap();
    let mut spec = research.spec.clone();
    spec.organization = "beta".into();
    store
        .update(ACTOR, research.metadata.clone(), spec)
        .await
        .unwrap();
    assert_eq!(
        store.get_api_key("k-res").await.unwrap().spec.organization,
        "beta"
    );
    let mut spec = store.get::<TeamSpec>("research").await.unwrap().spec;
    spec.organization = "acme".into();
    store.update(ACTOR, research.metadata, spec).await.unwrap();

    // Organisation mit Keys lässt sich nicht löschen.
    let err = store
        .delete(ACTOR, ResourceKind::Organization, "gamma")
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Conflict(_)), "{err}");

    // --- F42: SQL-Pagination -----------------------------------------------------------------
    let p1 = store.list_page::<TeamSpec>(&q(2)).await.unwrap();
    assert_eq!(names(&p1.items), ["ops", "platform"]);
    assert_eq!(p1.next.as_deref(), Some("platform"));
    let p2 = store
        .list_page::<TeamSpec>(&ListQuery {
            after: p1.next.clone(),
            ..q(2)
        })
        .await
        .unwrap();
    assert_eq!(names(&p2.items), ["research", "web"]);
    assert_eq!(p2.next, None);
    let by_org = store
        .list_page::<TeamSpec>(&ListQuery {
            filters: vec![ListFilter::Organization("acme".into())],
            labels: vec![("env".into(), Some("prod".into()))],
            ..q(50)
        })
        .await
        .unwrap();
    assert_eq!(names(&by_org.items), ["platform", "web"]);
    let has_label = store
        .list_page::<OrganizationSpec>(&ListQuery {
            labels: vec![("plan".into(), None)],
            search: Some("a".into()),
            ..q(50)
        })
        .await
        .unwrap();
    assert_eq!(names(&has_label.items), ["acme", "beta", "gamma"]);
    let none = store
        .list_page::<OrganizationSpec>(&ListQuery {
            labels: vec![("missing".into(), None)],
            ..q(50)
        })
        .await
        .unwrap();
    assert!(none.items.is_empty());

    // Geltungsbereich: Org-Regel sieht Org + Teams + Keys der Teams; Team-Regel nur ihr Team.
    let org_scope = AuditScope::Within(vec![ScopeRule {
        organization: Some("acme".into()),
        team: None,
    }]);
    let team_scope = AuditScope::Within(vec![ScopeRule {
        organization: None,
        team: Some("ops".into()),
    }]);
    let orgs = store
        .list_page::<OrganizationSpec>(&ListQuery {
            scope: org_scope.clone(),
            ..q(50)
        })
        .await
        .unwrap();
    assert_eq!(names(&orgs.items), ["acme"]);
    let teams = store
        .list_page::<TeamSpec>(&ListQuery {
            scope: team_scope.clone(),
            ..q(50)
        })
        .await
        .unwrap();
    assert_eq!(names(&teams.items), ["ops"]);
    let keys = store
        .list_api_keys_page(&ListQuery {
            scope: org_scope.clone(),
            ..q(50)
        })
        .await
        .unwrap();
    let kn: Vec<_> = keys
        .items
        .iter()
        .map(|k| k.metadata.name.as_str())
        .collect();
    assert_eq!(kn, ["k-plat-1", "k-plat-2", "k-res"]);
    let keys = store
        .list_api_keys_page(&ListQuery {
            filters: vec![
                ListFilter::Team("platform".into()),
                ListFilter::State(ApiKeyState::Active.as_str().into()),
            ],
            ..q(50)
        })
        .await
        .unwrap();
    assert_eq!(keys.items.len(), 1);
    assert_eq!(keys.items[0].metadata.name, "k-plat-1");
    let prefix = keys.items[0].status.prefix.clone();
    let by_prefix = store
        .list_api_keys_page(&ListQuery {
            search: Some(prefix),
            ..q(50)
        })
        .await
        .unwrap();
    assert_eq!(by_prefix.items.len(), 1);
    let solo = store
        .list_api_keys_page(&ListQuery {
            filters: vec![ListFilter::Organization("gamma".into())],
            ..q(50)
        })
        .await
        .unwrap();
    assert_eq!(solo.items.len(), 1);
    let users = store
        .list_page::<UserSpec>(&ListQuery {
            filters: vec![ListFilter::Email("alice@example.ORG".into())],
            ..q(50)
        })
        .await
        .unwrap();
    assert_eq!(names(&users.items), ["alice"]);
    let users = store
        .list_page::<UserSpec>(&ListQuery {
            scope: org_scope.clone(),
            ..q(50)
        })
        .await
        .unwrap();
    assert!(users.items.is_empty(), "Nutzer sind global");
    assert!(matches!(
        store
            .list_page::<UserSpec>(&ListQuery {
                filters: vec![ListFilter::Model("x".into())],
                ..q(50)
            })
            .await,
        Err(StoreError::Invalid(_))
    ));

    // Katalog: Modellgruppen (Suche in Aliassen), Deployments, Preise (gültige Version).
    for (g, aliases) in [
        ("claude", vec!["sonnet"]),
        ("gpt-4o", vec!["gpt-4o-latest"]),
    ] {
        store
            .create(
                ACTOR,
                meta(g),
                ModelGroupSpec {
                    aliases: aliases.into_iter().map(Into::into).collect(),
                    ..ModelGroupSpec::default()
                },
            )
            .await
            .unwrap();
    }
    let groups = store
        .list_page::<ModelGroupSpec>(&ListQuery {
            search: Some("sonn".into()),
            ..q(50)
        })
        .await
        .unwrap();
    assert_eq!(names(&groups.items), ["claude"]);
    for (d, enabled) in [("d-a", true), ("d-b", false)] {
        store
            .create(
                ACTOR,
                meta(d),
                DeploymentSpec {
                    model_group: "gpt-4o".into(),
                    provider: ProviderKind::Mock,
                    endpoint: None,
                    provider_model: format!("mock-{d}"),
                    credential: None,
                    weight: 1,
                    priority: 0,
                    capacity: Default::default(),
                    enabled,
                    settings: Default::default(),
                },
            )
            .await
            .unwrap();
    }
    let deps = store
        .list_page::<DeploymentSpec>(&ListQuery {
            filters: vec![
                ListFilter::ModelGroup("gpt-4o".into()),
                ListFilter::Enabled(false),
            ],
            ..q(50)
        })
        .await
        .unwrap();
    assert_eq!(names(&deps.items), ["d-b"]);
    let deps = store
        .list_page::<DeploymentSpec>(&ListQuery {
            search: Some("mock-d-a".into()),
            ..q(50)
        })
        .await
        .unwrap();
    assert_eq!(names(&deps.items), ["d-a"]);
    for (n, from) in [
        ("p-2024", datetime!(2024-01-01 0:00 UTC)),
        ("p-2025", datetime!(2025-01-01 0:00 UTC)),
        ("p-2030", datetime!(2030-01-01 0:00 UTC)),
    ] {
        store
            .create(
                ACTOR,
                meta(n),
                PriceSpec {
                    provider: None,
                    model: "gpt-4o".into(),
                    valid_from: from,
                    per_million_tokens: Default::default(),
                    per_request: None,
                    per_image: None,
                    per_audio_minute: None,
                    per_million_characters: None,
                },
            )
            .await
            .unwrap();
    }
    let prices = store
        .list_page::<PriceSpec>(&ListQuery {
            filters: vec![
                ListFilter::Provider(String::new()),
                ListFilter::EffectiveAt(datetime!(2026-06-01 0:00 UTC)),
            ],
            ..q(50)
        })
        .await
        .unwrap();
    assert_eq!(names(&prices.items), ["p-2025"]);
    assert_eq!(
        prices.items[0].spec.valid_from,
        datetime!(2025-01-01 0:00 UTC),
        "TIMESTAMPTZ-Rundreise"
    );

    // --- F54: valueFrom-Verweise -------------------------------------------------------------
    let dir = std::env::temp_dir().join(format!("aigw-backends-{}", new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("key");
    std::fs::write(&file, "sk-from-file\n").unwrap();
    let file_s = file.display().to_string();
    store
        .create_credential(
            ACTOR,
            &ring,
            meta("openai"),
            ProviderCredentialSpec {
                provider: ProviderKind::OpenAi,
                entra: None,
                data: BTreeMap::from([
                    (
                        "apiKey".to_owned(),
                        SecretRef {
                            value: None,
                            value_from: Some(ValueFrom::File(file_s.clone())),
                        },
                    ),
                    ("organization".to_owned(), SecretRef::value("org-1")),
                ]),
            },
        )
        .await
        .unwrap();
    let refs = store.credential_refs().await.unwrap();
    assert_eq!(
        refs["openai"],
        BTreeMap::from([("apiKey".to_owned(), ValueFrom::File(file_s.clone()))])
    );
    let creds = store.decrypt_credentials(&ring).await.unwrap();
    assert_eq!(creds["openai"].1.0["apiKey"].expose(), "sk-from-file");
    let yaml = export_yaml(&store, ApiKeyMode::Include).await.unwrap();
    assert!(yaml.contains(&format!("file: {file_s}")), "{yaml}");
    assert!(
        yaml.contains("AIGW_SECRET_"),
        "Klartext-Feld als Platzhalter: {yaml}"
    );
    assert!(yaml.contains("organization: gamma"), "{yaml}");
    // PATCH mit Klartext entfernt den Verweis des Felds.
    store
        .patch_credential(
            ACTOR,
            &ring,
            "openai",
            None,
            None,
            None,
            BTreeMap::from([("apiKey".to_owned(), Some(SecretString::new("sk-neu")))]),
            aigw_store::Expect::Any,
        )
        .await
        .unwrap();
    assert!(store.credential_refs().await.unwrap()["openai"].is_empty());
    let _ = std::fs::remove_dir_all(dir);

    // --- Audit: Filter über Name und ID (UUID) -----------------------------------------------
    let k = store.get_api_key("k-solo").await.unwrap();
    for object in [
        k.metadata.name.clone(),
        k.id.to_string(),
        "kein-uuid".into(),
    ] {
        let page = store
            .query_audit_log(&AuditQuery {
                object: Some(object.clone()),
                limit: 50,
                ..AuditQuery::default()
            })
            .await
            .unwrap();
        let want = usize::from(object != "kein-uuid");
        assert_eq!(page.items.len(), want, "{object}");
    }
    let page = store
        .query_audit_log(&AuditQuery {
            scope: AuditScope::Within(vec![ScopeRule {
                organization: Some("gamma".into()),
                team: None,
            }]),
            limit: 50,
            ..AuditQuery::default()
        })
        .await
        .unwrap();
    assert!(
        page.items
            .iter()
            .any(|e| e.object_name.as_deref() == Some("k-solo")),
        "F3: Keys ohne Team liegen im Bereich ihrer Organisation"
    );
    assert!(
        page.items
            .iter()
            .all(|e| e.after.is_some() || e.before.is_some())
    );

    // --- Request-Log, Payload, Rollups (F34) ----------------------------------------------------
    let t0 = 1_767_225_600_000; // 2026-01-01T00:00:00Z
    let plat = store.get_api_key("k-plat-1").await.unwrap();
    let team_id = store
        .get::<TeamSpec>("platform")
        .await
        .unwrap()
        .id
        .to_string();
    let org_id = store
        .get::<OrganizationSpec>("acme")
        .await
        .unwrap()
        .id
        .to_string();
    let log = RequestLogRow {
        id: new_id().to_string(),
        at: t0 + 5 * MINUTE_MS + 123,
        endpoint: "openai-chat".into(),
        api_key_id: Some(plat.id.to_string()),
        team_id: Some(team_id.clone()),
        organization_id: Some(org_id.clone()),
        model_requested: Some("gpt-4o-latest".into()),
        model_group: Some("gpt-4o".into()),
        status: 200,
        input_tokens: 10,
        output_tokens: 5,
        cost: 2_500_000,
        latency_ms: 120,
        ttft_ms: Some(40),
        cache: Some("miss".into()),
        stream: true,
        usage_estimated: true,
        client_ip: Some("192.0.2.1".into()),
        client_request_id: Some("c-1".into()),
        parent_request_id: Some(new_id().to_string()),
        attempts: vec![AttemptRow {
            model_group: Some("gpt-4o".into()),
            deployment: "d-a".into(),
            deployment_id: None,
            error: Some("rate_limit".into()),
            upstream_status: Some(429),
            duration_ms: 12,
        }],
        ..RequestLogRow::default()
    };
    let payload = PayloadRow {
        id: log.id.clone(),
        at: log.at,
        mode: "masked".into(),
        payload: serde_json::json!({"messages": [{"role": "user", "content": "***"}]}),
    };
    let counts = RollupCounts {
        requests: 1,
        input_tokens: 10,
        output_tokens: 5,
        cost: 2_500_000,
        latency_ms_sum: 120,
        latency_hist: {
            let mut h = [0; 12];
            h[2] = 1;
            h
        },
        ..RollupCounts::default()
    };
    let rollups = vec![
        (
            RollupKey {
                bucket_start: t0 + 5 * MINUTE_MS,
                api_key_id: plat.id.to_string(),
                team_id: team_id.clone(),
                organization_id: org_id.clone(),
                model_group: "gpt-4o".into(),
                deployment_id: String::new(),
                error_class: String::new(),
            },
            counts.clone(),
        ),
        (
            RollupKey {
                bucket_start: t0 + 65 * MINUTE_MS,
                ..RollupKey::default()
            },
            counts.clone(),
        ),
    ];
    let batch = new_id();
    assert!(
        store
            .write_request_batch_once(batch, &[&log], &[&payload], &rollups)
            .await
            .unwrap()
    );
    // Wiederholung desselben Batches: nichts doppelt.
    assert!(
        !store
            .write_request_batch_once(batch, &[&log], &[&payload], &rollups)
            .await
            .unwrap()
    );
    let stored = store.get_request_log(&log.id).await.unwrap().unwrap();
    assert_eq!(stored, log, "Rundreise aller Typen");
    assert!(store.get_request_log("kein-uuid").await.unwrap().is_none());
    assert_eq!(
        store.get_request_payload(&log.id).await.unwrap().unwrap(),
        payload
    );
    let minute = store.list_rollups(t0, t0 + 2 * HOUR_MS).await.unwrap();
    assert_eq!(minute.len(), 2);
    assert_eq!(minute[0].1, counts, "Rollup einmal gezählt");
    assert_eq!(minute[1].0, rollups[1].0, "leere Schlüssel bleiben ''");
    let hourly = store
        .list_hourly_rollups(t0, t0 + 2 * HOUR_MS)
        .await
        .unwrap();
    assert_eq!(hourly.len(), 2);
    assert_eq!(hourly[0].0.bucket_start, t0);

    // Analytics: Buckets und Gruppen in beiden Dialekten gleich.
    let buckets = store
        .query_rollups(&RollupQuery {
            from_ms: t0,
            to_ms: t0 + 2 * HOUR_MS,
            step_ms: Some(HOUR_MS),
            grouping: Grouping::Raw(RollupDim::Team),
            filter: KeyFilter::default(),
            columns: vec!["requests", "cost"],
        })
        .await
        .unwrap();
    let got: Vec<(i64, &str, i64)> = buckets
        .iter()
        .map(|b| (b.bucket_ms, b.group.as_str(), b.counts.requests))
        .collect();
    assert_eq!(got, [(t0, team_id.as_str(), 1), (t0 + HOUR_MS, "", 1)]);
    let five = store
        .query_rollups(&RollupQuery {
            from_ms: t0,
            to_ms: t0 + 2 * HOUR_MS,
            step_ms: Some(5 * MINUTE_MS),
            grouping: Grouping::Mapped {
                dim: RollupDim::Team,
                groups: vec![("plat".into(), vec![team_id.clone()])],
                other: "__other__".into(),
            },
            filter: KeyFilter {
                teams: Some(vec![team_id.clone(), "kein-uuid".into()]),
                ..KeyFilter::default()
            },
            columns: vec![],
        })
        .await
        .unwrap();
    assert_eq!(five.len(), 1);
    assert_eq!(
        (five[0].bucket_ms, five[0].group.as_str()),
        (t0 + 5 * MINUTE_MS, "plat")
    );
    let total = store
        .query_rollups(&RollupQuery {
            from_ms: t0,
            to_ms: t0 + 2 * HOUR_MS,
            step_ms: None,
            grouping: Grouping::None,
            filter: KeyFilter {
                teams: Some(vec![String::new()]),
                ..KeyFilter::default()
            },
            columns: vec![],
        })
        .await
        .unwrap();
    assert_eq!(total.len(), 1);
    assert_eq!((total[0].bucket_ms, total[0].counts.requests), (0, 1));
    let logs = store
        .list_request_logs(&LogQuery {
            stream: Some(true),
            keys: KeyFilter {
                teams: Some(vec![team_id.clone()]),
                ..KeyFilter::default()
            },
            limit: 10,
            ..LogQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(logs.len(), 1);
    assert!(logs[0].has_payload);
    let logs = store
        .list_request_logs(&LogQuery {
            before: Some((log.at, log.id.clone())),
            limit: 10,
            ..LogQuery::default()
        })
        .await
        .unwrap();
    assert!(logs.is_empty(), "Cursor schließt den Eintrag selbst aus");
    let children = store
        .list_request_logs(&LogQuery {
            parent_request_id: log.parent_request_id.clone(),
            limit: 10,
            ..LogQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(children.len(), 1, "F191: Filter auf parent_request_id");
    let tomorrow = time::OffsetDateTime::now_utc().unix_timestamp() * 1000 + 24 * HOUR_MS;
    assert_eq!(store.purge_observe_batches(tomorrow).await.unwrap(), 1);
    assert_eq!(store.purge_request_logs(t0 + HOUR_MS).await.unwrap(), 1);

    // --- Budget-Ledger, Ereignis-Dedup, Cache-Ablage -----------------------------------------
    let bk = BudgetKey {
        level: BudgetLevel::Team,
        target: store.get::<TeamSpec>("platform").await.unwrap().id,
        period: BudgetPeriod::Month,
        period_start_ms: t0,
    };
    assert_eq!(
        store
            .sync_budget_usage(&[(bk, 7)], &[bk], t0)
            .await
            .unwrap(),
        [7]
    );
    assert_eq!(
        store
            .sync_budget_usage(&[(bk, 5)], &[bk], t0)
            .await
            .unwrap(),
        [12]
    );
    assert!(
        store
            .claim_event("e1", "budget", t0, t0 + 10)
            .await
            .unwrap()
    );
    assert!(
        !store
            .claim_event("e1", "budget", t0, t0 + 10)
            .await
            .unwrap()
    );
    assert!(
        store
            .claim_event("e1", "budget", t0 + 10, t0 + 20)
            .await
            .unwrap()
    );
    let sealed = ring.seal(b"cache-1", b"vektor+antwort").unwrap();
    store
        .cache_vectors_upsert(&[aigw_store::cache::CacheVectorRow {
            id: "cache-1".into(),
            partition_key: "p".into(),
            dims: 3,
            created_at: t0,
            expires_at: t0 + 1000,
            sealed: sealed.clone(),
        }])
        .await
        .unwrap();
    let loaded = store.cache_vectors_load(t0, 10).await.unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].sealed, sealed, "BYTEA/BLOB-Rundreise");
    assert_eq!(
        ring.open(b"cache-1", &loaded[0].sealed).unwrap(),
        b"vektor+antwort"
    );

    // Geltungsbereich von MCP-Freigaben und Plugin-Bindungen bleibt unberührt: Löschen eines
    // Teams mit Keys scheitert am Fremdschlüssel.
    let err = store
        .delete(ACTOR, ResourceKind::Team, "ops")
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Conflict(_)), "{err}");

    store.close().await;
}

#[tokio::test]
async fn sqlite_in_memory() {
    run(Store::connect("sqlite::memory:").await.unwrap()).await;
}

#[tokio::test]
async fn sqlite_file() {
    let dir = std::env::temp_dir().join(format!("aigw-backends-db-{}", new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let url = format!("sqlite://{}/aigw.db?mode=rwc", dir.display());
    run(Store::connect(&url).await.unwrap()).await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn postgres() {
    let Ok(base) = std::env::var("AIGW_TEST_POSTGRES_URL") else {
        eprintln!("AIGW_TEST_POSTGRES_URL nicht gesetzt – Postgres-Test übersprungen");
        return;
    };
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

/// Datenbanken aus der Zeit vor AIGW-73 (gemeinsamer Migrationssatz) werden erkannt.
#[tokio::test]
async fn old_schema_is_rejected() {
    // Migrationstabelle einer alten Datenbank (Version 7 des gemeinsamen Satzes) vortäuschen.
    let dir = std::env::temp_dir().join(format!("aigw-oldschema-{}", new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let url = format!("sqlite://{}/aigw.db?mode=rwc", dir.display());
    let raw = sqlx::SqlitePool::connect(&url).await.unwrap();
    sqlx::query(
        "CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY, description TEXT NOT NULL, \
         installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP, success BOOLEAN NOT NULL, \
         checksum BLOB NOT NULL, execution_time BIGINT NOT NULL)",
    )
    .execute(&raw)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES (7, 'cache_vectors', TRUE, X'00', 0)",
    )
    .execute(&raw)
    .await
    .unwrap();
    raw.close().await;
    let old = Store::connect(&url).await.unwrap();
    let err = old.migrate().await.unwrap_err();
    assert!(matches!(err, StoreError::IncompatibleSchema(_)), "{err}");
    old.close().await;
    let _ = std::fs::remove_dir_all(dir);
}
