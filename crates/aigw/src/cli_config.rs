// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! `aigw config export|diff|apply|validate` – GitOps über die Kommandozeile (AIGW-33).
//!
//! Die Befehle arbeiten direkt auf der Datenbank aus der Startkonfiguration (`--config`, wie
//! `aigw serve`); `diff`/`apply` brauchen zusätzlich den Master-Key (Zugangsdaten vergleichen
//! und verschlüsseln). Laufende Server übernehmen Änderungen über den Reloader (Postgres
//! `NOTIFY` sofort, SQLite per Versionsabfrage `store.reload_poll_secs`).
//!
//! Exit-Codes: 0 = Erfolg, 1 = Fehler (Manifest ungültig, Datenbank …), 2 = `diff --exit-code`
//! mit ausstehenden Änderungen.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use aigw_core::Config;
use aigw_store::gitops::{
    self, ApiKeyMode, ApplyOptions, GITOPS_ACTOR, GitopsError, Plan, ProcessSecrets,
};
use aigw_store::{Keyring, Store};
use clap::{Args, Subcommand, ValueEnum};

type BoxError = Box<dyn std::error::Error>;

/// Ausgabeformat.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum OutputFormat {
    /// Lesbarer Text.
    #[default]
    Text,
    /// JSON (Plan bzw. Ergebnis).
    Json,
}

/// Umgang mit API-Keys.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum KeysArg {
    /// Keys exportieren/anwenden (ohne Klartext; neue Keys nur mit `spec.secret.valueFrom`).
    Include,
    /// Keys weder exportieren noch anwenden noch löschen (Standard, F4).
    #[default]
    Ignore,
}

impl From<KeysArg> for ApiKeyMode {
    fn from(k: KeysArg) -> Self {
        match k {
            KeysArg::Include => Self::Include,
            KeysArg::Ignore => Self::Ignore,
        }
    }
}

/// Gemeinsame Argumente von `diff`, `apply` und `validate`.
#[derive(Debug, Args)]
pub(crate) struct PlanArgs {
    /// Manifest-Datei (YAML, Mehrfachdokument; `-` = Standardeingabe).
    file: PathBuf,
    /// Ressourcen löschen, die nicht im Manifest stehen (ohne: nie löschen).
    #[arg(long)]
    prune: bool,
    /// Umgang mit API-Keys.
    #[arg(long, value_enum, default_value_t)]
    api_keys: KeysArg,
    /// Ausgabeformat.
    #[arg(short, long, value_enum, default_value_t)]
    output: OutputFormat,
    /// Auch unveränderte und nicht verwaltete Ressourcen einzeln zeigen.
    #[arg(short, long)]
    verbose: bool,
}

/// Unterbefehle von `aigw config`.
#[derive(Debug, Subcommand)]
pub(crate) enum ConfigCommand {
    /// Exportiert alle Ressourcen als YAML (Geheimnisse nur als Platzhalter-Verweis).
    Export {
        /// Startkonfiguration (YAML) mit Datenbank.
        #[arg(short, long, env = "AIGW_CONFIG")]
        config: PathBuf,
        /// Zieldatei (Standard: Standardausgabe).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Umgang mit API-Keys.
        #[arg(long, value_enum, default_value_t)]
        api_keys: KeysArg,
    },
    /// Zeigt den Soll/Ist-Vergleich (create/update/delete/unchanged) ohne zu schreiben.
    Diff {
        /// Startkonfiguration (YAML) mit Datenbank und Master-Key.
        #[arg(short, long, env = "AIGW_CONFIG")]
        config: PathBuf,
        #[command(flatten)]
        args: PlanArgs,
        /// Exit-Code 2, wenn es Änderungen gibt (Drift-Erkennung in CI).
        #[arg(long)]
        exit_code: bool,
    },
    /// Wendet Manifeste in einer Transaktion an (alles oder nichts).
    Apply {
        /// Startkonfiguration (YAML) mit Datenbank und Master-Key.
        #[arg(short, long, env = "AIGW_CONFIG")]
        config: PathBuf,
        #[command(flatten)]
        args: PlanArgs,
        /// Nur planen, nichts schreiben.
        #[arg(long)]
        dry_run: bool,
        /// Akteur im Audit-Log.
        #[arg(long, default_value = GITOPS_ACTOR)]
        actor: String,
    },
    /// Prüft die Startkonfiguration (YAML + `AIGW_…__…`-Overrides der Umgebung): Syntax,
    /// unbekannte Felder, Typen, Upstream-Regeln (`admin.*`), Webhook-Geheimnisse. Öffnet
    /// weder Datenbank noch Master-Key.
    Check {
        /// Startkonfiguration (YAML).
        #[arg(short, long, env = "AIGW_CONFIG")]
        config: PathBuf,
    },
    /// Master-Key-Rotation: verschlüsselt alle Zugangsdaten und Secrets, die nicht mit dem
    /// aktuellen Key (`secrets.master_key_file` bzw. `AIGW_MASTER_KEY`) verschlüsselt sind,
    /// neu; die alten Keys müssen dazu in `secrets.previous_key_files` stehen.
    Rekey {
        /// Startkonfiguration (YAML) mit Datenbank und Master-Keys.
        #[arg(short, long, env = "AIGW_CONFIG")]
        config: PathBuf,
        /// Akteur im Audit-Log.
        #[arg(long, default_value = "rekey")]
        actor: String,
    },
    /// Prüft Manifeste: ohne `--config` offline (Syntax, Schema, Namen, Geheimnisregel,
    /// Dubletten), mit `--config` zusätzlich Verweise und Geheimnisse gegen die Datenbank.
    Validate {
        /// Startkonfiguration für die vollständige Prüfung (optional).
        #[arg(short, long)]
        config: Option<PathBuf>,
        #[command(flatten)]
        args: PlanArgs,
    },
}

/// Führt einen `aigw config`-Befehl aus.
pub(crate) fn run(cmd: ConfigCommand) -> ExitCode {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return fail(&e.to_string()),
    };
    match rt.block_on(exec(cmd)) {
        Ok(code) => code,
        Err(e) => fail(&e.to_string()),
    }
}

fn fail(msg: &str) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "Fehler: {msg}");
    ExitCode::FAILURE
}

fn out(text: &str) -> Result<(), BoxError> {
    let mut o = std::io::stdout().lock();
    o.write_all(text.as_bytes())?;
    if !text.ends_with('\n') {
        o.write_all(b"\n")?;
    }
    o.flush()?;
    Ok(())
}

fn read_manifest(path: &Path) -> Result<String, BoxError> {
    if path.as_os_str() == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        Ok(s)
    } else {
        std::fs::read_to_string(path)
            .map_err(|e| format!("Manifest {} nicht lesbar: {e}", path.display()).into())
    }
}

/// Lädt die Startkonfiguration, verbindet und migriert die Datenbank (idempotent, wie
/// `aigw serve`; so funktionieren `diff`/`export` auch auf einer frischen Datenbank).
async fn open(config: &Path) -> Result<(Config, Store), BoxError> {
    let cfg = Config::load(config)?;
    let store = Store::connect_with(&cfg.store).await?;
    store.migrate().await?;
    Ok((cfg, store))
}

fn options(
    cfg: &Config,
    args: &PlanArgs,
    dry_run: bool,
    actor: &str,
) -> Result<ApplyOptions, BoxError> {
    Ok(ApplyOptions {
        dry_run,
        prune: args.prune,
        api_keys: args.api_keys.into(),
        actor: actor.to_owned(),
        // Gleiche Endpunktregeln wie die Admin-API (`admin.*` der Startkonfiguration).
        upstreams: aigw_core::upstream::UpstreamPolicy::from_config(&cfg.admin)?
            .with_ca_dir(cfg.upstream.ca_dir.clone()),
        group_settings: Some(aigw_admin::resources::group_settings),
        // Plugin-Bindungen mit der Registry der Plugin-Kette (F118/F165).
        plugin_bindings: Some(aigw_admin::plugin_bindings::check),
        ..ApplyOptions::default()
    })
}

fn print_plan(plan: &Plan, args: &PlanArgs) -> Result<(), BoxError> {
    match args.output {
        OutputFormat::Text => out(&plan.to_text(args.verbose)),
        OutputFormat::Json => out(&serde_json::to_string_pretty(plan)?),
    }
}

/// Validierungsfehler als Liste auf stderr, sonst normaler Fehler.
fn report(e: GitopsError) -> Result<ExitCode, BoxError> {
    match e {
        GitopsError::Invalid(issues) => {
            let mut err = std::io::stderr().lock();
            writeln!(
                err,
                "Manifest ungültig ({} Fehler), nichts angewendet:",
                issues.len()
            )?;
            for i in issues {
                writeln!(err, "  - {i}")?;
            }
            Ok(ExitCode::FAILURE)
        }
        other => Err(other.into()),
    }
}

/// Prüfungen von `aigw config check` (ohne Datenbank und Master-Key).
fn check_start_config(path: &Path) -> Result<Config, BoxError> {
    let cfg = Config::load(path)?;
    check_config(&cfg)?;
    Ok(cfg)
}

fn check_config(cfg: &Config) -> Result<(), BoxError> {
    aigw_core::upstream::UpstreamPolicy::from_config(&cfg.admin)
        .map_err(|e| format!("admin: {e}"))?;
    for w in &cfg.events.webhooks {
        if let Some(secret) = &w.secret {
            secret
                .validate()
                .map_err(|e| format!("events.webhooks[{}]: {e}", w.name))?;
        }
    }
    Ok(())
}

async fn exec(cmd: ConfigCommand) -> Result<ExitCode, BoxError> {
    match cmd {
        ConfigCommand::Rekey { config, actor } => {
            let (cfg, store) = open(&config).await?;
            let keyring = Keyring::from_config(&cfg.secrets)?;
            let res = store.rotate_credentials(&actor, &keyring).await;
            store.close().await;
            let n = res?;
            out(&format!(
                "Neu verschlüsselt: {n} Datensatz/Datensätze (Key-ID {})",
                keyring.primary_id()
            ))?;
            Ok(ExitCode::SUCCESS)
        }
        ConfigCommand::Check { config } => {
            let cfg = check_start_config(&config)?;
            out(&format!(
                "OK: Startkonfiguration {} gültig (Datenbank: {}, Zähler: {:?}, Daten {} / Steuerung {})",
                config.display(),
                cfg.store.url.split(':').next().unwrap_or_default(),
                cfg.counters.backend,
                cfg.server.data.listen,
                cfg.server.control.listen,
            ))?;
            Ok(ExitCode::SUCCESS)
        }
        ConfigCommand::Export {
            config,
            out: target,
            api_keys,
        } => {
            let (_, store) = open(&config).await?;
            let yaml = gitops::export_yaml(&store, api_keys.into()).await?;
            store.close().await;
            match target {
                Some(p) => std::fs::write(&p, yaml)
                    .map_err(|e| format!("{} nicht schreibbar: {e}", p.display()))?,
                None => out(&yaml)?,
            }
            Ok(ExitCode::SUCCESS)
        }
        ConfigCommand::Diff {
            config,
            args,
            exit_code,
        } => {
            let docs = match gitops::parse_documents(&read_manifest(&args.file)?) {
                Ok(d) => d,
                Err(e) => return report(e),
            };
            let (cfg, store) = open(&config).await?;
            let keyring = Keyring::from_config(&cfg.secrets)?;
            let opts = options(&cfg, &args, true, GITOPS_ACTOR)?;
            let res = gitops::plan(&store, &keyring, &docs, &opts, &ProcessSecrets).await;
            store.close().await;
            let plan = match res {
                Ok(p) => p,
                Err(e) => return report(e),
            };
            print_plan(&plan, &args)?;
            Ok(if exit_code && plan.has_changes() {
                ExitCode::from(2)
            } else {
                ExitCode::SUCCESS
            })
        }
        ConfigCommand::Apply {
            config,
            args,
            dry_run,
            actor,
        } => {
            let docs = match gitops::parse_documents(&read_manifest(&args.file)?) {
                Ok(d) => d,
                Err(e) => return report(e),
            };
            let (cfg, store) = open(&config).await?;
            let keyring = Keyring::from_config(&cfg.secrets)?;
            let opts = options(&cfg, &args, dry_run, &actor)?;
            let res = gitops::apply(&store, &keyring, &docs, &opts, &ProcessSecrets).await;
            store.close().await;
            let rep = match res {
                Ok(r) => r,
                Err(e) => return report(e),
            };
            match args.output {
                OutputFormat::Json => out(&serde_json::to_string_pretty(&rep)?)?,
                OutputFormat::Text => {
                    let mut text = rep.plan.to_text(args.verbose);
                    text.push_str(&if rep.applied {
                        format!("Angewendet: config_version {}\n", rep.config_version)
                    } else if dry_run {
                        "Trockenlauf – nichts geschrieben.\n".to_owned()
                    } else {
                        "Keine Änderungen.\n".to_owned()
                    });
                    out(&text)?;
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        ConfigCommand::Validate { config, args } => {
            let docs = match gitops::parse_documents(&read_manifest(&args.file)?) {
                Ok(d) => d,
                Err(e) => return report(e),
            };
            let mut msg = format!("OK: {} Dokument(e) gültig", docs.len());
            if let Some(config) = config {
                let (cfg, store) = open(&config).await?;
                let keyring = Keyring::from_config(&cfg.secrets)?;
                let opts = options(&cfg, &args, true, GITOPS_ACTOR)?;
                let res = gitops::plan(&store, &keyring, &docs, &opts, &ProcessSecrets).await;
                store.close().await;
                if let Err(e) = res {
                    return report(e);
                }
                msg.push_str(" (inkl. Verweise und Geheimnisse gegen die Datenbank)");
            } else {
                msg.push_str(" (offline; Verweise/Geheimnisse prüft `--config`)");
            }
            match args.output {
                OutputFormat::Text => out(&msg)?,
                OutputFormat::Json => out(&serde_json::to_string_pretty(&serde_json::json!({
                    "valid": true,
                    "documents": docs.len(),
                }))?)?,
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    #[derive(Debug, clap::Parser)]
    struct T {
        #[command(subcommand)]
        cmd: ConfigCommand,
    }

    #[test]
    fn parses_subcommands() {
        let t = T::try_parse_from([
            "t",
            "apply",
            "-c",
            "aigw.yaml",
            "m.yaml",
            "--prune",
            "--dry-run",
            "-o",
            "json",
        ])
        .unwrap();
        let ConfigCommand::Apply {
            args,
            dry_run,
            actor,
            ..
        } = t.cmd
        else {
            panic!("apply erwartet")
        };
        assert!(args.prune && dry_run && args.output == OutputFormat::Json);
        assert_eq!(actor, "gitops");
        let t = T::try_parse_from(["t", "validate", "m.yaml", "--api-keys", "include"]).unwrap();
        assert!(matches!(
            t.cmd,
            ConfigCommand::Validate { config: None, args } if args.api_keys == KeysArg::Include
        ));
        // F4: ohne Angabe werden API-Keys ignoriert (export, diff, apply, validate).
        let t = T::try_parse_from(["t", "validate", "m.yaml"]).unwrap();
        assert!(matches!(
            t.cmd,
            ConfigCommand::Validate { config: None, args } if args.api_keys == KeysArg::Ignore
        ));
        let t = T::try_parse_from(["t", "export", "-c", "x.yaml"]).unwrap();
        assert!(matches!(
            t.cmd,
            ConfigCommand::Export {
                api_keys: KeysArg::Ignore,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn validate_example_offline() {
        let example = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../config/examples/gitops.yaml"
        );
        let code = exec(ConfigCommand::Validate {
            config: None,
            args: PlanArgs {
                file: example.into(),
                prune: false,
                api_keys: KeysArg::Include,
                output: OutputFormat::Json,
                verbose: false,
            },
        })
        .await
        .unwrap();
        assert_eq!(code, ExitCode::SUCCESS);
    }
    /// Geheimnisquelle für die Beispielprüfung: jede Variable/Datei liefert einen Platzhalter.
    struct DummySecrets;

    impl gitops::SecretSource for DummySecrets {
        fn env(&self, name: &str) -> Option<String> {
            Some(dummy_secret(name))
        }
        fn file(&self, path: &str) -> std::io::Result<String> {
            Ok(dummy_secret(path))
        }
    }

    fn dummy_secret(name: &str) -> String {
        // Vertex-Schlüsseldatei als JSON, sonst ein beliebiger Wert.
        if name.contains("SERVICE_ACCOUNT") || name.ends_with(".json") {
            serde_json::json!({
                "type": "service_account",
                "project_id": "example",
                "client_email": "aigw@example.iam.gserviceaccount.com",
                "private_key": "-----BEGIN PRIVATE KEY-----\nMA==\n-----END PRIVATE KEY-----\n",
            })
            .to_string()
        } else {
            // Gültiges Key-Format, damit auch ApiKey.spec.secret geprüft werden kann.
            aigw_store::generate_api_key().unwrap().secret
        }
    }

    /// AIGW-59: Alle Beispiele unter `config/examples/` müssen gültig bleiben – jede
    /// Startkonfiguration (`*/aigw.yaml`) wie `aigw config check`, jedes Manifest
    /// (`*/resources*.yaml`, `gitops.yaml`) per Apply in eine frische SQLite-Datenbank mit den
    /// Upstream-Regeln der zugehörigen Startkonfiguration und anschließendem Snapshot-Aufbau.
    #[tokio::test]
    async fn config_examples_are_valid() {
        let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../config"));
        let mut checked = Vec::new();
        // Container-Standardkonfiguration und kommentiertes Vollbeispiel.
        for f in ["aigw.container.yaml", "aigw.example.yaml"] {
            let cfg = Config::from_yaml(&std::fs::read_to_string(root.join(f)).unwrap())
                .unwrap_or_else(|e| panic!("{f}: {e}"));
            check_config(&cfg).unwrap_or_else(|e| panic!("{f}: {e}"));
            checked.push(f.to_owned());
        }
        let examples = root.join("examples");
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&examples)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        // gitops.yaml (oberste Ebene) mit der Standard-Startkonfiguration.
        let mut sets: Vec<(String, Config, Vec<PathBuf>)> = vec![(
            "gitops.yaml".into(),
            Config::default(),
            vec![examples.join("gitops.yaml")],
        )];
        for dir in dirs {
            let name = dir.file_name().unwrap().to_string_lossy().into_owned();
            let start = dir.join("aigw.yaml");
            let cfg = Config::from_yaml(&std::fs::read_to_string(&start).unwrap())
                .unwrap_or_else(|e| panic!("{name}/aigw.yaml: {e}"));
            check_config(&cfg).unwrap_or_else(|e| panic!("{name}/aigw.yaml: {e}"));
            checked.push(format!("{name}/aigw.yaml"));
            let mut manifests: Vec<PathBuf> = std::fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().path())
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("resources"))
                })
                .collect();
            manifests.sort();
            sets.push((name, cfg, manifests));
        }
        for (name, cfg, manifests) in sets {
            let store = Store::connect("sqlite::memory:").await.unwrap();
            store.migrate().await.unwrap();
            let keyring = Keyring::new(aigw_store::MasterKey::from_bytes(&[7u8; 32]).unwrap());
            let args = PlanArgs {
                file: PathBuf::new(),
                prune: false,
                api_keys: KeysArg::Include,
                output: OutputFormat::Text,
                verbose: false,
            };
            let opts = options(&cfg, &args, false, GITOPS_ACTOR).unwrap();
            for m in manifests {
                let label = format!("{name}/{}", m.file_name().unwrap().to_string_lossy());
                let docs = gitops::parse_documents(&std::fs::read_to_string(&m).unwrap())
                    .unwrap_or_else(|e| panic!("{label}: {e}"));
                let rep = gitops::apply(&store, &keyring, &docs, &opts, &DummySecrets)
                    .await
                    .unwrap_or_else(|e| panic!("{label}: {e}"));
                assert!(rep.applied, "{label}: nichts angewendet");
                checked.push(label);
            }
            aigw_store::load_snapshot(&store, &keyring)
                .await
                .unwrap_or_else(|e| panic!("{name}: Snapshot: {e}"));
            store.close().await;
        }
        assert!(checked.len() >= 10, "zu wenige Beispiele: {checked:?}");
    }
}
