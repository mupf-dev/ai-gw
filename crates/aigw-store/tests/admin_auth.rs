// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! AIGW-74 (Admin/Auth) gegen **beide** Backends (SQLite in-memory, SQLite-Datei und – mit
//! `AIGW_TEST_POSTGRES_URL` – Postgres in einer Wegwerf-Datenbank):
//!
//! - F38: Versionsvergleich im `UPDATE`/`DELETE` (`Expect`), streng monotones `updated_at`,
//!   gleichzeitige Änderungen mit demselben erwarteten Stand → genau eine gewinnt.
//! - F37: Key-Zustände `active`/`suspended`/`revoked` (Snapshot, endgültiger Widerruf, Audit).
//! - F193: Keys ohne Team im Geltungsbereich ihrer Organisation (Listen in SQL).
//! - F90/F113: Einladungen (einmalig, Ablauf) und Sessions mit Audit in derselben Transaktion.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr,
    clippy::too_many_lines
)]

use aigw_core::resource::{
    ApiKeySpec, ApiKeyState, ObjectMeta, OrganizationSpec, ResourceKind, Role, RoleBinding,
    TeamSpec, UserSpec, new_id,
};
use aigw_core::snapshot::KeyHash;
use aigw_store::admin_auth::AdminApiKeySpec;
use aigw_store::{
    AuditEvent, AuditScope, Expect, Keyring, ListQuery, MasterKey, ScopeRule, Store, StoreError,
    WriteExtras, hash_api_key, load_snapshot,
};
use sqlx::AssertSqlSafe;
use time::{Duration, OffsetDateTime};

const ACTOR: &str = "test";

fn meta(name: &str) -> ObjectMeta {
    ObjectMeta::named(name)
}

fn key(org: &str, team: Option<&str>) -> ApiKeySpec {
    ApiKeySpec {
        organization: org.into(),
        team: team.map(Into::into),
        ..ApiKeySpec::default()
    }
}

fn h(s: &str) -> KeyHash {
    hash_api_key(s)
}

fn q(scope: AuditScope) -> ListQuery {
    ListQuery {
        limit: 50,
        after: None,
        search: None,
        labels: Vec::new(),
        filters: Vec::new(),
        scope,
    }
}

fn precondition_failed<T: std::fmt::Debug>(r: Result<T, StoreError>) -> bool {
    matches!(r, Err(StoreError::PreconditionFailed { .. }))
}

async fn run(store: Store) {
    store.migrate().await.unwrap();
    let ring = Keyring::new(MasterKey::generate().unwrap().0);
    for o in ["acme", "other"] {
        store
            .create(ACTOR, meta(o), OrganizationSpec::default())
            .await
            .unwrap();
    }
    store
        .create(
            ACTOR,
            meta("platform"),
            TeamSpec {
                organization: "acme".into(),
                ..TeamSpec::default()
            },
        )
        .await
        .unwrap();
    for (n, org, team) in [
        ("k-team", "acme", Some("platform")),
        ("k-org", "acme", None),
        ("k-other", "other", None),
    ] {
        store
            .create_api_key(ACTOR, meta(n), key(org, team))
            .await
            .unwrap();
    }

    // --- F193: Keys ohne Team im Bereich ihrer Organisation ----------------------------------
    let names = |p: aigw_store::Page<aigw_core::resource::ApiKey>| -> Vec<String> {
        p.items.into_iter().map(|k| k.metadata.name).collect()
    };
    let org_rule = AuditScope::Within(vec![ScopeRule {
        organization: Some("acme".into()),
        team: None,
    }]);
    assert_eq!(
        names(store.list_api_keys_page(&q(org_rule)).await.unwrap()),
        ["k-org", "k-team"]
    );
    let team_rule = AuditScope::Within(vec![ScopeRule {
        organization: Some("acme".into()),
        team: Some("platform".into()),
    }]);
    assert_eq!(
        names(store.list_api_keys_page(&q(team_rule)).await.unwrap()),
        ["k-team"]
    );

    // --- F38: Versionsvergleich im UPDATE -----------------------------------------------------
    let t0 = store.get::<TeamSpec>("platform").await.unwrap();
    let v0 = t0.metadata.updated_at.unwrap();
    let mut spec = t0.spec.clone();
    spec.display_name = Some("Plattform".into());
    let t1 = store
        .update_with(
            ACTOR,
            t0.metadata.clone(),
            spec.clone(),
            &WriteExtras::default().expecting(Expect::Version(v0)),
        )
        .await
        .unwrap();
    let v1 = t1.metadata.updated_at.unwrap();
    assert!(v1 > v0, "updated_at streng monoton");
    // veralteter Stand → abgelehnt, nichts geändert
    let version = store.config_version().await.unwrap();
    spec.display_name = Some("veraltet".into());
    assert!(precondition_failed(
        store
            .update_with(
                ACTOR,
                t0.metadata.clone(),
                spec.clone(),
                &WriteExtras::default().expecting(Expect::Version(v0)),
            )
            .await
    ));
    assert_eq!(store.config_version().await.unwrap(), version);
    assert_eq!(
        store
            .get::<TeamSpec>("platform")
            .await
            .unwrap()
            .spec
            .display_name
            .as_deref(),
        Some("Plattform")
    );
    // schnelle Folge: jeder Schritt erwartet den vorigen Stand (gleiche Millisekunde möglich)
    let mut prev = v1;
    for i in 0..5 {
        spec.display_name = Some(format!("p{i}"));
        let t = store
            .update_with(
                ACTOR,
                t0.metadata.clone(),
                spec.clone(),
                &WriteExtras::default().expecting(Expect::Version(prev)),
            )
            .await
            .unwrap();
        let v = t.metadata.updated_at.unwrap();
        assert!(v > prev, "Schritt {i}: {v} > {prev}");
        prev = v;
    }
    // gleichzeitig mit demselben erwarteten Stand: genau eine Änderung gewinnt
    let mk = |name: &str| {
        let store = store.clone();
        let m = t0.metadata.clone();
        let mut s = spec.clone();
        s.display_name = Some(name.into());
        async move {
            store
                .update_with(
                    ACTOR,
                    m,
                    s,
                    &WriteExtras::default().expecting(Expect::Version(prev)),
                )
                .await
        }
    };
    let (a, b) = tokio::join!(mk("a"), mk("b"));
    assert_eq!(
        usize::from(a.is_ok()) + usize::from(b.is_ok()),
        1,
        "{a:?} / {b:?}"
    );
    assert!(precondition_failed(if a.is_ok() { b } else { a }));
    // Löschen mit veraltetem Stand → abgelehnt; mit aktuellem → gelöscht
    let scratch = store
        .create(ACTOR, meta("scratch"), OrganizationSpec::default())
        .await
        .unwrap();
    let sv = scratch.metadata.updated_at.unwrap();
    store
        .update(ACTOR, scratch.metadata.clone(), OrganizationSpec::default())
        .await
        .unwrap();
    assert!(precondition_failed(
        store
            .delete_expect(
                ACTOR,
                ResourceKind::Organization,
                "scratch",
                Expect::Version(sv)
            )
            .await
    ));
    let cur = store
        .get::<OrganizationSpec>("scratch")
        .await
        .unwrap()
        .metadata
        .updated_at
        .unwrap();
    store
        .delete_expect(
            ACTOR,
            ResourceKind::Organization,
            "scratch",
            Expect::Version(cur),
        )
        .await
        .unwrap();
    // Admin-API-Keys ebenso
    let ak = store
        .create_admin_api_key(
            ACTOR,
            "ci",
            AdminApiKeySpec {
                description: None,
                roles: vec![RoleBinding {
                    role: Role::Gitops,
                    organization: None,
                    team: None,
                }],
                expires_at: None,
            },
        )
        .await
        .unwrap();
    let akv = ak.key.updated_at;
    let upd = store
        .update_admin_api_key(ACTOR, "ci", ak.key.spec.clone(), Expect::Version(akv))
        .await
        .unwrap();
    assert!(upd.updated_at > akv);
    assert!(precondition_failed(
        store
            .update_admin_api_key(ACTOR, "ci", ak.key.spec.clone(), Expect::Version(akv))
            .await
    ));
    assert!(precondition_failed(
        store
            .delete_admin_api_key(ACTOR, "ci", Expect::Version(akv))
            .await
    ));
    store
        .delete_admin_api_key(ACTOR, "ci", Expect::Version(upd.updated_at))
        .await
        .unwrap();

    // --- F37: Zustände ------------------------------------------------------------------------
    let in_snapshot = |snap: &aigw_core::snapshot::Snapshot, n: &str| {
        snap.keys.values().any(|k| k.key.metadata.name == n)
    };
    let k = store
        .set_api_key_state(ACTOR, "k-team", ApiKeyState::Suspended, Expect::Any)
        .await
        .unwrap();
    assert_eq!(k.status.state, ApiKeyState::Suspended);
    assert!(!in_snapshot(
        &load_snapshot(&store, &ring).await.unwrap(),
        "k-team"
    ));
    // idempotent: keine neue Version
    let v = store.config_version().await.unwrap();
    store
        .set_api_key_state(ACTOR, "k-team", ApiKeyState::Suspended, Expect::Any)
        .await
        .unwrap();
    assert_eq!(store.config_version().await.unwrap(), v);
    // Fortsetzen mit veraltetem Stand → abgelehnt
    let stale = store
        .get_api_key("k-org")
        .await
        .unwrap()
        .metadata
        .updated_at
        .unwrap();
    store
        .set_api_key_state(
            ACTOR,
            "k-org",
            ApiKeyState::Suspended,
            Expect::Version(stale),
        )
        .await
        .unwrap();
    assert!(precondition_failed(
        store
            .set_api_key_state(ACTOR, "k-org", ApiKeyState::Active, Expect::Version(stale))
            .await
    ));
    store
        .set_api_key_state(ACTOR, "k-team", ApiKeyState::Active, Expect::Any)
        .await
        .unwrap();
    assert!(in_snapshot(
        &load_snapshot(&store, &ring).await.unwrap(),
        "k-team"
    ));
    store
        .set_api_key_state(ACTOR, "k-team", ApiKeyState::Revoked, Expect::Any)
        .await
        .unwrap();
    for s in [ApiKeyState::Active, ApiKeyState::Suspended] {
        let err = store
            .set_api_key_state(ACTOR, "k-team", s, Expect::Any)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Conflict(_)), "{err}");
    }
    let err = store
        .rotate_api_key(
            ACTOR,
            "k-team",
            meta("k-team-2"),
            OffsetDateTime::now_utc(),
            Expect::Any,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Conflict(_)), "{err}");
    assert!(!in_snapshot(
        &load_snapshot(&store, &ring).await.unwrap(),
        "k-team"
    ));
    let actions: Vec<String> = store
        .audit_log(200)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.object_name.as_deref() == Some("k-team"))
        .map(|e| e.action)
        .collect();
    for a in ["suspend", "resume", "revoke"] {
        assert!(actions.iter().any(|x| x == a), "{a}: {actions:?}");
    }

    // --- F90: Einladungen; F113: Sessions mit Audit -------------------------------------------
    let user = store
        .create(ACTOR, meta("anna"), UserSpec::default())
        .await
        .unwrap();
    let now = OffsetDateTime::now_utc();
    store
        .create_invitation(ACTOR, user.id, "anna", &h("inv"), now + Duration::hours(1))
        .await
        .unwrap();
    assert!(store.invitation_valid(&h("inv"), now).await.unwrap());
    assert_eq!(
        store.take_invitation(&h("inv"), now).await.unwrap(),
        Some(user.id)
    );
    assert_eq!(store.take_invitation(&h("inv"), now).await.unwrap(), None);
    let ev = |action: &str| AuditEvent {
        actor: "user:anna".into(),
        action: action.into(),
        kind: "admin-session".into(),
        object_id: Some(user.id),
        object_name: Some("anna".into()),
        details: None,
    };
    store
        .create_admin_session_audited(
            &h("sid"),
            user.id,
            &["g".into()],
            now + Duration::hours(1),
            &ev("login"),
        )
        .await
        .unwrap();
    assert!(store.get_admin_session(&h("sid")).await.unwrap().is_some());
    assert!(
        store
            .delete_admin_session_audited(&h("sid"), &ev("logout"))
            .await
            .unwrap()
    );
    assert!(store.get_admin_session(&h("sid")).await.unwrap().is_none());
    let session_actions: Vec<String> = store
        .audit_log(200)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "admin-session")
        .map(|e| e.action)
        .collect();
    assert_eq!(session_actions, ["logout", "login"]);
}

#[tokio::test]
async fn sqlite_in_memory() {
    run(Store::connect("sqlite::memory:").await.unwrap()).await;
}

#[tokio::test]
async fn sqlite_file() {
    let dir = std::env::temp_dir().join(format!("aigw-admin-auth-db-{}", new_id()));
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
