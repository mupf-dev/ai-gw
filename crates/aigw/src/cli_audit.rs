// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! `aigw audit …` (F116): Hash-Kette des Audit-Logs prüfen, Aufbewahrung als Hintergrundlauf.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use aigw_core::Config;
use aigw_core::config::AuditConfig;
use aigw_store::Store;
use clap::Subcommand;

type BoxError = Box<dyn std::error::Error>;

/// Befehle für das Audit-Log.
#[derive(Debug, Subcommand)]
pub(crate) enum AuditCommand {
    /// Prüft die Hash-Kette des Audit-Logs (Exit-Code 1 bei Abweichungen).
    Verify {
        /// Pfad zur Konfigurationsdatei (YAML).
        #[arg(short, long, env = "AIGW_CONFIG")]
        config: PathBuf,
        /// Bericht als JSON statt Text.
        #[arg(long)]
        json: bool,
        /// Erwarteter Hash des Kettenkopfs (extern gesicherte Kopie); Abweichung = Fehler.
        #[arg(long)]
        expect_head: Option<String>,
    },
}

/// Führt einen `aigw audit`-Befehl aus.
pub(crate) fn run(cmd: AuditCommand) -> ExitCode {
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

async fn exec(cmd: AuditCommand) -> Result<ExitCode, BoxError> {
    match cmd {
        AuditCommand::Verify {
            config,
            json,
            expect_head,
        } => {
            let cfg = Config::load(&config)?;
            let store = Store::connect_with(&cfg.store).await?;
            store.migrate().await?;
            let report = store.verify_audit_chain().await?;
            store.close().await;
            let head_ok = expect_head
                .as_deref()
                .is_none_or(|h| h.eq_ignore_ascii_case(&report.head_hash));
            let mut out = std::io::stdout().lock();
            if json {
                writeln!(out, "{}", serde_json::to_string_pretty(&report)?)?;
            } else {
                writeln!(
                    out,
                    "Audit-Log: {} Einträge geprüft (Nummern {}–{}), {} unverkettet (vor der Hash-Kette)",
                    report.checked,
                    report.anchor_seq + 1,
                    report.head_seq,
                    report.unchained
                )?;
                writeln!(out, "Kettenkopf: {}", report.head_hash)?;
                for i in &report.issues {
                    match i.id {
                        Some(id) => writeln!(out, "  FEHLER #{} ({id}): {}", i.seq, i.problem)?,
                        None => writeln!(out, "  FEHLER #{}: {}", i.seq, i.problem)?,
                    }
                }
                if !head_ok {
                    writeln!(
                        out,
                        "  FEHLER: Kettenkopf weicht vom erwarteten Wert ab (Kette neu berechnet?)"
                    )?;
                }
                writeln!(
                    out,
                    "{}",
                    if report.ok && head_ok {
                        "OK: Kette unversehrt"
                    } else {
                        "NICHT OK: Kette verletzt"
                    }
                )?;
            }
            Ok(if report.ok && head_ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
    }
}

/// Aufbewahrung im Hintergrund (`audit.retention_days > 0`); `None` ohne Frist.
pub(crate) fn spawn_retention(
    store: &Store,
    cfg: &AuditConfig,
) -> Option<tokio::task::JoinHandle<()>> {
    if cfg.retention_days == 0 {
        return None;
    }
    let store = store.clone();
    let keep = time::Duration::days(i64::from(cfg.retention_days));
    let every = Duration::from_secs(cfg.sweep_interval_secs.max(60));
    Some(tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        loop {
            tick.tick().await;
            let cutoff = time::OffsetDateTime::now_utc() - keep;
            match store.prune_audit_log(cutoff).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(deleted = n, "Audit-Log: alte Einträge gelöscht"),
                Err(e) => tracing::warn!(error = %e, "Audit-Log: Aufbewahrung fehlgeschlagen"),
            }
        }
    }))
}
