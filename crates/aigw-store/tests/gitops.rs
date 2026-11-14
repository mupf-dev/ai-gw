// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! GitOps (AIGW-33): Export → Apply → Export identisch, Diff-Fälle, Prune, alles-oder-nichts,
//! Geheimnisse aus Umgebung/Datei, API-Keys – gegen SQLite (In-Memory, Datei) und – mit
//! `AIGW_TEST_POSTGRES_URL` – Postgres (eigene Wegwerf-Datenbank).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::collections::BTreeMap;

use aigw_core::resource::{
    Amount, ModelGroupSpec, ObjectMeta, OrganizationSpec, ResourceKind, ResourceSpec, SecretRef,
    ValueFrom, new_id,
};
use aigw_store::gitops::{
    ApiKeyMode, ApplyOptions, ChangeAction, Document, GitopsError, Plan, SecretPolicy,
    SecretSource, apply, export_yaml, parse_documents, plan,
};
use aigw_store::{Keyring, MasterKey, Store, generate_api_key, hash_api_key, load_snapshot};
use sqlx::AssertSqlSafe;

const EXAMPLE: &str = include_str!("../../../config/examples/gitops.yaml");

/// Geheimnisse aus einer festen Tabelle (statt Prozessumgebung), Dateien echt.
#[derive(Default)]
struct Secrets(BTreeMap<String, String>);

impl Secrets {
    fn set(&mut self, k: &str, v: &str) {
        self.0.insert(k.to_owned(), v.to_owned());
    }
}

impl SecretSource for Secrets {
    fn env(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
    fn file(&self, path: &str) -> std::io::Result<String> {
        std::fs::read_to_string(path)
    }
}

/// Wie die bisherigen Tests: API-Keys verwalten (Standard ist seit F4 `ignore`).
fn opts() -> ApplyOptions {
    ApplyOptions {
        api_keys: ApiKeyMode::Include,
        ..ApplyOptions::default()
    }
}

fn dry(mut o: ApplyOptions) -> ApplyOptions {
    o.dry_run = true;
    o
}

fn pruning(mut o: ApplyOptions) -> ApplyOptions {
    o.prune = true;
    o
}

fn doc_mut<'a>(docs: &'a mut [Document], kind: ResourceKind, name: &str) -> &'a mut Document {
    docs.iter_mut()
        .find(|d| d.kind() == kind && d.name() == name)
        .unwrap_or_else(|| panic!("{kind} {name} fehlt"))
}

fn without(docs: &[Document], kind: ResourceKind, names: &[&str]) -> Vec<Document> {
    docs.iter()
        .filter(|d| !(d.kind() == kind && names.contains(&d.name())))
        .cloned()
        .collect()
}

fn action(plan: &Plan, kind: ResourceKind, name: &str) -> ChangeAction {
    plan.changes
        .iter()
        .find(|c| c.kind == kind && c.name == name)
        .unwrap_or_else(|| panic!("{kind} {name} nicht im Plan"))
        .action
}

async fn credential_value(store: &Store, ring: &Keyring, name: &str) -> String {
    let all = store.decrypt_credentials(ring).await.unwrap();
    all[name].1.get("apiKey").unwrap().expose().to_owned()
}

async fn run(store: Store) {
    store.migrate().await.unwrap();
    let (master, _) = MasterKey::generate().unwrap();
    let ring = Keyring::new(master);
    let ci_key = generate_api_key().unwrap().secret;
    let mut env = Secrets::default();
    env.set("AIGW_SECRET_OPENAI_PROD_API_KEY", "sk-openai-1");
    env.set("AIGW_SECRET_ANTHROPIC_PROD_API_KEY", "sk-ant-1");
    env.set("AIGW_SECRET_CI_KEY", &ci_key);

    let docs = parse_documents(EXAMPLE).unwrap();
    assert_eq!(docs.len(), 14);
    let v0 = store.config_version().await.unwrap();

    // --- Trockenlauf: alles anlegen, nichts geschrieben ----------------------------------
    let r = apply(&store, &ring, &docs, &dry(opts()), &env)
        .await
        .unwrap();
    assert!(!r.applied);
    assert_eq!(r.plan.summary.create, 14, "{}", r.plan.to_text(true));
    assert_eq!(store.config_version().await.unwrap(), v0);
    assert!(store.list_credentials().await.unwrap().is_empty());

    // --- Anwenden: eine Version, ein Audit-Eintrag je Ressource mit Akteur gitops ----------
    let r = apply(&store, &ring, &docs, &opts(), &env).await.unwrap();
    assert!(r.applied);
    assert_eq!(r.config_version, v0 + 1);
    assert_eq!(store.config_version().await.unwrap(), v0 + 1);
    let audit = store.audit_log(100).await.unwrap();
    assert_eq!(audit.len(), 14);
    assert!(
        audit
            .iter()
            .all(|a| a.actor == "gitops" && a.config_version == v0 + 1 && a.action == "create")
    );
    let snap = load_snapshot(&store, &ring).await.unwrap();
    assert!(
        snap.key(&hash_api_key(&ci_key)).is_some(),
        "Key mit vorgegebenem Klartext"
    );
    assert!(snap.model_group("gpt-4o-latest").is_some());
    assert_eq!(snap.plugin_bindings.len(), 1);
    assert_eq!(snap.mcp_grants.len(), 1);
    assert_eq!(
        credential_value(&store, &ring, "openai-prod").await,
        "sk-openai-1"
    );
    // Geheimnisse nie im Audit-Log.
    let audit_json = serde_json::to_string(&audit).unwrap();
    assert!(!audit_json.contains("sk-openai-1") && !audit_json.contains(&ci_key));

    // --- Erneut anwenden: nichts zu tun, keine neue Version -------------------------------
    let r = apply(&store, &ring, &docs, &opts(), &env).await.unwrap();
    assert!(
        !r.applied && !r.plan.has_changes(),
        "{}",
        r.plan.to_text(true)
    );
    assert_eq!(r.plan.summary.unchanged, 14);
    assert_eq!(store.config_version().await.unwrap(), v0 + 1);

    // --- Rundreise: export → apply → export identisch -----------------------------------
    let exported = export_yaml(&store, ApiKeyMode::Include).await.unwrap();
    assert!(exported.contains("AIGW_SECRET_OPENAI_PROD_API_KEY"));
    assert!(!exported.contains("sk-openai-1") && !exported.contains(&ci_key));
    assert!(!exported.contains("createdAt") && !exported.contains("prefix"));
    assert!(!exported.contains("id:"), "keine IDs im Export");
    let re = parse_documents(&exported).unwrap();
    assert_eq!(re.len(), 14);
    let r = apply(&store, &ring, &re, &pruning(opts()), &env)
        .await
        .unwrap();
    assert!(!r.plan.has_changes(), "{}", r.plan.to_text(true));
    assert_eq!(
        export_yaml(&store, ApiKeyMode::Include).await.unwrap(),
        exported
    );
    let without_keys = export_yaml(&store, ApiKeyMode::Ignore).await.unwrap();
    assert!(!without_keys.contains("kind: ApiKey"));

    // --- Diff-Fälle --------------------------------------------------------------------
    let mut changed = docs.clone();
    if let ResourceSpec::Team(t) = &mut doc_mut(&mut changed, ResourceKind::Team, "platform")
        .manifest
        .spec
    {
        t.budget.as_mut().unwrap().amount = "600".parse::<Amount>().unwrap();
    }
    changed.push(Document {
        index: 99,
        manifest: aigw_core::resource::Manifest {
            metadata: ObjectMeta::named("mistral-large"),
            spec: ResourceSpec::ModelGroup(ModelGroupSpec::default()),
        },
        api_key_secret: None,
    });
    let changed = without(
        &changed,
        ResourceKind::Deployment,
        &["claude-sonnet-anthropic"],
    );
    let mut env2 = Secrets(env.0.clone());
    env2.set("AIGW_SECRET_OPENAI_PROD_API_KEY", "sk-openai-2");

    let p = plan(&store, &ring, &changed, &opts(), &env2).await.unwrap();
    assert_eq!(
        action(&p, ResourceKind::Team, "platform"),
        ChangeAction::Update
    );
    let team = p.changes.iter().find(|c| c.name == "platform").unwrap();
    assert_eq!(team.fields.len(), 1);
    assert_eq!(team.fields[0].path, "spec.budget.amount");
    assert_eq!(
        action(&p, ResourceKind::ModelGroup, "mistral-large"),
        ChangeAction::Create
    );
    assert_eq!(
        action(&p, ResourceKind::Deployment, "claude-sonnet-anthropic"),
        ChangeAction::Retain
    );
    let cred = p.changes.iter().find(|c| c.name == "openai-prod").unwrap();
    assert_eq!(cred.action, ChangeAction::Update);
    assert!(cred.fields[0].secret && cred.fields[0].path == "spec.data.apiKey");
    assert_eq!(
        action(&p, ResourceKind::ModelGroup, "gpt-4o"),
        ChangeAction::Unchanged
    );
    let json = serde_json::to_string(&p).unwrap();
    assert!(!json.contains("sk-openai-2"), "Geheimnis nie im Plan");
    let text = p.to_text(false);
    assert!(text.contains("~ Team \"platform\" ändern"), "{text}");
    assert!(
        text.contains("spec.budget.amount: \"500\" → \"600\""),
        "{text}"
    );
    assert!(text.contains("(Geheimnis geändert)"), "{text}");

    let p = plan(&store, &ring, &changed, &pruning(opts()), &env2)
        .await
        .unwrap();
    assert_eq!(
        action(&p, ResourceKind::Deployment, "claude-sonnet-anthropic"),
        ChangeAction::Delete
    );
    assert_eq!(p.summary.delete, 1);

    // --- Prune anwenden: Deployment weg, Rest geändert, eine Version ---------------------
    let v = store.config_version().await.unwrap();
    let r = apply(&store, &ring, &changed, &pruning(opts()), &env2)
        .await
        .unwrap();
    assert!(r.applied);
    assert_eq!(r.config_version, v + 1);
    assert_eq!(
        (
            r.plan.summary.create,
            r.plan.summary.update,
            r.plan.summary.delete
        ),
        (1, 2, 1)
    );
    assert_eq!(
        credential_value(&store, &ring, "openai-prod").await,
        "sk-openai-2"
    );
    assert!(
        store
            .get::<aigw_core::resource::DeploymentSpec>("claude-sonnet-anthropic")
            .await
            .is_err()
    );
    let audit = store.audit_log(10).await.unwrap();
    assert_eq!(
        audit.iter().filter(|a| a.config_version == v + 1).count(),
        4
    );

    // --- Prune, der noch verwendete Ressourcen löschen würde → nichts angewendet ---------
    let dangling = without(&changed, ResourceKind::ModelGroup, &["claude-sonnet"]);
    let err = apply(&store, &ring, &dangling, &pruning(opts()), &env2)
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("würde durch --prune gelöscht") || msg.contains("claude-sonnet"),
        "{msg}"
    );
    assert!(matches!(err, GitopsError::Invalid(_)));
    assert_eq!(store.config_version().await.unwrap(), v + 1);

    // --- Alias wandert zwischen Gruppen, Gruppe wird gelöscht – in einer Transaktion -----
    let mut moved = changed.clone();
    if let ResourceSpec::ModelGroup(g) =
        &mut doc_mut(&mut moved, ResourceKind::ModelGroup, "gpt-4o")
            .manifest
            .spec
    {
        g.aliases.clear();
        g.fallbacks.clear();
    }
    if let ResourceSpec::ModelGroup(g) =
        &mut doc_mut(&mut moved, ResourceKind::ModelGroup, "mistral-large")
            .manifest
            .spec
    {
        g.aliases = vec!["gpt-4o-latest".into(), "claude-sonnet-latest".into()];
    }
    if let ResourceSpec::Team(t) = &mut doc_mut(&mut moved, ResourceKind::Team, "platform")
        .manifest
        .spec
    {
        t.allowed_models = vec!["gpt-4o".into()];
    }
    let moved = without(&moved, ResourceKind::ModelGroup, &["claude-sonnet"]);
    let r = apply(&store, &ring, &moved, &pruning(opts()), &env2)
        .await
        .unwrap();
    assert!(r.applied, "{}", r.plan.to_text(true));
    let snap = load_snapshot(&store, &ring).await.unwrap();
    assert_eq!(
        snap.model_group("gpt-4o-latest")
            .unwrap()
            .group
            .metadata
            .name,
        "mistral-large"
    );
    assert!(snap.model_group("claude-sonnet").is_none());

    // --- Fehler in einem Dokument → nichts angewendet -------------------------------------
    let v = store.config_version().await.unwrap();
    let bad = format!(
        "{}\n---\napiVersion: aigw/v1alpha1\nkind: Organization\nmetadata: {{name: neu}}\n---\n\
         apiVersion: aigw/v1alpha1\nkind: Team\nmetadata: {{name: kaputt}}\nspec: {{organization: gibt-es-nicht}}\n",
        EXAMPLE
    );
    let bad_docs = parse_documents(&bad).unwrap();
    let err = apply(&store, &ring, &bad_docs, &opts(), &env)
        .await
        .unwrap_err();
    let issue = err
        .issues()
        .iter()
        .find(|i| i.message.contains("gibt-es-nicht"))
        .unwrap_or_else(|| panic!("{err}"));
    assert_eq!(issue.document, Some(16), "{err}");
    assert_eq!(store.config_version().await.unwrap(), v);
    assert!(store.get::<OrganizationSpec>("neu").await.is_err());

    // Fehlende Umgebungsvariable → nichts angewendet.
    let mut no_env = Secrets(env2.0.clone());
    no_env.0.remove("AIGW_SECRET_ANTHROPIC_PROD_API_KEY");
    let err = apply(&store, &ring, &moved, &opts(), &no_env)
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("AIGW_SECRET_ANTHROPIC_PROD_API_KEY"),
        "{err}"
    );

    // --- Geheimnis aus Datei --------------------------------------------------------------
    let dir = std::env::temp_dir().join(format!("aigw-gitops-{}", new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("anthropic");
    std::fs::write(&file, "sk-ant-aus-datei\n").unwrap();
    let mut from_file = moved.clone();
    if let ResourceSpec::ProviderCredential(c) = &mut doc_mut(
        &mut from_file,
        ResourceKind::ProviderCredential,
        "anthropic-prod",
    )
    .manifest
    .spec
    {
        c.data.insert(
            "apiKey".into(),
            SecretRef {
                value: None,
                value_from: Some(ValueFrom::File(file.display().to_string())),
            },
        );
    }
    let r = apply(&store, &ring, &from_file, &opts(), &env2)
        .await
        .unwrap();
    assert_eq!(r.plan.summary.update, 1, "{}", r.plan.to_text(true));
    assert_eq!(
        credential_value(&store, &ring, "anthropic-prod").await,
        "sk-ant-aus-datei"
    );
    // Eingeschränkte Herkunft (Admin-API): Dateien und fremde Variablen verboten.
    let mut restricted = opts();
    restricted.secrets = SecretPolicy::restricted("AIGW_SECRET_");
    let err = plan(&store, &ring, &from_file, &restricted, &env2)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("valueFrom.file"), "{err}");

    // --- API-Keys ------------------------------------------------------------------------
    // Neuer Key ohne secret → übersprungen (Warnung), bestehender ohne secret → unverändert.
    let mut keys = from_file.clone();
    doc_mut(&mut keys, ResourceKind::ApiKey, "ci").api_key_secret = None;
    let mut extra = doc_mut(&mut keys, ResourceKind::ApiKey, "ci").clone();
    extra.manifest.metadata.name = "ci-neu".into();
    keys.push(extra);
    let p = plan(&store, &ring, &keys, &pruning(opts()), &env2)
        .await
        .unwrap();
    assert_eq!(
        action(&p, ResourceKind::ApiKey, "ci"),
        ChangeAction::Unchanged
    );
    assert_eq!(
        action(&p, ResourceKind::ApiKey, "ci-neu"),
        ChangeAction::Skip
    );
    assert!(p.warnings.iter().any(|w| w.contains("ci-neu")));
    // Anderer vorgegebener Klartext → Update (Hash/Präfix), alter Key ungültig.
    let new_ci = generate_api_key().unwrap().secret;
    let mut env3 = Secrets(env2.0.clone());
    env3.set("AIGW_SECRET_CI_KEY", &new_ci);
    let r = apply(&store, &ring, &from_file, &opts(), &env3)
        .await
        .unwrap();
    let ci = r.plan.changes.iter().find(|c| c.name == "ci").unwrap();
    assert_eq!(ci.action, ChangeAction::Update);
    assert_eq!(ci.fields[0].path, "spec.secret");
    let snap = load_snapshot(&store, &ring).await.unwrap();
    assert!(snap.key(&hash_api_key(&new_ci)).is_some());
    assert!(snap.key(&hash_api_key(&ci_key)).is_none());
    // Ungültiges Key-Format.
    env3.set("AIGW_SECRET_CI_KEY", "kein-key");
    assert!(
        plan(&store, &ring, &from_file, &opts(), &env3)
            .await
            .is_err()
    );
    // Modus ignore (Standard seit F4): Keys weder angewendet noch per prune gelöscht.
    assert_eq!(ApplyOptions::default().api_keys, ApiKeyMode::Ignore);
    assert_eq!(ApiKeyMode::default(), ApiKeyMode::Ignore);
    let mut ignore = pruning(opts());
    ignore.api_keys = ApiKeyMode::Ignore;
    let no_keys = without(&from_file, ResourceKind::ApiKey, &["ci"]);
    let p = plan(&store, &ring, &from_file, &ignore, &env3)
        .await
        .unwrap();
    assert_eq!(action(&p, ResourceKind::ApiKey, "ci"), ChangeAction::Skip);
    let p = plan(&store, &ring, &no_keys, &ignore, &env3).await.unwrap();
    assert_eq!(action(&p, ResourceKind::ApiKey, "ci"), ChangeAction::Retain);
    // Modus include + prune ohne Key im Manifest → löschen.
    let p = plan(&store, &ring, &no_keys, &pruning(opts()), &env3)
        .await
        .unwrap();
    assert_eq!(action(&p, ResourceKind::ApiKey, "ci"), ChangeAction::Delete);

    // --- Leeres Manifest mit prune löscht alles (in umgekehrter Reihenfolge) --------------
    let r = apply(&store, &ring, &[], &pruning(opts()), &env3)
        .await
        .unwrap();
    assert!(r.applied, "{}", r.plan.to_text(true));
    assert_eq!(r.plan.summary.delete, 13);
    let exported = export_yaml(&store, ApiKeyMode::Include).await.unwrap();
    assert!(parse_documents(&exported).unwrap().is_empty(), "{exported}");

    let _ = std::fs::remove_dir_all(dir);
    store.close().await;
}

#[tokio::test]
async fn sqlite_in_memory() {
    run(Store::connect("sqlite::memory:").await.unwrap()).await;
}

#[tokio::test]
async fn sqlite_file() {
    let dir = std::env::temp_dir().join(format!("aigw-gitops-test-{}", new_id()));
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
    let db = format!("aigw_gitops_{}", new_id().simple());
    let admin = sqlx::PgPool::connect(&base).await.unwrap();
    sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {db}")))
        .execute(&admin)
        .await
        .unwrap();
    let (prefix, _) = base.rsplit_once('/').expect("URL mit Datenbankpfad");
    let store = Store::connect(&format!("{prefix}/{db}")).await.unwrap();
    run(store).await;
    sqlx::query(AssertSqlSafe(format!("DROP DATABASE {db} WITH (FORCE)")))
        .execute(&admin)
        .await
        .unwrap();
}

/// S-05 (AIGW-90): ohne `manage_users` (Rolle `gitops` über die Admin-API) keine Nutzer mit
/// Rollen bzw. OIDC-Verknüpfung anlegen, ändern oder löschen – auch nicht im Trockenlauf;
/// unveränderte und beibehaltene Nutzer sowie Nutzer ohne Rollen/OIDC bleiben erlaubt.
#[tokio::test]
async fn privileged_users_need_manage_users() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    store.migrate().await.unwrap();
    let ring = Keyring::new(MasterKey::generate().unwrap().0);
    let env = Secrets::default();
    let restricted = ApplyOptions {
        manage_users: false,
        ..ApplyOptions::default()
    };
    let user = |name: &str, extra: &str| {
        format!(
            "apiVersion: aigw/v1alpha1\nkind: User\nmetadata: {{name: {name}}}\nspec:\n  \
             email: {name}@example.org\n{extra}"
        )
    };
    let admin_role = "  roles: [{role: admin}]\n";
    let oidc = "  oidcIssuer: https://idp.example.org\n  oidcSubject: s-1\n";
    let rejected = |r: Result<aigw_store::gitops::ApplyReport, GitopsError>, name: &str| {
        let Err(GitopsError::Invalid(issues)) = r else {
            panic!("{name}: Validierungsfehler erwartet: {r:?}");
        };
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].name.as_deref(), Some(name));
        assert!(
            issues[0].message.contains("gitops.manage_users"),
            "{}",
            issues[0].message
        );
    };

    // Nutzer ohne Rollen/OIDC: erlaubt.
    let plain = parse_documents(&user("plain", "")).unwrap();
    apply(&store, &ring, &plain, &restricted, &env)
        .await
        .unwrap();
    // Anlegen mit Rolle bzw. OIDC: abgelehnt (Trockenlauf ebenso).
    for (name, extra) in [("boss", admin_role), ("linked", oidc)] {
        let docs = parse_documents(&user(name, extra)).unwrap();
        rejected(
            apply(&store, &ring, &docs, &dry(restricted.clone()), &env).await,
            name,
        );
        rejected(apply(&store, &ring, &docs, &restricted, &env).await, name);
    }
    // Bestehenden Nutzer zum Admin machen bzw. verknüpfen: abgelehnt.
    let promote = parse_documents(&user("plain", admin_role)).unwrap();
    rejected(
        apply(&store, &ring, &promote, &restricted, &env).await,
        "plain",
    );
    let link = parse_documents(&user("plain", oidc)).unwrap();
    rejected(
        apply(&store, &ring, &link, &restricted, &env).await,
        "plain",
    );

    // Mit Freigabe (globaler admin bzw. gitops.manage_users): erlaubt.
    let boss = parse_documents(&user("boss", admin_role)).unwrap();
    apply(&store, &ring, &boss, &ApplyOptions::default(), &env)
        .await
        .unwrap();
    // Unverändert erneut anwenden und beibehalten: erlaubt.
    let r = apply(&store, &ring, &boss, &restricted, &env)
        .await
        .unwrap();
    assert_eq!(
        action(&r.plan, ResourceKind::User, "boss"),
        ChangeAction::Unchanged
    );
    apply(&store, &ring, &plain, &restricted, &env)
        .await
        .unwrap();
    // Ändern (auch nur die E-Mail) und Löschen per Prune: abgelehnt.
    let changed =
        parse_documents(&user("boss", admin_role).replace("boss@example.org", "chef@example.org"))
            .unwrap();
    rejected(
        apply(&store, &ring, &changed, &restricted, &env).await,
        "boss",
    );
    rejected(
        apply(&store, &ring, &plain, &pruning(restricted.clone()), &env).await,
        "boss",
    );
    // Rollen entziehen ist ebenfalls eine Änderung eines berechtigten Nutzers.
    let demote = parse_documents(&user("boss", "")).unwrap();
    rejected(
        apply(&store, &ring, &demote, &restricted, &env).await,
        "boss",
    );
    assert_eq!(
        store
            .get::<aigw_core::resource::UserSpec>("boss")
            .await
            .unwrap()
            .spec
            .roles
            .len(),
        1
    );
    store.close().await;
}
