// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! `aigw` – Einstiegspunkt des Gateways.
//!
//! - `aigw serve --config <pfad>` startet Datenebene (`server.data.listen`, Standard `:4000`)
//!   sowie Admin-API und Admin-UI (`server.control.listen`, Standard `:4001`); bei gleicher
//!   Adresse auf einem gemeinsamen Port.
//! - `aigw openapi` gibt die OpenAPI-Spezifikation der Admin-API aus (für `ui/` → TS-Typen).

mod runtime;
mod server;

use std::io::{IsTerminal as _, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use tracing_subscriber::EnvFilter;

/// AI-Gateway (Gateway und Admin-API in einem Binary).
#[derive(Debug, Parser)]
#[command(name = "aigw", version, about)]
struct Cli {
    /// Log-Format.
    #[arg(long, global = true, value_enum, default_value_t = LogFormat::Text, env = "AIGW_LOG_FORMAT")]
    log_format: LogFormat,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Startet den Server.
    Serve {
        /// Pfad zur Konfigurationsdatei (YAML).
        #[arg(short, long, env = "AIGW_CONFIG")]
        config: PathBuf,
    },
    /// Gibt die OpenAPI-Spezifikation der Admin-API auf stdout aus.
    Openapi,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum LogFormat {
    Text,
    Json,
}

fn init_tracing(format: LogFormat) {
    // Filter über `AIGW_LOG` (Syntax wie `RUST_LOG`), Standard `info`.
    let filter = EnvFilter::try_from_env("AIGW_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr);
    match format {
        LogFormat::Text => builder.init(),
        LogFormat::Json => builder.json().init(),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Openapi => match aigw_admin::openapi_json() {
            Ok(json) => {
                let mut out = std::io::stdout().lock();
                if writeln!(out, "{json}").is_err() {
                    return ExitCode::FAILURE;
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                let _ = writeln!(
                    std::io::stderr(),
                    "OpenAPI-Serialisierung fehlgeschlagen: {e}"
                );
                ExitCode::FAILURE
            }
        },
        Command::Serve { config } => {
            init_tracing(cli.log_format);
            match serve(config) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    tracing::error!(error = %e, "Abbruch");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

fn serve(config_path: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    // rustls: aws-lc-rs als einziger Krypto-Provider (kein ring, kein OpenSSL).
    if rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .is_err()
    {
        tracing::debug!("rustls-Krypto-Provider war bereits gesetzt");
    }

    let config = aigw_core::Config::load(&config_path)?;
    tracing::info!(config = %config_path.display(), "Konfiguration geladen");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let runtime_state = runtime::Runtime::start(&config).await?;
        let listeners = server::Listeners::bind(&config.server).await?;
        let (data, control) = listeners.local_addrs()?;
        tracing::info!(
            data = %data,
            control = %control,
            combined = config.server.is_combined(),
            version = aigw_core::VERSION,
            ui = aigw_admin::ui::KIND,
            "ai-gw gestartet"
        );
        // Bis die Anbieter-Adapter (AIGW-15) und die Key-Prüfung (AIGW-28) da sind, bedient
        // der eingebaute Mock-Anbieter die Datenebene.
        tracing::warn!("Datenebene nutzt den Mock-Anbieter (echo), keine Schlüsselprüfung");
        let state = aigw_proxy::ProxyState {
            config: aigw_proxy::ProxyConfig::from(&config.server.data),
            ..aigw_proxy::ProxyState::mock()
        };
        server::run(listeners, state, shutdown_signal()).await?;
        runtime_state.shutdown().await;
        tracing::info!("ai-gw beendet");
        Ok(())
    })
}

/// Wartet auf SIGINT (Strg+C) oder SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = term => {},
    }
    tracing::info!("Beenden angefordert");
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_consistent_and_parses_serve() {
        Cli::command().debug_assert();
        let cli = Cli::try_parse_from(["aigw", "serve", "--config", "x.toml"]).unwrap();
        assert!(matches!(cli.command, Command::Serve { config } if config.as_os_str() == "x.toml"));
    }
}
