// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Hot Reload des Snapshots (AIGW-10): SQLite im Prozess, Versionsabfrage zwischen zwei
//! Instanzen, Fehlerfälle und – falls `AIGW_TEST_POSTGRES_URL` gesetzt ist – Postgres
//! `LISTEN/NOTIFY` zwischen zwei Store-Instanzen.
//!
//! Messung (`--ignored`): Kosten von `load()` und Neubauzeit für 10 000 Keys / 200 Modellgruppen.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aigw_core::resource::{
    ApiKeySpec, DeploymentSpec, ModelGroupSpec, ObjectMeta, OrganizationSpec,
    ProviderCredentialSpec, ResourceKind, SecretRef, TeamSpec, new_id,
};
use aigw_core::snapshot::SnapshotHandle;
use aigw_core::types::ProviderKind;
use aigw_store::{Keyring, MasterKey, ReloadOptions, Reloader, Store};
use sqlx::AssertSqlSafe;

const ACTOR: &str = "test";
const LIMIT: Duration = Duration::from_secs(1);

fn ring() -> Arc<Keyring> {
    Arc::new(Keyring::new(MasterKey::generate().unwrap().0))
}

/// Nur Benachrichtigungen, keine Versionsabfrage (sonst wäre „< 1 s“ kein Nachweis).
fn push_only() -> ReloadOptions {
    ReloadOptions {
        poll_interval: None,
        ..ReloadOptions::default()
    }
}

async fn org(store: &Store, name: &str) -> i64 {
    store
        .create(ACTOR, ObjectMeta::named(name), OrganizationSpec::default())
        .await
        .unwrap();
    store.config_version().await.unwrap()
}

/// Wartet, bis `handle` mindestens `version` zeigt; gibt die Wartezeit zurück.
async fn wait_for(handle: &SnapshotHandle, version: i64, limit: Duration) -> Duration {
    let started = Instant::now();
    let mut rx = handle.subscribe();
    tokio::time::timeout(limit, rx.wait_for(|v| *v >= version))
        .await
        .unwrap_or_else(|_| {
            panic!(
                "Snapshot {version} nicht innerhalb von {limit:?} sichtbar (aktiv: {})",
                handle.version()
            )
        })
        .unwrap();
    started.elapsed()
}

fn temp_sqlite_url() -> (std::path::PathBuf, String) {
    let dir = std::env::temp_dir().join(format!("aigw-reload-test-{}", new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let url = format!("sqlite://{}/aigw.db?mode=rwc", dir.display());
    (dir, url)
}

#[tokio::test]
async fn sqlite_commit_triggers_reload() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    store.migrate().await.unwrap();
    let reloader = Reloader::bootstrap(store.clone(), ring(), push_only())
        .await
        .unwrap();
    let handle = reloader.handle().clone();
    let metrics = reloader.metrics();
    let held = handle.load();
    assert_eq!(held.version, 0);
    let task = reloader.spawn();

    let v = org(&store, "acme").await;
    let took = wait_for(&handle, v, LIMIT).await;
    eprintln!("SQLite: neue Version nach {took:?} sichtbar");
    let snap = handle.load();
    assert_eq!(snap.version, v);
    assert_eq!(snap.organizations.len(), 1);
    assert!(held.organizations.is_empty(), "alter Snapshot unverändert");

    // Viele Änderungen kurz hintereinander werden zusammengefasst.
    let before = metrics
        .reloads_ok
        .load(std::sync::atomic::Ordering::Relaxed);
    let mut last = 0;
    for i in 0..20 {
        last = org(&store, &format!("burst-{i}")).await;
    }
    wait_for(&handle, last, LIMIT).await;
    assert_eq!(handle.load().organizations.len(), 21);
    let rebuilds = metrics
        .reloads_ok
        .load(std::sync::atomic::Ordering::Relaxed)
        - before;
    eprintln!("SQLite: 20 Änderungen → {rebuilds} Neubauten");
    assert!(
        rebuilds < 20,
        "Debounce fasst zusammen ({rebuilds} Neubauten)"
    );

    task.shutdown().await;
    store.close().await;
}

#[tokio::test]
async fn sqlite_two_instances_via_poll() {
    let (dir, url) = temp_sqlite_url();
    let a = Store::connect(&url).await.unwrap();
    a.migrate().await.unwrap();
    let b = Store::connect(&url).await.unwrap();
    let opts = ReloadOptions {
        poll_interval: Some(Duration::from_millis(200)),
        ..ReloadOptions::default()
    };
    let reloader = Reloader::bootstrap(b.clone(), ring(), opts).await.unwrap();
    let handle = reloader.handle().clone();
    let task = reloader.spawn();

    // A ändert; B bekommt keine Benachrichtigung im Prozess, nur die Versionsabfrage.
    let v = org(&a, "acme").await;
    let took = wait_for(&handle, v, LIMIT).await;
    eprintln!("SQLite, zweite Instanz per Abfrage: {took:?}");

    task.shutdown().await;
    a.close().await;
    b.close().await;
    let _ = std::fs::remove_dir_all(dir);
}

/// Kaputte Daten (nicht entschlüsselbare Zugangsdaten) → alter Snapshot bleibt; nach der
/// Reparatur wird wieder geladen.
#[tokio::test]
async fn broken_data_keeps_old_snapshot() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    store.migrate().await.unwrap();
    let writer_ring = ring();
    let reader_ring = ring(); // anderer Master-Key: Entschlüsseln schlägt fehl
    let opts = ReloadOptions {
        backoff_min: Duration::from_millis(50),
        backoff_max: Duration::from_millis(200),
        ..push_only()
    };
    let reloader = Reloader::bootstrap(store.clone(), reader_ring, opts)
        .await
        .unwrap();
    let handle = reloader.handle().clone();
    let metrics = reloader.metrics();
    let task = reloader.spawn();

    let good = org(&store, "acme").await;
    wait_for(&handle, good, LIMIT).await;

    store
        .create_credential(
            ACTOR,
            &writer_ring,
            ObjectMeta::named("openai"),
            ProviderCredentialSpec {
                provider: ProviderKind::OpenAi,
                entra: None,
                data: BTreeMap::from([("apiKey".to_owned(), SecretRef::value("sk-x"))]),
            },
        )
        .await
        .unwrap();
    let broken = store.config_version().await.unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(handle.version(), good, "alter Snapshot bleibt aktiv");
    assert_eq!(handle.load().organizations.len(), 1);
    let failed = metrics
        .reloads_failed
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(failed >= 2, "Fehler gezählt und erneut versucht ({failed})");

    store
        .delete(ACTOR, ResourceKind::ProviderCredential, "openai")
        .await
        .unwrap();
    let fixed = store.config_version().await.unwrap();
    assert!(fixed > broken);
    wait_for(&handle, fixed, LIMIT).await;

    task.shutdown().await;
    store.close().await;
}

/// Datenbank weg → letzter Snapshot bleibt, Versionsabfragen schlagen fehl und werden gezählt.
#[tokio::test]
async fn database_unreachable_keeps_snapshot() {
    let (dir, url) = temp_sqlite_url();
    let store = Store::connect(&url).await.unwrap();
    store.migrate().await.unwrap();
    let v = org(&store, "acme").await;
    let opts = ReloadOptions {
        poll_interval: Some(Duration::from_millis(50)),
        backoff_min: Duration::from_millis(20),
        backoff_max: Duration::from_millis(100),
        ..ReloadOptions::default()
    };
    let reloader = Reloader::bootstrap(store.clone(), ring(), opts)
        .await
        .unwrap();
    let handle = reloader.handle().clone();
    let metrics = reloader.metrics();
    assert!(reloader.reload_now().await.unwrap().is_none(), "aktuell");
    let check = reloader.clone();
    let task = reloader.spawn();

    store.close().await; // Pool geschlossen = Datenbank nicht erreichbar
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(check.reload_now().await.is_err());
    assert!(
        metrics
            .checks_failed
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 2
    );
    assert_eq!(handle.version(), v);
    assert_eq!(handle.load().organizations.len(), 1);

    task.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn postgres_two_instances_listen_notify() {
    let Ok(base) = std::env::var("AIGW_TEST_POSTGRES_URL") else {
        eprintln!("AIGW_TEST_POSTGRES_URL nicht gesetzt – Postgres-Test übersprungen");
        return;
    };
    let db = format!("aigw_reload_{}", new_id().simple());
    let admin = sqlx::PgPool::connect(&base).await.unwrap();
    sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {db}")))
        .execute(&admin)
        .await
        .unwrap();
    let (prefix, _) = base.rsplit_once('/').expect("URL mit Datenbankpfad");
    let url = format!("{prefix}/{db}");

    let a = Store::connect(&url).await.unwrap();
    a.migrate().await.unwrap();
    let b = Store::connect(&url).await.unwrap();
    let ring = ring();
    let ra = Reloader::bootstrap(a.clone(), Arc::clone(&ring), push_only())
        .await
        .unwrap();
    let rb = Reloader::bootstrap(b.clone(), ring, push_only())
        .await
        .unwrap();
    let (ha, hb) = (ra.handle().clone(), rb.handle().clone());
    let mb = rb.metrics();
    let (ta, tb) = (ra.spawn(), rb.spawn());
    // LISTEN steht, sobald B die erste Benachrichtigung nach dem Verbinden verarbeitet hat;
    // kurz warten, damit die Änderung nicht vor dem LISTEN liegt (ohne Abfrage ginge sie verloren).
    tokio::time::sleep(Duration::from_millis(500)).await;

    let t0 = Instant::now();
    let v = org(&a, "acme").await;
    wait_for(&ha, v, LIMIT).await;
    let took_a = t0.elapsed();
    wait_for(&hb, v, LIMIT).await;
    let took_b = t0.elapsed();
    assert!(took_b < LIMIT);
    eprintln!("Postgres ab Commit-Beginn: Instanz A nach {took_a:?}, Instanz B nach {took_b:?}");
    assert_eq!(hb.load().organizations.len(), 1);
    assert!(
        mb.notifications.load(std::sync::atomic::Ordering::Relaxed) >= 1,
        "B wurde per NOTIFY informiert"
    );

    // Und in die Gegenrichtung.
    b.create(
        ACTOR,
        ObjectMeta::named("platform"),
        TeamSpec {
            organization: "acme".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let v2 = b.config_version().await.unwrap();
    wait_for(&ha, v2, LIMIT).await;
    assert_eq!(ha.load().teams.len(), 1);

    ta.shutdown().await;
    tb.shutdown().await;
    a.close().await;
    b.close().await;
    sqlx::query(AssertSqlSafe(format!("DROP DATABASE {db} WITH (FORCE)")))
        .execute(&admin)
        .await
        .unwrap();
}

/// Messung: `cargo test --release -p aigw-store --test reload -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "Messung, nicht Teil der normalen Testsuite"]
async fn bench_load_and_rebuild() {
    const KEYS: usize = 10_000;
    const GROUPS: usize = 200;
    let (dir, url) = temp_sqlite_url();
    let store = Store::connect(&url).await.unwrap();
    store.migrate().await.unwrap();
    let ring = ring();

    let fill = Instant::now();
    store
        .create(
            ACTOR,
            ObjectMeta::named("acme"),
            OrganizationSpec::default(),
        )
        .await
        .unwrap();
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
    store
        .create_credential(
            ACTOR,
            &ring,
            ObjectMeta::named("openai"),
            ProviderCredentialSpec {
                provider: ProviderKind::OpenAi,
                entra: None,
                data: BTreeMap::from([("apiKey".to_owned(), SecretRef::value("sk-x"))]),
            },
        )
        .await
        .unwrap();
    for g in 0..GROUPS {
        let name = format!("model-{g}");
        store
            .create(
                ACTOR,
                ObjectMeta::named(&name),
                ModelGroupSpec {
                    aliases: vec![format!("alias-{g}")],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        store
            .create(
                ACTOR,
                ObjectMeta::named(format!("dep-{g}")),
                DeploymentSpec {
                    model_group: name.clone(),
                    provider: ProviderKind::OpenAi,
                    endpoint: None,
                    provider_model: name,
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
    eprintln!("Befüllen: {:?}", fill.elapsed());

    let mut builds = Vec::new();
    let mut snap = None;
    for _ in 0..5 {
        let t = Instant::now();
        snap = Some(aigw_store::load_snapshot(&store, &ring).await.unwrap());
        builds.push(t.elapsed());
    }
    builds.sort();
    let snap = snap.unwrap();
    assert_eq!(snap.keys.len(), KEYS);
    assert_eq!(snap.model_groups.len(), 2 * GROUPS);
    eprintln!(
        "Neubau {KEYS} Keys / {GROUPS} Gruppen: min {:?}, Median {:?}, max {:?}",
        builds[0], builds[2], builds[4]
    );

    let handle = SnapshotHandle::new(snap);
    const N: u32 = 10_000_000;
    let t = Instant::now();
    for _ in 0..N {
        std::hint::black_box(handle.load());
    }
    let load_ns = t.elapsed().as_nanos() as f64 / f64::from(N);
    let t = Instant::now();
    for _ in 0..N {
        std::hint::black_box(handle.peek().version);
    }
    let peek_ns = t.elapsed().as_nanos() as f64 / f64::from(N);
    eprintln!("load(): {load_ns:.1} ns, peek(): {peek_ns:.1} ns (1 Thread)");

    // 4 Threads gleichzeitig, während ein fünfter laufend neue Snapshots veröffentlicht.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let (h, stop) = (handle.clone(), Arc::clone(&stop));
        let base = handle.load();
        std::thread::spawn(move || {
            let mut swaps = 0u64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                h.store(aigw_core::snapshot::Snapshot {
                    version: base.version + 1 + swaps as i64,
                    ..Default::default()
                });
                swaps += 1;
                std::thread::sleep(Duration::from_millis(1));
            }
            swaps
        })
    };
    let readers: Vec<_> = (0..4)
        .map(|i| {
            let h = handle.clone();
            std::thread::spawn(move || {
                let t = Instant::now();
                for _ in 0..N / 4 {
                    if i % 2 == 0 {
                        std::hint::black_box(h.load());
                    } else {
                        std::hint::black_box(h.peek().version);
                    }
                }
                t.elapsed().as_nanos() as f64 / f64::from(N / 4)
            })
        })
        .collect();
    let per: Vec<f64> = readers.into_iter().map(|r| r.join().unwrap()).collect();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let swaps = writer.join().unwrap();
    eprintln!(
        "4 Leser parallel + {swaps} Austausche: load() {:.1}/{:.1} ns, peek() {:.1}/{:.1} ns",
        per[0], per[2], per[1], per[3]
    );

    store.close().await;
    let _ = std::fs::remove_dir_all(dir);
}
