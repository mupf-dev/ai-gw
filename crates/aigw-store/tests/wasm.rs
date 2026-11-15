// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! WASM-Module (AIGW-63, Migration 0009) auf beiden Backends: Hochladen mit SHA-256-Prüfung,
//! Ersetzen, „unverändert“ ohne neue Version, Audit ohne Inhalt, Löschschutz bei verweisenden
//! Bindungen, Snapshot (vollständig und inkrementell). Postgres mit `AIGW_TEST_POSTGRES_URL`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use aigw_core::resource::{ObjectMeta, PluginBindingSpec, Scope, ScopeLevel, new_id};
use aigw_store::wasm::{NewWasmModule, PutOutcome, sha256_hex};
use aigw_store::{
    Incremental, Keyring, MasterKey, Store, StoreError, load_incremental, load_snapshot,
};
use sqlx::AssertSqlSafe;

const ACTOR: &str = "admin:test";

fn component(tail: &[u8]) -> Vec<u8> {
    let mut v = b"\0asm\x0d\0\x01\0".to_vec();
    v.extend_from_slice(tail);
    v
}

fn new(name: &str, content: Vec<u8>, sha: Option<String>) -> NewWasmModule {
    NewWasmModule {
        name: name.into(),
        description: None,
        content,
        sha256: sha,
        signature: None,
    }
}

async fn run(store: Store) {
    store.migrate().await.unwrap();
    let ring = Keyring::new(MasterKey::generate().unwrap().0);
    let base = load_snapshot(&store, &ring).await.unwrap();
    assert!(base.wasm_modules.is_empty());

    // Falscher SHA, kein Komponenten-Format, zu groß, ungültiger Name.
    let v1 = component(b"eins");
    for (m, max) in [
        (new("guard", v1.clone(), Some("0".repeat(64))), 1024),
        (new("guard", b"\0asm\x01\0\0\0".to_vec(), None), 1024),
        (new("guard", v1.clone(), None), 4),
        (new("Guard!", v1.clone(), None), 1024),
    ] {
        assert!(
            matches!(
                store.put_wasm_module(ACTOR, m, max).await,
                Err(StoreError::Invalid(_))
            ),
            "ungültig erwartet"
        );
    }
    assert_eq!(
        store.config_version().await.unwrap(),
        0,
        "nichts geschrieben"
    );

    // Anlegen mit passendem SHA (Groß-/Kleinschreibung egal).
    let sha1 = sha256_hex(&v1);
    let out = store
        .put_wasm_module(
            ACTOR,
            new("guard", v1.clone(), Some(sha1.to_uppercase())),
            1024,
        )
        .await
        .unwrap();
    let PutOutcome::Created(info) = out else {
        panic!("Created erwartet: {out:?}")
    };
    assert_eq!(info.sha256, sha1);
    assert_eq!(info.size, v1.len() as u64);
    assert_eq!(info.created_by, ACTOR);
    assert_eq!(store.config_version().await.unwrap(), 1);
    assert_eq!(
        store.wasm_module("guard").await.unwrap().unwrap().content,
        v1
    );
    assert!(matches!(
        store
            .put_wasm_module(ACTOR, new("guard", v1.clone(), None), 1024)
            .await
            .unwrap(),
        PutOutcome::Unchanged(_)
    ));
    assert_eq!(
        store.config_version().await.unwrap(),
        1,
        "unverändert = keine Version"
    );

    // Audit: Metadaten, kein Inhalt.
    let audit = store.audit_log(10).await.unwrap();
    let e = audit.iter().find(|e| e.kind == "wasm-module").unwrap();
    assert_eq!(e.object_id, Some(info.id));
    assert_eq!(e.action, "create");
    let json = serde_json::to_string(&e.after).unwrap();
    assert!(json.contains(&sha1) && !json.contains("eins"), "{json}");

    // Snapshot und inkrementeller Neubau (Ersetzen + Anlegen + Löschen).
    let s1 = load_snapshot(&store, &ring).await.unwrap();
    assert_eq!(s1.wasm_modules["guard"].sha256, sha1);
    let v2 = component(b"zwei");
    assert!(matches!(
        store
            .put_wasm_module(ACTOR, new("guard", v2.clone(), None), 1024)
            .await
            .unwrap(),
        PutOutcome::Replaced(_)
    ));
    store
        .put_wasm_module(ACTOR, new("other", component(b"x"), None), 1024)
        .await
        .unwrap();
    let Incremental::Built { snapshot: s2, .. } =
        load_incremental(&store, &ring, &s1, 100).await.unwrap()
    else {
        panic!("inkrementell erwartet")
    };
    let full = load_snapshot(&store, &ring).await.unwrap();
    assert_eq!(*s2, full);
    assert_eq!(s2.wasm_modules["guard"].content, v2);
    assert_eq!(s2.wasm_modules.len(), 2);

    // Löschschutz: Bindung verweist auf das Modul.
    let mut config = serde_json::Map::new();
    config.insert("module".into(), "guard".into());
    store
        .create(
            ACTOR,
            ObjectMeta::named("b"),
            PluginBindingSpec {
                inherit: None,
                locked: false,
                fail_mode: None,
                timeout_ms: None,
                output_reject_mode: None,
                scope: Scope {
                    level: ScopeLevel::Global,
                    target: None,
                },
                plugin: "wasm".into(),
                order: 0,
                enabled: true,
                config,
            },
        )
        .await
        .unwrap();
    assert_eq!(store.wasm_module_users("guard").await.unwrap(), ["b"]);
    assert!(matches!(
        store.delete_wasm_module(ACTOR, "guard").await,
        Err(StoreError::Conflict(_))
    ));
    store
        .delete_wasm_module(ACTOR, "other")
        .await
        .unwrap()
        .unwrap();
    assert!(
        store
            .delete_wasm_module(ACTOR, "other")
            .await
            .unwrap()
            .is_none()
    );
    let Incremental::Built { snapshot: s3, .. } =
        load_incremental(&store, &ring, &s2, 100).await.unwrap()
    else {
        panic!("inkrementell erwartet")
    };
    assert_eq!(*s3, load_snapshot(&store, &ring).await.unwrap());
    assert!(!s3.wasm_modules.contains_key("other"));
    let names: Vec<String> = store
        .list_wasm_modules()
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.name)
        .collect();
    assert_eq!(names, ["guard"]);

    // Signatur (AIGW-101, Migration 0011): gleicher Inhalt + neue Signatur = ersetzt (neue
    // Version), gleiche Signatur = unverändert; Snapshot (voll und inkrementell) trägt sie.
    let sig = aigw_core::wasm::ModuleSignature::parse("release", &"A".repeat(86)).unwrap();
    let signed = NewWasmModule {
        signature: Some(sig.clone()),
        ..new("guard", v2.clone(), None)
    };
    let PutOutcome::Replaced(info) = store
        .put_wasm_module(ACTOR, signed.clone(), 1024)
        .await
        .unwrap()
    else {
        panic!("Replaced erwartet")
    };
    assert_eq!(info.signature.as_ref(), Some(&sig));
    assert!(matches!(
        store.put_wasm_module(ACTOR, signed, 1024).await.unwrap(),
        PutOutcome::Unchanged(_)
    ));
    let Incremental::Built { snapshot: s4, .. } =
        load_incremental(&store, &ring, &s3, 100).await.unwrap()
    else {
        panic!("inkrementell erwartet")
    };
    assert_eq!(*s4, load_snapshot(&store, &ring).await.unwrap());
    assert_eq!(s4.wasm_modules["guard"].signature.as_ref(), Some(&sig));
    assert_eq!(
        store
            .wasm_module_info("guard")
            .await
            .unwrap()
            .unwrap()
            .signature,
        Some(sig)
    );
    // Signatur entfernen (unsigniert ersetzen) = neue Version.
    assert!(matches!(
        store
            .put_wasm_module(ACTOR, new("guard", v2.clone(), None), 1024)
            .await
            .unwrap(),
        PutOutcome::Replaced(_)
    ));
    assert!(
        store
            .wasm_module("guard")
            .await
            .unwrap()
            .unwrap()
            .signature
            .is_none()
    );
    store.close().await;
}

#[tokio::test]
async fn sqlite() {
    run(Store::connect("sqlite::memory:").await.unwrap()).await;
}

#[tokio::test]
async fn postgres() {
    let Ok(base) = std::env::var("AIGW_TEST_POSTGRES_URL") else {
        eprintln!("AIGW_TEST_POSTGRES_URL nicht gesetzt – Postgres-Test übersprungen");
        return;
    };
    let db = format!("aigw_wasm_{}", new_id().simple());
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
