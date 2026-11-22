// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! AIGW-115: Workload-/Agent-Identitäten an API-Keys (Migration 0012) – Speichern, Prüfung,
//! Eindeutigkeit über alle Keys (Admin-Schreibwege und GitOps, reihenfolgeunabhängig),
//! Rotation, Snapshot-Index – gegen SQLite und (mit `AIGW_TEST_POSTGRES_URL`) Postgres.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use aigw_core::resource::{ApiKeySpec, ObjectMeta, OrganizationSpec, new_id};
use aigw_core::workload::{WorkloadIdentity, WorkloadKey};
use aigw_store::gitops::{ApiKeyMode, ApplyOptions, GitopsError, apply, parse_documents};
use aigw_store::{Keyring, MasterKey, Store, StoreError, load_snapshot};
use sqlx::AssertSqlSafe;

const ACTOR: &str = "test";
const AGENT: &str = "11112222-bbbb-3333-cccc-4444dddd5555";

fn ident(subject: &str) -> WorkloadIdentity {
    WorkloadIdentity {
        issuer: "entra".into(),
        subject: subject.into(),
        description: Some("Agent".into()),
    }
}

fn spec(ids: Vec<WorkloadIdentity>) -> ApiKeySpec {
    ApiKeySpec {
        organization: "o".into(),
        workload_identities: ids,
        ..ApiKeySpec::default()
    }
}

struct NoSecrets;

impl aigw_store::gitops::SecretSource for NoSecrets {
    fn env(&self, _: &str) -> Option<String> {
        None
    }
    fn file(&self, path: &str) -> std::io::Result<String> {
        std::fs::read_to_string(path)
    }
}

async fn run(store: Store) {
    store.migrate().await.unwrap();
    let ring = Keyring::new(MasterKey::generate().unwrap().0);
    store
        .create(ACTOR, ObjectMeta::named("o"), OrganizationSpec::default())
        .await
        .unwrap();

    // Anlegen mit Identität; Lesen liefert sie zurück; Snapshot-Index findet den Key.
    let a = store
        .create_api_key(ACTOR, ObjectMeta::named("a"), spec(vec![ident(AGENT)]))
        .await
        .unwrap();
    let read = store.get_api_key("a").await.unwrap();
    assert_eq!(read.spec.workload_identities, vec![ident(AGENT)]);
    let snap = load_snapshot(&store, &ring).await.unwrap();
    let k = snap
        .workload_key(&WorkloadKey::new("entra", AGENT))
        .unwrap();
    assert_eq!(k.key.id, a.key.id);

    // Doppelt (auch in anderer Schreibweise der GUID) → Konflikt.
    let err = store
        .create_api_key(
            ACTOR,
            ObjectMeta::named("b"),
            spec(vec![ident(&AGENT.to_uppercase())]),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Conflict(_)), "{err:?}");
    store
        .create_api_key(ACTOR, ObjectMeta::named("b"), spec(vec![]))
        .await
        .unwrap();
    let err = store
        .update_api_key(ACTOR, ObjectMeta::named("b"), spec(vec![ident(AGENT)]))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Conflict(_)), "{err:?}");

    // Ungültige Angaben.
    let bad = WorkloadIdentity {
        issuer: "Entra Prod".into(),
        subject: "x".into(),
        description: None,
    };
    let err = store
        .update_api_key(ACTOR, ObjectMeta::named("b"), spec(vec![bad]))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)), "{err:?}");

    // Rotation: Identitäten ziehen zum neuen Key um.
    let (new, old) = store
        .rotate_api_key(
            ACTOR,
            "a",
            ObjectMeta::named("a2"),
            time::OffsetDateTime::now_utc() + time::Duration::hours(1),
            aigw_store::Expect::Any,
        )
        .await
        .unwrap();
    assert!(old.spec.workload_identities.is_empty());
    assert_eq!(new.key.spec.workload_identities, vec![ident(AGENT)]);
    let snap = load_snapshot(&store, &ring).await.unwrap();
    assert_eq!(
        snap.workload_key(&WorkloadKey::new("entra", AGENT))
            .unwrap()
            .key
            .id,
        new.key.id
    );

    // GitOps: doppelte Zuordnung in einer Datei wird schon beim Lesen abgelehnt.
    let dup = format!(
        "apiVersion: aigw/v1alpha1\nkind: ApiKey\nmetadata: {{name: a2}}\nspec:\n  organization: o\n  workloadIdentities: [{{issuer: entra, subject: {AGENT}}}]\n---\n\
         apiVersion: aigw/v1alpha1\nkind: ApiKey\nmetadata: {{name: b}}\nspec:\n  organization: o\n  workloadIdentities: [{{issuer: entra, subject: {AGENT}}}]\n"
    );
    assert!(matches!(
        parse_documents(&dup),
        Err(GitopsError::Invalid(_))
    ));

    // GitOps: Identität in einem Apply von a2 nach b umhängen (Reihenfolge egal).
    let moved = format!(
        "apiVersion: aigw/v1alpha1\nkind: ApiKey\nmetadata: {{name: b}}\nspec:\n  organization: o\n  workloadIdentities: [{{issuer: entra, subject: {AGENT}}}]\n---\n\
         apiVersion: aigw/v1alpha1\nkind: ApiKey\nmetadata: {{name: a2}}\nspec:\n  organization: o\n"
    );
    let docs = parse_documents(&moved).unwrap();
    let opts = ApplyOptions {
        api_keys: ApiKeyMode::Include,
        ..ApplyOptions::default()
    };
    apply(&store, &ring, &docs, &opts, &NoSecrets)
        .await
        .unwrap();
    let snap = load_snapshot(&store, &ring).await.unwrap();
    assert_eq!(
        snap.workload_key(&WorkloadKey::new("entra", AGENT))
            .unwrap()
            .key
            .metadata
            .name,
        "b"
    );

    // GitOps gegen die Datenbank: Konflikt mit einem Key außerhalb der Datei.
    let clash = format!(
        "apiVersion: aigw/v1alpha1\nkind: ApiKey\nmetadata: {{name: a2}}\nspec:\n  organization: o\n  workloadIdentities: [{{issuer: entra, subject: {AGENT}}}]\n"
    );
    let docs = parse_documents(&clash).unwrap();
    assert!(
        apply(&store, &ring, &docs, &opts, &NoSecrets)
            .await
            .is_err()
    );

    store.close().await;
}

#[tokio::test]
async fn sqlite_in_memory() {
    run(Store::connect("sqlite::memory:").await.unwrap()).await;
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
    run(Store::connect(&format!("{prefix}/{db}")).await.unwrap()).await;
    sqlx::query(AssertSqlSafe(format!("DROP DATABASE {db} WITH (FORCE)")))
        .execute(&admin)
        .await
        .unwrap();
}
