// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! `aigw` – Einstiegspunkt des Gateways.
//!
//! - `aigw serve --config <pfad>` startet Datenebene (`server.data.listen`, Standard `:4000`)
//!   sowie Admin-API und Admin-UI (`server.control.listen`, Standard `:4001`); bei gleicher
//!   Adresse auf einem gemeinsamen Port.
//! - `aigw openapi` gibt die OpenAPI-Spezifikation der Admin-API aus (für `ui/` → TS-Typen).
//! - `aigw config export|diff|apply|validate` – GitOps (siehe [`cli_config`]).

mod cli_config;
mod observe;
mod runtime;
mod server;

use std::io::{IsTerminal as _, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use tracing_subscriber::layer::{Layer as _, SubscriberExt as _};
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, Registry};

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
    /// GitOps: Ressourcen als YAML exportieren, vergleichen, anwenden, prüfen.
    #[command(subcommand)]
    Config(cli_config::ConfigCommand),
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum LogFormat {
    Text,
    Json,
}

/// Richtet das Logging ein und hängt den (noch inaktiven) OpenTelemetry-Layer ein; Traces
/// werden erst nach dem Laden der Konfiguration aktiviert (`observe.otlp`, AIGW-45).
fn init_tracing(format: LogFormat) -> aigw_observe::otel::TraceActivator {
    // Filter über `AIGW_LOG` (Syntax wie `RUST_LOG`), Standard `info`. Die Trace-Spans
    // (`aigw::otel`) gehören nicht ins Log.
    let filter = EnvFilter::try_from_env("AIGW_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let filter = match "aigw::otel=off".parse() {
        Ok(d) => filter.add_directive(d),
        Err(_) => filter,
    };
    let fmt = tracing_subscriber::fmt::layer()
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr);
    let fmt: Box<dyn tracing_subscriber::Layer<Registry> + Send + Sync> = match format {
        LogFormat::Text => Box::new(fmt),
        LogFormat::Json => Box::new(fmt.json()),
    };
    let (otel, activator) = aigw_observe::otel::layer();
    tracing_subscriber::registry()
        .with(fmt.with_filter(filter))
        .with(otel)
        .init();
    activator
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
        Command::Config(cmd) => {
            init_tracing(cli.log_format);
            cli_config::run(cmd)
        }
        Command::Serve { config } => {
            let traces = init_tracing(cli.log_format);
            match serve(config, &traces) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    tracing::error!(error = %e, "Abbruch");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

fn serve(
    config_path: PathBuf,
    traces: &aigw_observe::otel::TraceActivator,
) -> Result<(), Box<dyn std::error::Error>> {
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
        let runtime_state = runtime::Runtime::start(&config, traces).await?;
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
        // Routing aus dem Snapshot (AIGW-24): Neuaufbau bei jeder neuen config_version,
        // Health Checks im Hintergrund; Keys und Modellfreigaben prüft der Snapshot (AIGW-28).
        let routes = std::sync::Arc::new(
            aigw_proxy::SnapshotRoutes::with_defaults()
                .map_err(|e| format!("Upstream-Client: {e}"))?,
        );
        let routes_task = routes.spawn(runtime_state.snapshot.clone());
        let state = aigw_proxy::ProxyState {
            routes,
            auth: std::sync::Arc::new(aigw_proxy::SnapshotAuthenticator::new(
                runtime_state.snapshot.clone(),
            )),
            observer: runtime_state.observer.clone(),
            config: aigw_proxy::ProxyConfig::from(&config.server.data),
        };
        // Steuerebene: Admin-API/-UI (Anmeldung: AIGW-28/32), GitOps (AIGW-33) und
        // /metrics (AIGW-43); Modus „nur YAML“ sperrt die Schreibrouten.
        let auth_cfg = &config.admin.auth;
        let bootstrap = bootstrap_token(auth_cfg.bootstrap, &runtime_state.store).await?;
        let client_secret = if auth_cfg.oidc.enabled() {
            aigw_admin::session::client_secret(&auth_cfg.oidc)?
        } else {
            None
        };
        let admin = aigw_admin::AdminState::from_config(
            runtime_state.store.clone(),
            auth_cfg,
            bootstrap,
            client_secret,
        )
        .map_err(|e| format!("admin.auth: {e}"))?;
        if admin.oidc_enabled() {
            tracing::info!("Admin-API: OIDC-Login aktiv");
        }
        let auth = std::sync::Arc::clone(&admin.auth);
        let upstreams = aigw_admin::UpstreamPolicy::from_config(&config.admin)?;
        let control = aigw_admin::router_with(
            admin
                .with_keyring(std::sync::Arc::clone(&runtime_state.keyring))
                .with_upstream_policy(upstreams.clone()),
        )
        .merge(aigw_admin::gitops::router(
            aigw_admin::gitops::GitopsState {
                store: runtime_state.store.clone(),
                keyring: std::sync::Arc::clone(&runtime_state.keyring),
                auth,
                upstreams,
            },
        ))
        .merge(observe::metrics_router(runtime_state.metrics_sources()));
        let control = aigw_admin::read_only::apply(control, config.gitops.read_only_admin);
        server::run(listeners, state, control, shutdown_signal()).await?;
        drop(routes_task);
        runtime_state.shutdown().await;
        tracing::info!("ai-gw beendet");
        Ok(())
    })
}

/// Umgebungsvariable mit dem Bootstrap-Admin-Token.
const BOOTSTRAP_TOKEN_ENV: &str = "AIGW_BOOTSTRAP_TOKEN";

/// Bootstrap-Admin-Token (§15) aus `AIGW_BOOTSTRAP_TOKEN` oder – falls nicht gesetzt – für
/// diesen Prozess neu erzeugt und einmal geloggt. `None` bei `admin.auth.bootstrap: off` bzw.
/// (`auto`) wenn es nach dem ersten OIDC-Admin/Admin-API-Key abgeschaltet wurde (AIGW-32).
async fn bootstrap_token(
    mode: aigw_core::config::BootstrapMode,
    store: &aigw_store::Store,
) -> Result<Option<aigw_admin::BootstrapToken>, Box<dyn std::error::Error>> {
    use aigw_core::config::BootstrapMode;
    match mode {
        BootstrapMode::Off => {
            tracing::info!("Admin-API: Bootstrap-Token abgeschaltet (admin.auth.bootstrap: off)");
            return Ok(None);
        }
        BootstrapMode::Auto if store.bootstrap_disabled().await? => {
            tracing::info!(
                "Admin-API: Bootstrap-Token abgeschaltet (OIDC-Admin bzw. Admin-API-Key vorhanden)"
            );
            return Ok(None);
        }
        BootstrapMode::Auto | BootstrapMode::On => {}
    }
    match std::env::var(BOOTSTRAP_TOKEN_ENV) {
        Ok(t) if !t.trim().is_empty() => {
            tracing::info!("Admin-API: Bootstrap-Token aus {BOOTSTRAP_TOKEN_ENV}");
            Ok(Some(
                aigw_admin::BootstrapToken::new(&t)
                    .map_err(|e| format!("{BOOTSTRAP_TOKEN_ENV}: {e}"))?,
            ))
        }
        _ => {
            let (token, secret) = aigw_admin::BootstrapToken::generate()?;
            tracing::warn!(
                token = %secret,
                "Admin-API: Bootstrap-Token erzeugt (gilt nur für diesen Prozess; \
                 fest vorgeben über {BOOTSTRAP_TOKEN_ENV})"
            );
            Ok(Some(token))
        }
    }
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
