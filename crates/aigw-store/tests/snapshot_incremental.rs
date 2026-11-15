// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Inkrementeller Snapshot-Neubau, Konsistenzprüfung und Platten-Cache (F14/F16, AIGW-75).
//!
//! * Property-Test: zufällige Folgen von Admin-Änderungen über alle Ressourcenarten (inkl.
//!   Team-Umzug mit `ON UPDATE CASCADE`, Löschungen mit mitgelöschten Bindungen/Freigaben,
//!   Key-Rotation, Zugangsdaten-Wechsel, Löschen + gleichnamige Neuanlage von Key,
//!   Modellgruppe, Team und Organisation); nach jedem Schritt-Block muss der inkrementell
//!   fortgeschriebene Snapshot exakt dem vollständigen Neubau entsprechen. SQLite immer,
//!   Postgres mit `AIGW_TEST_POSTGRES_URL`.
//! * Löschen + gleichnamige Neuanlage gezielt (Schritt für Schritt bzw. alles in einem Neubau,
//!   laufender Reloader mit Debounce).
//! * Rückfälle (Lücke, zu viele Änderungen), Reloader-Metriken, Konsistenzprüfung.
//! * Platten-Cache: Rundreise, kaputter/fremder Cache wird verworfen, Kaltstart ohne Datenbank.
//!
//! Messung (`--ignored`): 1 Änderung bei 100 000 Keys, inkrementell vs. vollständig.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use aigw_core::resource::{
    ApiKeySpec, ApiKeyState, DeploymentSpec, Id, McpGrantSpec, McpServerSpec, ModelGroupSpec,
    ObjectMeta, OrganizationSpec, PluginBindingSpec, PriceSpec, ProviderCredentialSpec,
    ResourceKind, Scope, ScopeLevel, SecretRef, TeamSpec, new_id,
};
use aigw_core::snapshot::{KeyHash, Snapshot, SnapshotHandle, SnapshotSource};
use aigw_core::types::ProviderKind;
use aigw_store::{
    Fallback, Incremental, Keyring, MasterKey, PlainResource, ReloadOptions, Reloader,
    SnapshotCache, Store, hash_api_key, load_incremental, load_snapshot,
};
use sqlx::AssertSqlSafe;
use time::OffsetDateTime;

const ACTOR: &str = "test";

fn ring() -> Arc<Keyring> {
    Arc::new(Keyring::new(MasterKey::generate().unwrap().0))
}

/// Kleiner deterministischer Zufallsgenerator (xorshift64*), reproduzierbar über den Seed.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn name(&mut self, prefix: &str, n: u64) -> String {
        format!("{prefix}{}", self.below(n))
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("aigw-{tag}-{}", new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Eine zufällige Admin-Änderung (Fehler wie Namenskonflikte oder verweigerte Löschungen sind
/// erwartet und werden ignoriert – sie erhöhen `config_version` nicht).
async fn random_change(store: &Store, keyring: &Keyring, rng: &mut Rng) {
    let org = rng.name("o", 3);
    let team = rng.name("t", 5);
    let key = rng.name("k", 16);
    let cred = rng.name("c", 3);
    let group = rng.name("g", 4);
    let models =
        |rng: &mut Rng| -> Vec<String> { (0..rng.below(3)).map(|_| rng.name("g", 4)).collect() };
    match rng.below(32) {
        // WASM-Module (AIGW-63): anlegen/ersetzen bzw. löschen (Journal-Art `wasm-module`).
        30 => {
            let mut content = b"\0asm\x0d\0\x01\0".to_vec();
            content.extend(rng.next().to_le_bytes());
            drop(
                store
                    .put_wasm_module(
                        ACTOR,
                        aigw_store::wasm::NewWasmModule {
                            name: rng.name("w", 3),
                            description: None,
                            content,
                            sha256: None,
                            signature: None,
                        },
                        1024,
                    )
                    .await,
            );
        }
        31 => drop(store.delete_wasm_module(ACTOR, &rng.name("w", 3)).await),
        0 => drop(
            store
                .create(ACTOR, ObjectMeta::named(&org), OrganizationSpec::default())
                .await,
        ),
        1 => drop(
            store
                .update(
                    ACTOR,
                    ObjectMeta::named(&org),
                    OrganizationSpec {
                        display_name: Some(format!("Org {}", rng.below(100))),
                        ..Default::default()
                    },
                )
                .await,
        ),
        2 => drop(store.delete(ACTOR, ResourceKind::Organization, &org).await),
        3 => drop(
            store
                .create(
                    ACTOR,
                    ObjectMeta::named(&team),
                    TeamSpec {
                        organization: org,
                        ..Default::default()
                    },
                )
                .await,
        ),
        // Team ändern bzw. in eine andere Organisation umziehen (Keys ziehen per Kaskade mit).
        4 | 5 => {
            let allowed_models = models(rng);
            drop(
                store
                    .update(
                        ACTOR,
                        ObjectMeta::named(&team),
                        TeamSpec {
                            organization: org,
                            allowed_models,
                            display_name: rng.chance(50).then(|| "Team".to_owned()),
                            ..Default::default()
                        },
                    )
                    .await,
            );
        }
        6 => drop(store.delete(ACTOR, ResourceKind::Team, &team).await),
        7 | 8 => {
            // Key mit Team: Organisation des Teams übernehmen (sonst lehnt der Store ab).
            let with_team = rng.chance(60);
            let org = if with_team {
                match store.get::<TeamSpec>(&team).await {
                    Ok(t) => t.spec.organization,
                    Err(_) => org,
                }
            } else {
                org
            };
            drop(
                store
                    .create_api_key(
                        ACTOR,
                        ObjectMeta::named(&key),
                        ApiKeySpec {
                            organization: org,
                            team: with_team.then_some(team),
                            ..Default::default()
                        },
                    )
                    .await,
            );
        }
        9 => {
            if let Ok(k) = store.get_api_key(&key).await {
                let mut spec = k.spec;
                spec.allowed_models = models(rng);
                drop(store.update_api_key(ACTOR, k.metadata, spec).await);
            }
        }
        10 => {
            // F37: vorübergehend sperren/fortsetzen, selten endgültig widerrufen.
            let state = if rng.chance(10) {
                ApiKeyState::Revoked
            } else if rng.chance(50) {
                ApiKeyState::Suspended
            } else {
                ApiKeyState::Active
            };
            drop(
                store
                    .set_api_key_state(ACTOR, &key, state, aigw_store::Expect::Any)
                    .await,
            );
        }
        11 => drop(
            store
                .rotate_api_key(
                    ACTOR,
                    &key,
                    ObjectMeta::named(rng.name("k", 16)),
                    OffsetDateTime::now_utc() + time::Duration::hours(1),
                    aigw_store::Expect::Any,
                )
                .await,
        ),
        12 => drop(store.delete(ACTOR, ResourceKind::ApiKey, &key).await),
        13 => {
            let spec = ProviderCredentialSpec {
                provider: ProviderKind::OpenAi,
                entra: None,
                data: BTreeMap::from([(
                    "apiKey".to_owned(),
                    SecretRef::value(format!("sk-{}", rng.below(1000))),
                )]),
            };
            if store.get_credential(&cred).await.is_ok() {
                drop(
                    store
                        .update_credential(
                            ACTOR,
                            keyring,
                            ObjectMeta::named(&cred),
                            spec,
                            aigw_store::Expect::Any,
                        )
                        .await,
                );
            } else {
                drop(
                    store
                        .create_credential(ACTOR, keyring, ObjectMeta::named(&cred), spec)
                        .await,
                );
            }
        }
        14 => drop(
            store
                .delete(ACTOR, ResourceKind::ProviderCredential, &cred)
                .await,
        ),
        // Modellgruppe anlegen/ändern (Aliasse wandern), löschen.
        15 => {
            let aliases: Vec<String> = (0..rng.below(3)).map(|_| rng.name("a", 5)).collect();
            let meta = ObjectMeta::named(&group);
            let spec = ModelGroupSpec {
                aliases,
                ..Default::default()
            };
            if store
                .create(ACTOR, meta.clone(), spec.clone())
                .await
                .is_err()
            {
                drop(store.update(ACTOR, meta, spec).await);
            }
        }
        16 => drop(store.delete(ACTOR, ResourceKind::ModelGroup, &group).await),
        // Deployments: anlegen/ändern (Gruppe, Priorität, an/aus, Zugangsdaten), löschen.
        17 | 18 => {
            let meta = ObjectMeta::named(rng.name("d", 8));
            let spec = DeploymentSpec {
                model_group: group,
                provider: ProviderKind::OpenAi,
                endpoint: None,
                provider_model: rng.name("m", 3),
                credential: rng.chance(60).then_some(cred),
                weight: 1,
                priority: u32::try_from(rng.below(3)).unwrap(),
                capacity: Default::default(),
                enabled: rng.chance(80),
                settings: Default::default(),
            };
            if store
                .create(ACTOR, meta.clone(), spec.clone())
                .await
                .is_err()
            {
                drop(store.update(ACTOR, meta, spec).await);
            }
        }
        19 => drop(
            store
                .delete(ACTOR, ResourceKind::Deployment, &rng.name("d", 8))
                .await,
        ),
        20 => {
            let meta = ObjectMeta::named(rng.name("p", 6));
            let spec = PriceSpec {
                per_audio_minute: None,
                per_million_characters: None,
                provider: rng.chance(50).then_some(ProviderKind::OpenAi),
                model: rng.name("m", 3),
                valid_from: OffsetDateTime::from_unix_timestamp(
                    1_700_000_000 + i64::try_from(rng.below(4)).unwrap() * 86_400,
                )
                .unwrap(),
                per_million_tokens: Default::default(),
                per_request: None,
                per_image: None,
            };
            if rng.chance(25) {
                drop(store.delete(ACTOR, ResourceKind::Price, &meta.name).await);
            } else if store
                .create(ACTOR, meta.clone(), spec.clone())
                .await
                .is_err()
            {
                drop(store.update(ACTOR, meta, spec).await);
            }
        }
        // Plugin-Bindungen auf allen Ebenen.
        21 | 22 => {
            let scope = match rng.below(5) {
                0 => Scope {
                    level: ScopeLevel::Global,
                    target: None,
                },
                1 => Scope {
                    level: ScopeLevel::Organization,
                    target: Some(org),
                },
                2 => Scope {
                    level: ScopeLevel::Team,
                    target: Some(team),
                },
                3 => Scope {
                    level: ScopeLevel::ApiKey,
                    target: Some(key),
                },
                _ => Scope {
                    level: ScopeLevel::ModelGroup,
                    target: Some(group),
                },
            };
            let meta = ObjectMeta::named(rng.name("b", 6));
            let spec = PluginBindingSpec {
                inherit: None,
                locked: false,
                fail_mode: None,
                timeout_ms: None,
                output_reject_mode: None,
                scope,
                plugin: "header".into(),
                order: i32::try_from(rng.below(3)).unwrap(),
                enabled: rng.chance(80),
                config: Default::default(),
            };
            if rng.chance(20) {
                drop(
                    store
                        .delete(ACTOR, ResourceKind::PluginBinding, &meta.name)
                        .await,
                );
            } else if store
                .create(ACTOR, meta.clone(), spec.clone())
                .await
                .is_err()
            {
                drop(store.update(ACTOR, meta, spec).await);
            }
        }
        // MCP-Server und -Freigaben.
        23 => {
            let meta = ObjectMeta::named(rng.name("s", 3));
            if rng.chance(20) {
                drop(
                    store
                        .delete(ACTOR, ResourceKind::McpServer, &meta.name)
                        .await,
                );
            } else {
                let spec = McpServerSpec {
                    secret: None,
                    url: format!("https://mcp{}.example/mcp", rng.below(3)),
                    transport: Default::default(),
                    credential: rng.chance(50).then_some(cred),
                    enabled: rng.chance(80),
                    settings: Default::default(),
                };
                if store
                    .create(ACTOR, meta.clone(), spec.clone())
                    .await
                    .is_err()
                {
                    drop(store.update(ACTOR, meta, spec).await);
                }
            }
        }
        25 => {
            let meta = ObjectMeta::named(rng.name("r", 5));
            if rng.chance(20) {
                drop(
                    store
                        .delete(ACTOR, ResourceKind::McpGrant, &meta.name)
                        .await,
                );
            } else {
                let subject = if rng.chance(50) {
                    Scope {
                        level: ScopeLevel::Team,
                        target: Some(team),
                    }
                } else {
                    Scope {
                        level: ScopeLevel::ApiKey,
                        target: Some(key),
                    }
                };
                let spec = McpGrantSpec {
                    server: rng.name("s", 3),
                    subject,
                    tools: (0..rng.below(2)).map(|_| rng.name("tool", 3)).collect(),
                };
                if store
                    .create(ACTOR, meta.clone(), spec.clone())
                    .await
                    .is_err()
                {
                    drop(store.update(ACTOR, meta, spec).await);
                }
            }
        }
        // Löschen + sofortige Neuanlage unter demselben Namen (Verdacht aus dem UI-e2e).
        26 | 27 => recreate_key(store, rng, &key, &org, &team).await,
        28 => recreate_group(store, rng, &group).await,
        _ => recreate_team_or_org(store, rng, &team, &org).await,
    }
}

/// Key löschen und gleichnamig neu anlegen – im selben oder einem anderen Scope, mit oder
/// ohne Team.
async fn recreate_key(store: &Store, rng: &mut Rng, key: &str, org: &str, team: &str) {
    if store
        .delete(ACTOR, ResourceKind::ApiKey, key)
        .await
        .is_err()
    {
        return;
    }
    let with_team = rng.chance(50);
    let org = match store.get::<TeamSpec>(team).await {
        Ok(t) if with_team => t.spec.organization,
        _ => org.to_owned(),
    };
    drop(
        store
            .create_api_key(
                ACTOR,
                ObjectMeta::named(key),
                ApiKeySpec {
                    organization: org,
                    team: with_team.then(|| team.to_owned()),
                    ..Default::default()
                },
            )
            .await,
    );
}

/// Modellgruppe samt Deployments löschen und gleichnamig neu anlegen (Deployments mit denselben
/// Namen, teils geändert).
async fn recreate_group(store: &Store, rng: &mut Rng, group: &str) {
    let Ok(all) = store.list::<DeploymentSpec>().await else {
        return;
    };
    let deps: Vec<_> = all
        .into_iter()
        .filter(|d| d.spec.model_group == group)
        .collect();
    for d in &deps {
        drop(
            store
                .delete(ACTOR, ResourceKind::Deployment, &d.metadata.name)
                .await,
        );
    }
    if store
        .delete(ACTOR, ResourceKind::ModelGroup, group)
        .await
        .is_ok()
    {
        let aliases: Vec<String> = (0..rng.below(3)).map(|_| rng.name("a", 5)).collect();
        drop(
            store
                .create(
                    ACTOR,
                    ObjectMeta::named(group),
                    ModelGroupSpec {
                        aliases,
                        ..Default::default()
                    },
                )
                .await,
        );
    }
    for d in deps {
        let mut spec = d.spec;
        spec.priority = u32::try_from(rng.below(3)).unwrap();
        spec.enabled = rng.chance(80);
        drop(
            store
                .create(ACTOR, ObjectMeta::named(&d.metadata.name), spec)
                .await,
        );
    }
}

/// Team (samt seiner Keys) bzw. Organisation löschen und gleichnamig neu anlegen; das Team
/// ggf. in einer anderen Organisation, die Keys unter ihren alten Namen.
async fn recreate_team_or_org(store: &Store, rng: &mut Rng, team: &str, org: &str) {
    if rng.chance(30) {
        // Organisation (geht nur ohne Teams/Keys).
        if store
            .delete(ACTOR, ResourceKind::Organization, org)
            .await
            .is_ok()
        {
            drop(
                store
                    .create(ACTOR, ObjectMeta::named(org), OrganizationSpec::default())
                    .await,
            );
        }
        return;
    }
    let Ok(keys) = store.list_api_keys().await else {
        return;
    };
    let keys: Vec<_> = keys
        .into_iter()
        .filter(|k| k.spec.team.as_deref() == Some(team))
        .collect();
    for k in &keys {
        drop(
            store
                .delete(ACTOR, ResourceKind::ApiKey, &k.metadata.name)
                .await,
        );
    }
    if store.delete(ACTOR, ResourceKind::Team, team).await.is_err() {
        return;
    }
    let new_org = if rng.chance(50) {
        org.to_owned()
    } else {
        rng.name("o", 3)
    };
    if store
        .create(
            ACTOR,
            ObjectMeta::named(team),
            TeamSpec {
                organization: new_org.clone(),
                ..Default::default()
            },
        )
        .await
        .is_err()
    {
        return;
    }
    for k in keys {
        drop(
            store
                .create_api_key(
                    ACTOR,
                    ObjectMeta::named(&k.metadata.name),
                    ApiKeySpec {
                        organization: new_org.clone(),
                        team: Some(team.to_owned()),
                        ..Default::default()
                    },
                )
                .await,
        );
    }
}

/// Kern des Property-Tests: zufällige Änderungsfolgen, Snapshot inkrementell fortschreiben und
/// nach jedem Block mit dem vollständigen Neubau vergleichen.
async fn incremental_equals_full(store: &Store, seeds: std::ops::Range<u64>, steps: usize) {
    let keyring = ring();
    let mut total_changes = 0usize;
    let mut built = 0usize;
    for seed in seeds {
        let mut rng = Rng::new(seed);
        let mut snap = load_snapshot(store, &keyring).await.unwrap();
        let mut done = 0;
        while done < steps {
            // Blöcke von 1–6 Änderungen (mehrere Versionen auf einmal einarbeiten).
            let block = usize::try_from(1 + rng.below(6)).unwrap();
            for _ in 0..block {
                random_change(store, &keyring, &mut rng).await;
            }
            done += block;
            let next = match load_incremental(store, &keyring, &snap, 10_000)
                .await
                .unwrap()
            {
                Incremental::UpToDate => snap.clone(),
                Incremental::Built { snapshot, changes } => {
                    total_changes += changes;
                    built += 1;
                    *snapshot
                }
                Incremental::NeedFull(f) => panic!("unerwarteter Rückfall {f:?} (Seed {seed})"),
            };
            let full = load_snapshot(store, &keyring).await.unwrap();
            assert_eq!(next.version, full.version, "Seed {seed}, Schritt {done}");
            if next != full {
                panic!(
                    "Seed {seed}, Schritt {done}: inkrementell ≠ vollständig\n\
                     inkrementell: {:#?}\nvollständig: {:#?}",
                    next.to_parts(),
                    full.to_parts()
                );
            }
            // Rohdaten-Rundreise erhält alle Indizes.
            assert_eq!(Snapshot::from_parts(full.to_parts()), full);
            snap = next;
        }
    }
    eprintln!("Property-Test: {built} inkrementelle Neubauten, {total_changes} Journal-Einträge");
    assert!(built > 0);
    // Am Ende soll der Datenbestand nicht trivial sein.
    let full = load_snapshot(store, &keyring).await.unwrap();
    assert!(!full.organizations.is_empty());
}

#[tokio::test]
async fn property_incremental_equals_full_sqlite() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    store.migrate().await.unwrap();
    incremental_equals_full(&store, 0..6, 250).await;
    store.close().await;
}

async fn pg_store(tag: &str) -> Option<(Store, sqlx::PgPool, String)> {
    let Ok(base) = std::env::var("AIGW_TEST_POSTGRES_URL") else {
        eprintln!("AIGW_TEST_POSTGRES_URL nicht gesetzt – Postgres-Test übersprungen");
        return None;
    };
    let db = format!("aigw_{tag}_{}", new_id().simple());
    let admin = sqlx::PgPool::connect(&base).await.unwrap();
    sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {db}")))
        .execute(&admin)
        .await
        .unwrap();
    let (prefix, _) = base.rsplit_once('/').expect("URL mit Datenbankpfad");
    let store = Store::connect(&format!("{prefix}/{db}")).await.unwrap();
    store.migrate().await.unwrap();
    Some((store, admin, db))
}

async fn pg_drop(store: Store, admin: sqlx::PgPool, db: String) {
    store.close().await;
    sqlx::query(AssertSqlSafe(format!("DROP DATABASE {db} WITH (FORCE)")))
        .execute(&admin)
        .await
        .unwrap();
}

#[tokio::test]
async fn property_incremental_equals_full_postgres() {
    let Some((store, admin, db)) = pg_store("snapinc").await else {
        return;
    };
    incremental_equals_full(&store, 100..103, 200).await;
    pg_drop(store, admin, db).await;
}

async fn org(store: &Store, name: &str) {
    store
        .create(ACTOR, ObjectMeta::named(name), OrganizationSpec::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn fallbacks_gap_too_many_and_up_to_date() {
    let dir = temp_dir("snapfb");
    let url = format!("sqlite://{}/aigw.db?mode=rwc", dir.display());
    let store = Store::connect(&url).await.unwrap();
    store.migrate().await.unwrap();
    let keyring = ring();
    org(&store, "acme").await;
    let base = load_snapshot(&store, &keyring).await.unwrap();
    assert!(matches!(
        load_incremental(&store, &keyring, &base, 10).await.unwrap(),
        Incremental::UpToDate
    ));

    for n in ["b1", "b2", "b3"] {
        org(&store, n).await;
    }
    assert!(matches!(
        load_incremental(&store, &keyring, &base, 2).await.unwrap(),
        Incremental::NeedFull(Fallback::TooMany(3))
    ));

    // Lücke: Journal-Einträge einer Version fehlen (z. B. aufgeräumtes Audit-Log).
    let raw = sqlx::SqlitePool::connect(&url).await.unwrap();
    let v = store.config_version().await.unwrap();
    sqlx::query("DELETE FROM audit_log WHERE config_version = ?")
        .bind(v - 1)
        .execute(&raw)
        .await
        .unwrap();
    raw.close().await;
    assert!(matches!(
        load_incremental(&store, &keyring, &base, 100)
            .await
            .unwrap(),
        Incremental::NeedFull(Fallback::Gap)
    ));

    // Datenbank „zurückgespielt“: Version kleiner als die des Snapshots.
    let ahead = Snapshot {
        version: v + 10,
        ..Snapshot::default()
    };
    assert!(matches!(
        load_incremental(&store, &keyring, &ahead, 100)
            .await
            .unwrap(),
        Incremental::NeedFull(Fallback::VersionBackwards)
    ));

    // Der Reloader fällt in diesen Fällen auf den vollständigen Neubau zurück.
    let reloader = Reloader::new(
        store.clone(),
        Arc::clone(&keyring),
        SnapshotHandle::new(base),
        ReloadOptions::default(),
    );
    assert_eq!(reloader.reload_now().await.unwrap(), Some(v));
    let m = reloader.metrics();
    assert_eq!(m.incremental_fallbacks.load(Ordering::Relaxed), 1);
    assert_eq!(reloader.handle().load().organizations.len(), 4);
    store.close().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn reloader_builds_incrementally_and_verify_repairs() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    store.migrate().await.unwrap();
    let keyring = ring();
    org(&store, "acme").await;
    let reloader = Reloader::bootstrap(
        store.clone(),
        Arc::clone(&keyring),
        ReloadOptions::default(),
    )
    .await
    .unwrap();
    org(&store, "beta").await;
    let v = reloader.reload_now().await.unwrap().unwrap();
    let m = reloader.metrics();
    assert_eq!(m.reloads_incremental.load(Ordering::Relaxed), 1);
    assert_eq!(reloader.handle().version(), v);
    assert!(reloader.verify_now().await.unwrap(), "konsistent");
    assert_eq!(m.verify_mismatches.load(Ordering::Relaxed), 0);

    // Abweichung vortäuschen: gleicher Stand, aber eine Organisation fehlt.
    let mut broken = (*reloader.handle().load()).clone();
    let id = *broken.organizations.keys().next().unwrap();
    broken.organizations.remove(&id);
    let handle = SnapshotHandle::new(broken);
    let r2 = Reloader::new(
        store.clone(),
        Arc::clone(&keyring),
        handle.clone(),
        ReloadOptions::default(),
    );
    assert!(!r2.verify_now().await.unwrap(), "Abweichung erkannt");
    assert_eq!(r2.metrics().verify_mismatches.load(Ordering::Relaxed), 1);
    assert_eq!(handle.load().organizations.len(), 2, "durch Neubau ersetzt");
    assert_eq!(
        *handle.load(),
        load_snapshot(&store, &keyring).await.unwrap()
    );
    store.close().await;
}

/// Daten für Cache-Tests: Organisation, Team, Key, Zugangsdaten, Gruppe mit Deployment.
async fn sample(store: &Store, keyring: &Keyring) {
    org(store, "acme").await;
    store
        .put_wasm_module(
            ACTOR,
            aigw_store::wasm::NewWasmModule {
                name: "guard".into(),
                description: Some("Test".into()),
                content: b"\0asm\x0d\0\x01\0inhalt".to_vec(),
                sha256: None,
                signature: Some(
                    aigw_core::wasm::ModuleSignature::parse("release", &"A".repeat(86)).unwrap(),
                ),
            },
            1024,
        )
        .await
        .unwrap();
    store
        .create(
            ACTOR,
            ObjectMeta::named("platform"),
            TeamSpec {
                organization: "acme".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    store
        .create_api_key(
            ACTOR,
            ObjectMeta::named("ci"),
            ApiKeySpec {
                organization: "acme".into(),
                team: Some("platform".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    store
        .create_credential(
            ACTOR,
            keyring,
            ObjectMeta::named("openai"),
            ProviderCredentialSpec {
                provider: ProviderKind::OpenAi,
                entra: None,
                data: BTreeMap::from([("apiKey".to_owned(), SecretRef::value("sk-geheim"))]),
            },
        )
        .await
        .unwrap();
    store
        .create(
            ACTOR,
            ObjectMeta::named("gpt"),
            ModelGroupSpec {
                aliases: vec!["gpt-latest".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    store
        .create(
            ACTOR,
            ObjectMeta::named("gpt-openai"),
            DeploymentSpec {
                model_group: "gpt".into(),
                provider: ProviderKind::OpenAi,
                endpoint: None,
                provider_model: "gpt-4o".into(),
                credential: Some("openai".into()),
                weight: 1,
                priority: 0,
                capacity: Default::default(),
                enabled: true,
                settings: Default::default(),
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn cache_roundtrip_and_broken_cache_is_discarded() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    store.migrate().await.unwrap();
    let (k1, hex1) = MasterKey::generate().unwrap();
    let keyring = Arc::new(Keyring::new(k1));
    sample(&store, &keyring).await;
    let snap = load_snapshot(&store, &keyring).await.unwrap();
    let dir = temp_dir("snapcache");
    let path = dir.join("sub").join("snapshot.cache");
    let cache = SnapshotCache::new(&path, Arc::clone(&keyring));
    cache.write(&snap).unwrap();

    let raw = std::fs::read(&path).unwrap();
    assert!(raw.starts_with(b"AIGWSNAP"));
    assert!(
        !raw.windows(9).any(|w| w == b"sk-geheim"),
        "Geheimnisse nie im Klartext auf Platte"
    );
    let back = cache.read().unwrap().unwrap();
    assert_eq!(back.snapshot, snap, "Rundreise verlustfrei");
    let data = back.snapshot.credential_data.get("openai").unwrap();
    assert_eq!(data.get("apiKey").unwrap().expose(), "sk-geheim");

    // Nach einer Master-Key-Rotation öffnet der frühere Key den Cache weiter; ein fremder nicht.
    let rotated = Arc::new(
        Keyring::new(MasterKey::generate().unwrap().0)
            .with_previous(MasterKey::from_hex(&hex1).unwrap()),
    );
    assert_eq!(
        SnapshotCache::new(&path, rotated)
            .read()
            .unwrap()
            .unwrap()
            .snapshot,
        snap
    );
    assert!(
        SnapshotCache::new(&path, ring()).read().is_err(),
        "fremder Master-Key"
    );

    // Ein verändertes Byte → Integritätsfehler → verworfen.
    let mut bad = raw.clone();
    let i = bad.len() - 5;
    bad[i] ^= 0x01;
    std::fs::write(&path, &bad).unwrap();
    assert!(cache.read().is_err());
    assert!(cache.read_or_discard().is_none());
    assert!(!path.exists(), "kaputter Cache gelöscht");

    // Unbekanntes Format bzw. abgeschnittene Datei.
    let mut fmt = raw.clone();
    fmt[9] = 99;
    std::fs::write(&path, &fmt).unwrap();
    assert!(cache.read().is_err());
    std::fs::write(&path, &raw[..20]).unwrap();
    assert!(cache.read().is_err());
    std::fs::write(&path, b"kein cache").unwrap();
    assert!(cache.read().is_err());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn reloader_writes_cache_and_cold_start_recovers() {
    let keyring = ring();
    let dir = temp_dir("snapcold");
    let cache_path = dir.join("snapshot.cache");
    let cache = Arc::new(SnapshotCache::new(&cache_path, Arc::clone(&keyring)));
    let opts = ReloadOptions {
        poll_interval: Some(Duration::from_millis(50)),
        debounce: Duration::from_millis(5),
        backoff_min: Duration::from_millis(20),
        backoff_max: Duration::from_millis(100),
        cache_write_interval: Duration::from_millis(10),
        ..ReloadOptions::default()
    };

    // 1. Normaler Betrieb mit Datenbank: der Reloader schreibt den Cache.
    let db_dir = dir.join("db");
    std::fs::create_dir_all(&db_dir).unwrap();
    let url = format!("sqlite://{}/aigw.db?mode=rwc", db_dir.display());
    let store = Store::connect(&url).await.unwrap();
    store.migrate().await.unwrap();
    sample(&store, &keyring).await;
    let reloader = Reloader::bootstrap(store.clone(), Arc::clone(&keyring), opts.clone())
        .await
        .unwrap()
        .with_cache(Arc::clone(&cache));
    let expected = reloader.handle().load();
    let metrics = reloader.metrics();
    let task = reloader.spawn();
    let deadline = Instant::now() + Duration::from_secs(5);
    while metrics.cache_writes.load(Ordering::Relaxed) == 0 {
        assert!(Instant::now() < deadline, "Cache nicht geschrieben");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    task.shutdown().await;
    store.close().await;
    assert_eq!(cache.read().unwrap().unwrap().snapshot, *expected);

    // 2. Kaltstart: Datenbank nicht erreichbar (Verzeichnis fehlt) → Stand aus dem Cache,
    //    Readiness „degraded“.
    let missing = dir.join("noch-nicht-da");
    let url2 = format!("sqlite://{}/aigw.db?mode=rwc", missing.display());
    let err = Store::connect(&url2).await.unwrap_err();
    assert!(err.is_unavailable(), "{err}");
    let lazy = Store::connect_lazy_with(&aigw_core::config::StoreConfig {
        url: url2,
        ..Default::default()
    })
    .await
    .unwrap();
    let cached = cache.read_or_discard().unwrap();
    let reloader = Reloader::from_cache(lazy.clone(), Arc::clone(&keyring), cached, opts);
    let handle = reloader.handle().clone();
    let st = handle.status();
    assert_eq!(st.source, SnapshotSource::Cache);
    assert!(st.stale);
    assert_eq!(*handle.load(), *expected, "letzter bekannter Stand aktiv");
    let task = reloader.spawn();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(handle.source(), SnapshotSource::Cache, "Datenbank noch weg");
    assert!(handle.status().failing);

    // 3. Datenbank wieder da (hier: leer) → migriert, vollständig neu gebaut, normaler Betrieb.
    std::fs::create_dir_all(&missing).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while handle.source() != SnapshotSource::Database {
        assert!(
            Instant::now() < deadline,
            "kein Neubau nach Rückkehr der Datenbank"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let st = handle.status();
    assert!(!st.stale && !st.failing, "{st:?}");
    assert_eq!(handle.load().version, 0, "Stand der (leeren) Datenbank");
    task.shutdown().await;
    lazy.close().await;
    let _ = std::fs::remove_dir_all(dir);
}

/// Messung: `cargo test --release -p aigw-store --test snapshot_incremental -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "Messung, nicht Teil der normalen Testsuite"]
async fn bench_incremental_vs_full_100k_keys() {
    const KEYS: usize = 100_000;
    let store = Store::connect("sqlite::memory:").await.unwrap();
    store.migrate().await.unwrap();
    let keyring = ring();
    let fill = Instant::now();
    org(&store, "acme").await;
    for t in 0..20 {
        store
            .create(
                ACTOR,
                ObjectMeta::named(format!("team-{t}")),
                TeamSpec {
                    organization: "acme".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }
    for g in 0..200 {
        store
            .create(
                ACTOR,
                ObjectMeta::named(format!("model-{g}")),
                ModelGroupSpec {
                    aliases: vec![format!("alias-{g}")],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }
    for k in 0..KEYS {
        store
            .create_api_key(
                ACTOR,
                ObjectMeta::named(format!("key-{k}")),
                ApiKeySpec {
                    organization: "acme".into(),
                    team: Some(format!("team-{}", k % 20)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }
    eprintln!("Befüllen ({KEYS} Keys): {:?}", fill.elapsed());

    let median = |mut v: Vec<Duration>| {
        v.sort();
        (v[0], v[v.len() / 2], v[v.len() - 1])
    };
    let mut full_times = Vec::new();
    let mut snap = load_snapshot(&store, &keyring).await.unwrap();
    for _ in 0..5 {
        let t = Instant::now();
        snap = load_snapshot(&store, &keyring).await.unwrap();
        full_times.push(t.elapsed());
    }
    assert_eq!(snap.keys.len(), KEYS);

    let mut cases: Vec<(&str, Vec<Duration>, Vec<Duration>)> = Vec::new();
    for case in ["key", "team", "organization", "model-group"] {
        let mut inc_times = Vec::new();
        let mut apply_times = Vec::new();
        for i in 0..21 {
            match case {
                "key" => {
                    let k = store
                        .get_api_key(&format!("key-{}", i * 997))
                        .await
                        .unwrap();
                    let mut spec = k.spec;
                    spec.allowed_models = vec![format!("model-{i}")];
                    store.update_api_key(ACTOR, k.metadata, spec).await.unwrap();
                }
                "organization" => {
                    store
                        .update(
                            ACTOR,
                            ObjectMeta::named("acme"),
                            OrganizationSpec {
                                display_name: Some(format!("Acme {i}")),
                                ..Default::default()
                            },
                        )
                        .await
                        .unwrap();
                }
                "team" => {
                    store
                        .update(
                            ACTOR,
                            ObjectMeta::named(format!("team-{}", i % 20)),
                            TeamSpec {
                                organization: "acme".into(),
                                display_name: Some(format!("T{i}")),
                                ..Default::default()
                            },
                        )
                        .await
                        .unwrap();
                }
                _ => {
                    store
                        .update(
                            ACTOR,
                            ObjectMeta::named(format!("model-{i}")),
                            ModelGroupSpec {
                                aliases: vec![format!("alias-{i}"), format!("neu-{i}")],
                                ..Default::default()
                            },
                        )
                        .await
                        .unwrap();
                }
            }
            let t = Instant::now();
            let Incremental::Built { snapshot, .. } =
                load_incremental(&store, &keyring, &snap, 1000)
                    .await
                    .unwrap()
            else {
                panic!("inkrementell erwartet");
            };
            inc_times.push(t.elapsed());
            // Reiner Einarbeitungsanteil (ohne Datenbank): erneut auf denselben Stand.
            snap = *snapshot;
        }
        // apply_patch allein: Patch einer Key-Änderung auf 100 000 Keys.
        for _ in 0..5 {
            let t = Instant::now();
            let s2 = snap.apply_patch(aigw_core::snapshot::SnapshotPatch {
                version: snap.version,
                ..Default::default()
            });
            apply_times.push(t.elapsed());
            assert_eq!(s2.keys.len(), KEYS);
        }
        cases.push((case, inc_times, apply_times));
    }
    let full = load_snapshot(&store, &keyring).await.unwrap();
    assert_eq!(
        full, snap,
        "inkrementell = vollständig nach allen Messungen"
    );

    let (fmin, fmed, fmax) = median(full_times);
    eprintln!("Vollständig, {KEYS} Keys: min {fmin:?}, Median {fmed:?}, max {fmax:?}");
    for (case, inc, apply) in cases {
        let (imin, imed, imax) = median(inc);
        let (_, amed, _) = median(apply);
        eprintln!(
            "Inkrementell, 1 Änderung ({case}): min {imin:?}, Median {imed:?}, max {imax:?} \
             (davon Kopie/Einarbeitung ohne DB ≈ {amed:?})"
        );
    }
}

// --- Löschen + gleichnamige Neuanlage ----------------------------------------------------------
//
// Verdacht aus dem UI-e2e: Ein API-Key, der gelöscht und sofort unter demselben Namen neu
// angelegt wurde, lieferte an der Datenebene 401. Die Szenarien prüfen den inkrementellen
// Snapshot dafür über alle betroffenen Arten – Schritt für Schritt (ein Neubau je Version) und
// gesammelt (alle Versionen in einem Neubau, wie in einem Debounce-Fenster).

/// Inkrementell fortgeschriebener Snapshot, bei jedem Abgleich mit dem vollständigen Neubau
/// verglichen.
struct Probe<'a> {
    store: &'a Store,
    keyring: Arc<Keyring>,
    /// `true`: nach jeder Änderung einarbeiten; `false`: erst bei [`Probe::sync`].
    step: bool,
    snap: Snapshot,
}

impl<'a> Probe<'a> {
    async fn new(store: &'a Store, keyring: Arc<Keyring>, step: bool) -> Self {
        let snap = load_snapshot(store, &keyring).await.unwrap();
        Self {
            store,
            keyring,
            step,
            snap,
        }
    }

    async fn advance(&mut self) {
        match load_incremental(self.store, &self.keyring, &self.snap, 10_000)
            .await
            .unwrap()
        {
            Incremental::UpToDate => {}
            Incremental::Built { snapshot, .. } => self.snap = *snapshot,
            Incremental::NeedFull(f) => panic!("unerwarteter Rückfall {f:?}"),
        }
    }

    /// Nach einer Änderung (im Schrittmodus sofort einarbeiten).
    async fn changed(&mut self) {
        if self.step {
            self.advance().await;
        }
    }

    /// Einarbeiten, mit dem vollständigen Neubau vergleichen und den Stand liefern.
    async fn sync(&mut self, what: &str) -> &Snapshot {
        self.advance().await;
        let full = load_snapshot(self.store, &self.keyring).await.unwrap();
        assert_eq!(self.snap.version, full.version, "{what}");
        if self.snap != full {
            panic!(
                "{what} (step={}): inkrementell ≠ vollständig\n\
                 inkrementell: {:#?}\nvollständig: {:#?}",
                self.step,
                self.snap.to_parts(),
                full.to_parts()
            );
        }
        &self.snap
    }

    async fn create_key(&mut self, name: &str, org: &str, team: Option<&str>) -> (Id, KeyHash) {
        let created = self
            .store
            .create_api_key(
                ACTOR,
                ObjectMeta::named(name),
                ApiKeySpec {
                    organization: org.into(),
                    team: team.map(Into::into),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        self.changed().await;
        (created.key.id, hash_api_key(&created.secret))
    }

    async fn delete(&mut self, kind: ResourceKind, name: &str) {
        self.store.delete(ACTOR, kind, name).await.unwrap();
        self.changed().await;
    }

    async fn create<R: PlainResource>(&mut self, name: &str, spec: R) -> Id {
        let id = self
            .store
            .create(ACTOR, ObjectMeta::named(name), spec)
            .await
            .unwrap()
            .id;
        self.changed().await;
        id
    }
}

fn team_spec(org: &str) -> TeamSpec {
    TeamSpec {
        organization: org.into(),
        ..Default::default()
    }
}

fn deployment_spec(group: &str, model: &str) -> DeploymentSpec {
    DeploymentSpec {
        model_group: group.into(),
        provider: ProviderKind::OpenAi,
        endpoint: None,
        provider_model: model.into(),
        credential: None,
        weight: 1,
        priority: 0,
        capacity: Default::default(),
        enabled: true,
        settings: Default::default(),
    }
}

fn key_binding(key: &str) -> PluginBindingSpec {
    PluginBindingSpec {
        inherit: None,
        locked: false,
        fail_mode: None,
        timeout_ms: None,
        output_reject_mode: None,
        scope: Scope {
            level: ScopeLevel::ApiKey,
            target: Some(key.into()),
        },
        plugin: "header".into(),
        order: 0,
        enabled: true,
        config: Default::default(),
    }
}

/// Alle Varianten „löschen und gleichnamig neu anlegen“ auf einem frischen Store.
async fn recreate_same_name(store: &Store, step: bool) {
    let keyring = ring();
    org(store, "acme").await;
    org(store, "beta").await;
    store
        .create(ACTOR, ObjectMeta::named("platform"), team_spec("acme"))
        .await
        .unwrap();
    store
        .create(ACTOR, ObjectMeta::named("ops"), team_spec("beta"))
        .await
        .unwrap();
    store
        .create(
            ACTOR,
            ObjectMeta::named("gpt"),
            ModelGroupSpec {
                aliases: vec!["gpt-latest".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    store
        .create(
            ACTOR,
            ObjectMeta::named("gpt-openai"),
            deployment_spec("gpt", "gpt-4o"),
        )
        .await
        .unwrap();
    let mut p = Probe::new(store, Arc::clone(&keyring), step).await;
    let tag = |s: &str| format!("{s} (step={step})");

    // 1. Key löschen und sofort unter demselben Namen (gleicher Scope) neu anlegen.
    let (_, h1) = p.create_key("ci", "acme", Some("platform")).await;
    assert!(p.sync(&tag("Key angelegt")).await.key(&h1).is_some());
    p.delete(ResourceKind::ApiKey, "ci").await;
    let (id2, h2) = p.create_key("ci", "acme", Some("platform")).await;
    let s = p.sync(&tag("Key gelöscht + neu (gleicher Scope)")).await;
    assert!(s.key(&h1).is_none(), "alter Key wirkt nicht mehr");
    let ctx = s.key(&h2).expect("neu angelegter Key fehlt im Snapshot");
    assert_eq!(ctx.key.id, id2);
    assert_eq!(s.key_ids.get(&id2), Some(&h2));
    assert_eq!(ctx.team.as_ref().unwrap().metadata.name, "platform");

    // 2. Gleicher Name, andere Organisation/Team – und ohne Team.
    p.delete(ResourceKind::ApiKey, "ci").await;
    let (id3, h3) = p.create_key("ci", "beta", Some("ops")).await;
    let s = p.sync(&tag("Key neu in anderer Org/Team")).await;
    assert!(s.key(&h2).is_none());
    let ctx = s.key(&h3).expect("Key in anderer Org fehlt");
    assert_eq!(ctx.key.id, id3);
    assert_eq!(ctx.organization.as_ref().unwrap().metadata.name, "beta");
    assert_eq!(ctx.team.as_ref().unwrap().metadata.name, "ops");
    p.delete(ResourceKind::ApiKey, "ci").await;
    let (_, h4) = p.create_key("ci", "acme", None).await;
    let s = p.sync(&tag("Key neu ohne Team")).await;
    assert!(s.key(&h3).is_none());
    let ctx = s.key(&h4).expect("Key ohne Team fehlt");
    assert!(ctx.team.is_none());
    assert_eq!(ctx.organization.as_ref().unwrap().metadata.name, "acme");

    // 3. Rotation: alter Key bleibt bis zum Ablauf gültig; danach alten löschen und unter
    //    seinem Namen neu anlegen, ebenso den rotierten Namen.
    let (rotated, _) = store
        .rotate_api_key(
            ACTOR,
            "ci",
            ObjectMeta::named("ci-next"),
            OffsetDateTime::now_utc() + time::Duration::hours(1),
            aigw_store::Expect::Any,
        )
        .await
        .unwrap();
    p.changed().await;
    let h_next = hash_api_key(&rotated.secret);
    let s = p.sync(&tag("Rotation")).await;
    assert!(s.key(&h4).is_some() && s.key(&h_next).is_some());
    p.delete(ResourceKind::ApiKey, "ci").await;
    let (_, h5) = p.create_key("ci", "acme", None).await;
    p.delete(ResourceKind::ApiKey, "ci-next").await;
    let (_, h6) = p.create_key("ci-next", "beta", None).await;
    let s = p.sync(&tag("nach Rotation gelöscht + neu")).await;
    assert!(s.key(&h4).is_none() && s.key(&h_next).is_none());
    assert!(s.key(&h5).is_some(), "Neuanlage unter altem Namen fehlt");
    assert!(
        s.key(&h6).is_some(),
        "Neuanlage unter rotiertem Namen fehlt"
    );

    // 4. Widerrufener Key gelöscht und gleichnamig neu.
    store
        .set_api_key_state(ACTOR, "ci", ApiKeyState::Revoked, aigw_store::Expect::Any)
        .await
        .unwrap();
    p.changed().await;
    p.delete(ResourceKind::ApiKey, "ci").await;
    let (_, h7) = p.create_key("ci", "acme", Some("platform")).await;
    let s = p.sync(&tag("widerrufen, gelöscht + neu")).await;
    assert!(s.key(&h5).is_none());
    assert!(s.key(&h7).is_some());

    // 5. Key mit Plugin-Bindung gelöscht (Bindung fällt mit), gleichnamig neu samt Bindung.
    p.create("ci-hdr", key_binding("ci")).await;
    let old_binding = p.sync(&tag("Bindung")).await.plugin_bindings[0].id;
    p.delete(ResourceKind::ApiKey, "ci").await;
    let (_, h8) = p.create_key("ci", "acme", Some("platform")).await;
    let b8 = p.create("ci-hdr", key_binding("ci")).await;
    let s = p.sync(&tag("Key + Bindung neu")).await;
    assert!(s.key(&h7).is_none());
    assert!(s.key(&h8).is_some());
    assert_eq!(s.plugin_bindings.len(), 1);
    assert_eq!(s.plugin_bindings[0].id, b8);
    assert_ne!(b8, old_binding);

    // 6. Modellgruppe (samt Deployment) löschen und gleichnamig neu anlegen.
    p.delete(ResourceKind::Deployment, "gpt-openai").await;
    p.delete(ResourceKind::ModelGroup, "gpt").await;
    let g2 = p
        .create(
            "gpt",
            ModelGroupSpec {
                aliases: vec!["gpt-neu".into()],
                ..Default::default()
            },
        )
        .await;
    let d2 = p
        .create("gpt-openai", deployment_spec("gpt", "gpt-4.1"))
        .await;
    let s = p.sync(&tag("Modellgruppe neu")).await;
    let g = s.model_groups.get("gpt").expect("Gruppe fehlt");
    assert_eq!(g.group.id, g2);
    assert_eq!(g.deployments.len(), 1);
    assert_eq!(g.deployments[0].deployment.id, d2);
    assert_eq!(g.deployments[0].deployment.spec.provider_model, "gpt-4.1");
    assert!(s.model_groups.contains_key("gpt-neu"));
    assert!(!s.model_groups.contains_key("gpt-latest"), "alter Alias");
    p.delete(ResourceKind::Deployment, "gpt-openai").await;
    p.delete(ResourceKind::ModelGroup, "gpt").await;
    let g3 = p.create("gpt", ModelGroupSpec::default()).await;
    let s = p.sync(&tag("Modellgruppe ohne Deployments neu")).await;
    let g = s.model_groups.get("gpt").expect("Gruppe fehlt");
    assert_eq!(g.group.id, g3);
    assert!(
        g.deployments.is_empty(),
        "Deployments der alten Gruppe übernommen"
    );

    // 7. Team löschen und gleichnamig in einer anderen Organisation neu, Key gleichnamig neu.
    p.delete(ResourceKind::ApiKey, "ci").await;
    p.delete(ResourceKind::Team, "platform").await;
    let t2 = p.create("platform", team_spec("beta")).await;
    let (_, h9) = p.create_key("ci", "beta", Some("platform")).await;
    let s = p.sync(&tag("Team neu in anderer Org")).await;
    assert!(s.key(&h8).is_none());
    let ctx = s.key(&h9).expect("Key im neuen Team fehlt");
    assert_eq!(ctx.team.as_ref().unwrap().id, t2);
    assert_eq!(ctx.organization.as_ref().unwrap().metadata.name, "beta");

    // 8. Organisation löschen und gleichnamig neu; Key gleichnamig in der neuen Org.
    p.delete(ResourceKind::ApiKey, "ci").await;
    p.delete(ResourceKind::ApiKey, "ci-next").await;
    p.delete(ResourceKind::Team, "platform").await;
    p.delete(ResourceKind::Team, "ops").await;
    p.delete(ResourceKind::Organization, "beta").await;
    let o2 = p.create("beta", OrganizationSpec::default()).await;
    let (_, h10) = p.create_key("ci", "beta", None).await;
    let s = p.sync(&tag("Organisation neu")).await;
    assert!(s.key(&h9).is_none() && s.key(&h6).is_none());
    let ctx = s.key(&h10).expect("Key in neuer Org fehlt");
    assert_eq!(ctx.organization.as_ref().unwrap().id, o2);
}

#[tokio::test]
async fn recreate_same_name_sqlite() {
    for step in [true, false] {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        store.migrate().await.unwrap();
        recreate_same_name(&store, step).await;
        store.close().await;
    }
}

#[tokio::test]
async fn recreate_same_name_postgres() {
    for step in [true, false] {
        let Some((store, admin, db)) = pg_store("snaprecreate").await else {
            return;
        };
        recreate_same_name(&store, step).await;
        pg_drop(store, admin, db).await;
    }
}

/// Wie im Betrieb: laufender Reloader (Debounce), Löschen + Neuanlage innerhalb eines
/// Debounce-Fensters bzw. über zwei Fenster verteilt.
async fn reloader_recreate(store: Store) {
    let keyring = ring();
    org(&store, "acme").await;
    let opts = ReloadOptions {
        poll_interval: None,
        debounce: Duration::from_millis(150),
        verify_interval: None,
        ..ReloadOptions::default()
    };
    let reloader = Reloader::bootstrap(store.clone(), Arc::clone(&keyring), opts)
        .await
        .unwrap();
    let handle = reloader.handle().clone();
    let metrics = reloader.metrics();
    let task = reloader.spawn();
    let wait = |want: i64| {
        let handle = handle.clone();
        async move {
            let deadline = Instant::now() + Duration::from_secs(10);
            while handle.version() < want {
                assert!(Instant::now() < deadline, "Snapshot {want} nicht erreicht");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    };
    let create = |name: &'static str| {
        let store = store.clone();
        async move {
            let c = store
                .create_api_key(
                    ACTOR,
                    ObjectMeta::named(name),
                    ApiKeySpec {
                        organization: "acme".into(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            hash_api_key(&c.secret)
        }
    };
    let mut hash = create("ci").await;
    wait(store.config_version().await.unwrap()).await;
    assert!(handle.load().key(&hash).is_some());
    for round in 0..10 {
        store
            .delete(ACTOR, ResourceKind::ApiKey, "ci")
            .await
            .unwrap();
        if round % 2 == 1 {
            // Über zwei Fenster verteilt.
            wait(store.config_version().await.unwrap()).await;
            assert!(
                handle.load().key(&hash).is_none(),
                "gelöschter Key wirkt noch"
            );
        }
        let old = hash;
        hash = create("ci").await;
        wait(store.config_version().await.unwrap()).await;
        let snap = handle.load();
        assert!(
            snap.key(&old).is_none(),
            "Runde {round}: alter Key wirkt noch"
        );
        assert!(
            snap.key(&hash).is_some(),
            "Runde {round}: neu angelegter Key fehlt (Version {})",
            snap.version
        );
    }
    assert_eq!(metrics.incremental_fallbacks.load(Ordering::Relaxed), 0);
    assert!(metrics.reloads_incremental.load(Ordering::Relaxed) > 0);
    let full = load_snapshot(&store, &keyring).await.unwrap();
    assert_eq!(*handle.load(), full);
    task.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reloader_recreate_same_name_sqlite() {
    let dir = temp_dir("snaprecreate");
    let url = format!("sqlite://{}/aigw.db?mode=rwc", dir.display());
    let store = Store::connect(&url).await.unwrap();
    store.migrate().await.unwrap();
    reloader_recreate(store.clone()).await;
    store.close().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reloader_recreate_same_name_postgres() {
    let Some((store, admin, db)) = pg_store("snaprecreatereload").await else {
        return;
    };
    reloader_recreate(store.clone()).await;
    pg_drop(store, admin, db).await;
}
