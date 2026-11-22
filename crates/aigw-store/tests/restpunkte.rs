// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! AIGW-78 gegen SQLite (Datei) und – mit `AIGW_TEST_POSTGRES_URL` – Postgres:
//! Plugin-Steuerfelder (F117, auch alte Bindungen), Audit-Kette und Aufbewahrung
//! (F115/F116).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::collections::BTreeMap;

use aigw_core::resource::{
    McpServerSpec, ObjectMeta, OrganizationSpec, PluginBindingSpec, PluginFailMode, ResourceKind,
    SecretRef, SecretSpec, SecretString, TeamSpec, new_id,
};
use aigw_store::gitops::{ApplyOptions, SecretSource, apply, export_yaml, parse_documents};
use aigw_store::{Keyring, MasterKey, Store, StoreError, load_snapshot};
use sqlx::AssertSqlSafe;

const ACTOR: &str = "test";

/// Führt rohes SQL auf derselben Datenbank aus (Manipulation „von außen“).
#[derive(Clone)]
enum Raw {
    Sqlite(sqlx::SqlitePool),
    Pg(sqlx::PgPool),
}

impl Raw {
    async fn exec(&self, sql: &str) {
        match self {
            Self::Sqlite(p) => {
                sqlx::query(AssertSqlSafe(sql.to_owned()))
                    .execute(p)
                    .await
                    .unwrap();
            }
            Self::Pg(p) => {
                sqlx::query(AssertSqlSafe(sql.to_owned()))
                    .execute(p)
                    .await
                    .unwrap();
            }
        }
    }

    /// Wahrheitswert in SQL.
    fn bool(&self, b: bool) -> &'static str {
        match (self, b) {
            (Self::Sqlite(_), true) => "1",
            (Self::Sqlite(_), false) => "0",
            (Self::Pg(_), true) => "TRUE",
            (Self::Pg(_), false) => "FALSE",
        }
    }
}

/// Geheimnisse für GitOps aus einer festen Tabelle.
struct Env(BTreeMap<String, String>);

impl SecretSource for Env {
    fn env(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
    fn file(&self, path: &str) -> std::io::Result<String> {
        std::fs::read_to_string(path)
    }
}

/// F149: Ressource `Secret` – verschlüsselt, ohne Werte lesbar, Snapshot, MCP-Verweis,
/// GitOps mit `valueFrom`, Rotation.
async fn secrets(store: &Store, raw: &Raw) {
    let (master, master_hex) = MasterKey::generate().unwrap();
    let ring = Keyring::new(master);
    // Basis für den inkrementellen Neubau (AIGW-75): Secrets laufen über das Journal.
    let base = load_snapshot(store, &ring).await.unwrap();
    let spec = |v: &str| SecretSpec {
        description: Some("HMAC für hooks".into()),
        data: BTreeMap::from([("value".to_owned(), SecretRef::value(v))]),
    };
    let s = store
        .create_secret(ACTOR, &ring, ObjectMeta::named("hooks"), spec("hmac-1"))
        .await
        .unwrap();
    assert_eq!(s.spec.fields, ["value"]);
    assert_eq!(s.spec.description.as_deref(), Some("HMAC für hooks"));
    let json = serde_json::to_string(&s).unwrap();
    assert!(
        !json.contains("hmac-1"),
        "keine Werte in der gelesenen Sicht"
    );
    // Ungültig: keine Felder, schlechter Feldname.
    assert!(matches!(
        store
            .create_secret(
                ACTOR,
                &ring,
                ObjectMeta::named("empty"),
                SecretSpec {
                    description: None,
                    data: BTreeMap::new()
                }
            )
            .await,
        Err(StoreError::Invalid(_))
    ));
    // Audit ohne Wert, Art kebab-case.
    let log = store.audit_log(1).await.unwrap();
    assert_eq!(
        (log[0].kind.as_str(), log[0].action.as_str()),
        ("secret", "create")
    );
    assert!(!serde_json::to_string(&log[0]).unwrap().contains("hmac-1"));

    // Patch: Feld hinzufügen, Beschreibung entfernen; Wert bleibt.
    let p = store
        .patch_secret(
            ACTOR,
            &ring,
            "hooks",
            None,
            Some(None),
            BTreeMap::from([("apiKey".to_owned(), Some(SecretString::new("k-2")))]),
            aigw_store::Expect::Any,
        )
        .await
        .unwrap();
    assert_eq!(p.spec.fields, ["apiKey", "value"]);
    assert_eq!(p.spec.description, None);
    let all = store.decrypt_secrets(&ring).await.unwrap();
    assert_eq!(all["hooks"].1.get("value").unwrap().expose(), "hmac-1");
    assert_eq!(all["hooks"].1.get("apiKey").unwrap().expose(), "k-2");

    // Snapshot + MCP-Server mit Secret-Verweis.
    store
        .create(
            ACTOR,
            ObjectMeta::named("gh"),
            McpServerSpec {
                url: "https://mcp.example.org/mcp".into(),
                transport: Default::default(),
                credential: None,
                secret: Some("hooks".into()),
                enabled: true,
                settings: Default::default(),
            },
        )
        .await
        .unwrap();
    let snap = load_snapshot(store, &ring).await.unwrap();
    assert_eq!(
        snap.secret_data["hooks"].get("value").unwrap().expose(),
        "hmac-1"
    );
    match aigw_store::load_incremental(store, &ring, &base, 1000)
        .await
        .unwrap()
    {
        aigw_store::Incremental::Built { snapshot, .. } => assert_eq!(*snapshot, snap),
        other => panic!("inkrementell erwartet: {other:?}"),
    }
    // Patch-Weg und Platten-Cache-Form (to_parts/from_parts) behalten Secrets.
    assert_eq!(
        aigw_core::snapshot::Snapshot::from_parts(snap.to_parts()),
        snap
    );
    let gh = &snap.mcp_servers["gh"];
    assert_eq!(gh.server.spec.secret.as_deref(), Some("hooks"));
    assert_eq!(
        gh.credential
            .as_ref()
            .unwrap()
            .get("apiKey")
            .unwrap()
            .expose(),
        "k-2"
    );
    // In Verwendung → Löschen 409.
    assert!(matches!(
        store.delete(ACTOR, ResourceKind::Secret, "hooks").await,
        Err(StoreError::Conflict(_))
    ));
    store
        .delete(ACTOR, ResourceKind::McpServer, "gh")
        .await
        .unwrap();

    // Rotation (gemeinsam mit Zugangsdaten).
    let (master2, _) = MasterKey::generate().unwrap();
    let ring2 = Keyring::new(master2).with_previous(MasterKey::from_hex(&master_hex).unwrap());
    assert_eq!(store.rotate_credentials(ACTOR, &ring2).await.unwrap(), 1);
    assert_eq!(store.rotate_credentials(ACTOR, &ring2).await.unwrap(), 0);
    let s = store.get_secret("hooks").await.unwrap();
    assert_eq!(s.spec.key_id, ring2.primary_id());
    assert_eq!(
        store.decrypt_secrets(&ring2).await.unwrap()["hooks"]
            .1
            .get("value")
            .unwrap()
            .expose(),
        "hmac-1"
    );

    // GitOps: valueFrom auflösen, Verweis originalgetreu exportieren, Klartext ablehnen.
    let yaml = "apiVersion: aigw/v1alpha1\nkind: Secret\nmetadata: {name: cs}\nspec:\n  description: Azure Content Safety\n  data:\n    apiKey:\n      valueFrom: {env: CS_KEY}\n";
    let docs = parse_documents(yaml).unwrap();
    let env = Env(BTreeMap::from([("CS_KEY".to_owned(), "cs-1".to_owned())]));
    let r = apply(store, &ring2, &docs, &ApplyOptions::default(), &env)
        .await
        .unwrap();
    assert!(r.applied);
    assert_eq!(
        store.decrypt_secrets(&ring2).await.unwrap()["cs"]
            .1
            .get("apiKey")
            .unwrap()
            .expose(),
        "cs-1"
    );
    let out = export_yaml(store, aigw_store::gitops::ApiKeyMode::Ignore)
        .await
        .unwrap();
    assert!(out.contains("kind: Secret"), "{out}");
    assert!(out.contains("CS_KEY"), "Verweis originalgetreu: {out}");
    assert!(
        out.contains("AIGW_SECRET_HOOKS_VALUE"),
        "Platzhalter: {out}"
    );
    assert!(!out.contains("cs-1") && !out.contains("hmac-1"));
    // Unverändert → kein Plan.
    let r = apply(store, &ring2, &docs, &ApplyOptions::default(), &env)
        .await
        .unwrap();
    assert!(!r.applied);
    let plain = "apiVersion: aigw/v1alpha1\nkind: Secret\nmetadata: {name: x}\nspec:\n  data:\n    value: {value: geheim}\n";
    assert!(parse_documents(plain).is_err());
    let _ = raw;
}

async fn run(store: Store, raw: Raw) {
    store.migrate().await.unwrap();
    secrets(&store, &raw).await;
    // Für die Kettenprüfungen unten mit leerem Audit-Log beginnen.
    raw.exec("DELETE FROM audit_log").await;
    raw.exec(
        "UPDATE audit_chain SET seq = 0, anchor_seq = 0,
             head = '0000000000000000000000000000000000000000000000000000000000000000',
             anchor_hash = '0000000000000000000000000000000000000000000000000000000000000000'",
    )
    .await;

    // --- F117: Steuerfelder ------------------------------------------------------------
    let legacy: PluginBindingSpec = serde_json::from_value(serde_json::json!({
        "scope": {"level": "global"}, "plugin": "pii",
        "config": {"action": "mask", "failMode": "open", "timeoutMs": 250}
    }))
    .unwrap();
    assert_eq!(legacy.fail_mode, Some(PluginFailMode::Open));
    let created = store
        .create(ACTOR, ObjectMeta::named("pii-global"), legacy)
        .await
        .unwrap();
    assert_eq!(created.spec.fail_mode, Some(PluginFailMode::Open));
    assert_eq!(created.spec.timeout_ms, Some(250));
    assert!(!created.spec.config.contains_key("failMode"));

    // Alte Zeile (vor F117): Steuerschlüssel in `config`, Spalten leer → beim Lesen übernommen.
    raw.exec(&format!(
        "UPDATE plugin_binding SET fail_mode = NULL, timeout_ms = NULL, locked = {},
             config = '{{\"action\":\"mask\",\"failMode\":\"closed\",\"locked\":true}}'
         WHERE name = 'pii-global'",
        raw.bool(false)
    ))
    .await;
    let read = store.get::<PluginBindingSpec>("pii-global").await.unwrap();
    assert_eq!(read.spec.fail_mode, Some(PluginFailMode::Closed));
    assert!(read.spec.locked);
    assert_eq!(read.spec.timeout_ms, None);
    assert_eq!(
        serde_json::Value::Object(read.spec.config.clone()),
        serde_json::json!({"action": "mask"})
    );
    // Ungültiger alter Wert: Lesen klappt trotzdem (Schlüssel bleibt in `config`).
    raw.exec(
        "UPDATE plugin_binding SET config = '{\"failMode\":\"sometimes\"}' WHERE name = 'pii-global'",
    )
    .await;
    let read = store.get::<PluginBindingSpec>("pii-global").await.unwrap();
    assert!(read.spec.config.contains_key("failMode"));

    // --- F115/F116: Audit-Kette ---------------------------------------------------------
    for n in ["acme", "beta"] {
        store
            .create(ACTOR, ObjectMeta::named(n), OrganizationSpec::default())
            .await
            .unwrap();
    }
    store
        .create(
            ACTOR,
            ObjectMeta::named("platform"),
            TeamSpec {
                organization: "acme".into(),
                ..TeamSpec::default()
            },
        )
        .await
        .unwrap();
    store
        .delete(ACTOR, ResourceKind::Organization, "beta")
        .await
        .unwrap();
    let log = store.audit_log(10).await.unwrap();
    assert_eq!(log[0].kind, "organization", "F115: kebab-case");
    assert_eq!(log[1].kind, "team");
    assert!(log.iter().any(|e| e.kind == "plugin-binding"));

    let ok = store.verify_audit_chain().await.unwrap();
    assert!(ok.ok, "{ok:?}");
    assert_eq!(ok.checked, 5);
    assert_eq!(ok.head_seq, 5);
    assert_eq!(ok.unchained, 0);

    // Inhalt geändert → Hash passt nicht.
    raw.exec("UPDATE audit_log SET actor = 'mallory' WHERE seq = 3")
        .await;
    let bad = store.verify_audit_chain().await.unwrap();
    assert!(!bad.ok);
    assert!(
        bad.issues
            .iter()
            .any(|i| i.seq == 3 && i.problem.contains("verändert")),
        "{bad:?}"
    );
    raw.exec(&format!(
        "UPDATE audit_log SET actor = '{ACTOR}' WHERE seq = 3"
    ))
    .await;
    assert!(store.verify_audit_chain().await.unwrap().ok);

    // Eintrag in der Mitte gelöscht → Lücke.
    raw.exec("DELETE FROM audit_log WHERE seq = 2").await;
    let gap = store.verify_audit_chain().await.unwrap();
    assert!(!gap.ok);
    assert!(gap.issues.iter().any(|i| i.problem.contains("fehlen")));

    // Ende abgeschnitten → Kopf passt nicht (eigene, neue Kette für den Rest des Tests).
    raw.exec("DELETE FROM audit_log").await;
    raw.exec(
        "UPDATE audit_chain SET seq = 0, anchor_seq = 0,
             head = '0000000000000000000000000000000000000000000000000000000000000000',
             anchor_hash = '0000000000000000000000000000000000000000000000000000000000000000'",
    )
    .await;
    for n in ["c1", "c2", "c3"] {
        store
            .create(ACTOR, ObjectMeta::named(n), OrganizationSpec::default())
            .await
            .unwrap();
    }
    assert!(store.verify_audit_chain().await.unwrap().ok);
    raw.exec("DELETE FROM audit_log WHERE seq = 3").await;
    let cut = store.verify_audit_chain().await.unwrap();
    assert!(!cut.ok);
    assert!(cut.issues.iter().any(|i| i.problem.contains("Kettenende")));

    // Aufbewahrung: Anfang löschen, Anker setzen, Kette bleibt prüfbar.
    raw.exec("DELETE FROM audit_log").await;
    raw.exec(
        "UPDATE audit_chain SET seq = 0, anchor_seq = 0,
             head = '0000000000000000000000000000000000000000000000000000000000000000',
             anchor_hash = '0000000000000000000000000000000000000000000000000000000000000000'",
    )
    .await;
    for n in ["d1", "d2"] {
        store
            .create(ACTOR, ObjectMeta::named(n), OrganizationSpec::default())
            .await
            .unwrap();
    }
    // Ein unverketteter Alteintrag (vor Migration 0005).
    let old_at = match &raw {
        Raw::Sqlite(_) => "1000".to_owned(),
        Raw::Pg(_) => "TIMESTAMPTZ '1970-01-01 00:00:01+00'".to_owned(),
    };
    raw.exec(&format!(
        "INSERT INTO audit_log (id, at, actor, action, kind, config_version)
         VALUES ('{}', {old_at}, 'x', 'create', 'Team', 1)",
        new_id()
    ))
    .await;
    let before = store.verify_audit_chain().await.unwrap();
    assert!(before.ok);
    assert_eq!(before.unchained, 1);
    // AIGW-58: unverketteter Eintrag NACH Kettenbeginn = am Gateway vorbei eingeschoben.
    let late_at = match &raw {
        Raw::Sqlite(_) => "99999999999999".to_owned(),
        Raw::Pg(_) => "TIMESTAMPTZ '2999-01-01 00:00:00+00'".to_owned(),
    };
    let forged = new_id();
    raw.exec(&format!(
        "INSERT INTO audit_log (id, at, actor, action, kind, config_version)
         VALUES ('{forged}', {late_at}, 'mallory', 'delete', 'Team', 1)"
    ))
    .await;
    let forged_report = store.verify_audit_chain().await.unwrap();
    assert!(!forged_report.ok);
    assert!(
        forged_report
            .issues
            .iter()
            .any(|i| i.problem.contains("ohne Kettenfelder")),
        "{forged_report:?}"
    );
    raw.exec(&format!("DELETE FROM audit_log WHERE id = '{forged}'"))
        .await;
    let deleted = store
        .prune_audit_log(time::OffsetDateTime::now_utc() + time::Duration::minutes(1))
        .await
        .unwrap();
    assert_eq!(deleted, 3);
    let after = store.verify_audit_chain().await.unwrap();
    assert!(after.ok, "{after:?}");
    assert_eq!(after.anchor_seq, 2);
    assert_eq!(after.checked, 1, "nur der Eintrag über die Löschung");
    assert_eq!(after.unchained, 0);
    let log = store.audit_log(1).await.unwrap();
    assert_eq!(
        (log[0].kind.as_str(), log[0].action.as_str()),
        ("audit-log", "prune")
    );
    // Weiter anhängen nach dem Anker.
    store
        .create(ACTOR, ObjectMeta::named("d3"), OrganizationSpec::default())
        .await
        .unwrap();
    assert!(store.verify_audit_chain().await.unwrap().ok);

    store.close().await;
}

#[tokio::test]
async fn sqlite_file() {
    let dir = std::env::temp_dir().join(format!("aigw-restpunkte-{}", new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let url = format!("sqlite://{}/aigw.db?mode=rwc", dir.display());
    let store = Store::connect(&url).await.unwrap();
    store.migrate().await.unwrap();
    let raw = Raw::Sqlite(sqlx::SqlitePool::connect(&url).await.unwrap());
    run(store, raw).await;
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
    let url = format!("{prefix}/{db}");
    let store = Store::connect(&url).await.unwrap();
    store.migrate().await.unwrap();
    let raw = Raw::Pg(sqlx::PgPool::connect(&url).await.unwrap());
    run(store, raw.clone()).await;
    if let Raw::Pg(p) = raw {
        p.close().await;
    }
    sqlx::query(AssertSqlSafe(format!("DROP DATABASE {db} WITH (FORCE)")))
        .execute(&admin)
        .await
        .unwrap();
}
