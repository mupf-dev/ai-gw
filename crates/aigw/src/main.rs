// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! `aigw` – Einstiegspunkt des Gateways.
//!
//! - `aigw serve --config <pfad>` startet Datenebene (`server.data.listen`, Standard `:4000`)
//!   sowie Admin-API und Admin-UI (`server.control.listen`, Standard `:4001`); bei gleicher
//!   Adresse auf einem gemeinsamen Port.
//! - `aigw openapi` gibt die OpenAPI-Spezifikation der Admin-API aus (für `ui/` → TS-Typen).
//! - `aigw config export|diff|apply|validate` – GitOps (siehe [`cli_config`]);
//!   `aigw config check` prüft die Startkonfiguration.
//! - `aigw healthcheck` – Bereitschaftsprüfung für Container (siehe [`healthcheck`]).

/// Globaler Allokator (AIGW-57): mimalloc statt glibc-malloc – weniger CPU je Anfrage und
/// keine Arena-Sperren unter Last (Messung: docs/betrieb/leistung.md).
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod cli_audit;
mod cli_config;
mod healthcheck;
mod keygen;
mod observe;
mod runtime;
mod serve;
mod server;
mod tokenizer;
mod wasm_sign;

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
    /// Erzeugt einen Master-Key (Hex oder Base64) für `secrets.master_key_file` bzw.
    /// `AIGW_MASTER_KEY` – auf die Standardausgabe oder in eine Datei (Rechte 0600).
    Keygen(keygen::KeygenArgs),
    /// Audit-Log: Hash-Kette prüfen (F116).
    #[command(subcommand)]
    Audit(cli_audit::AuditCommand),
    /// WASM-Module signieren (AIGW-101): Schlüsselpaar erzeugen, Modul signieren, Signatur
    /// prüfen.
    #[command(subcommand)]
    Wasm(wasm_sign::WasmCommand),
    /// Bereitschaftsprüfung (`GET /readyz` der Steuerebene) für Container-HEALTHCHECKs;
    /// Exit-Code 0 = bereit.
    Healthcheck(healthcheck::HealthcheckArgs),
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
        Command::Keygen(args) => match keygen::run(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                let _ = writeln!(std::io::stderr(), "aigw keygen: {e}");
                ExitCode::FAILURE
            }
        },
        Command::Healthcheck(args) => healthcheck::run(&args),
        Command::Wasm(cmd) => match wasm_sign::run(&cmd) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                let _ = writeln!(std::io::stderr(), "aigw wasm: {e}");
                ExitCode::FAILURE
            }
        },
        Command::Config(cmd) => {
            init_tracing(cli.log_format);
            cli_config::run(cmd)
        }
        Command::Audit(cmd) => {
            init_tracing(cli.log_format);
            cli_audit::run(cmd)
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
    // Vertrauensanker aller ausgehenden Verbindungen (upstream.roots/ca_file, AIGW-72); vor
    // dem Bau des ersten Clients, Fehler (z. B. unlesbare CA-Datei) brechen den Start ab.
    aigw_outbound::install_default_tls(aigw_outbound::TlsOptions::from_config(&config.upstream))
        .map_err(|e| format!("upstream: {e}"))?;
    // AIGW-90: Entra Workload/Managed Identity (Identität des Prozesses) nur mit Freigabe; vor
    // dem ersten Aufbau einer Token-Quelle.
    aigw_proxy::set_entra_ambient_identity(config.upstream.entra.ambient_identity);
    // AIGW-63: Grenzen der WASM-Plugins und Module aus Dateien vor dem Start prüfen.
    config.wasm.validate()?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        // Rangtabellen o200k/cl100k (Build ohne embedded-tokenizers, AIGW-95).
        tokenizer::init(&config).await?;
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
        entra_ambient_notice(&config, &runtime_state.snapshot.load());
        // Routing aus dem Snapshot (AIGW-24): Neuaufbau bei jeder neuen config_version,
        // Health Checks im Hintergrund; Keys und Modellfreigaben prüft der Snapshot (AIGW-28).
        let routes = std::sync::Arc::new(
            // SSRF-Regeln auch nach der Namensauflösung (AIGW-72).
            aigw_proxy::SnapshotRoutes::with_policy(
                aigw_admin::UpstreamPolicy::from_config(&config.admin)?
                    .with_ca_dir(config.upstream.ca_dir.clone()),
            )
            .map_err(|e| format!("Upstream-Client: {e}"))?
            // `admin.allow_mock: false` lässt Mock-Deployments aus (F85).
            .with_mock(config.admin.allow_mock),
        );
        let routes_task = routes.spawn(runtime_state.snapshot.clone());
        // Breaker-Zustand und offene Anfragen je Deployment in /metrics (F86).
        if let Some(g) = runtime_state.metrics_sources().gateway {
            let r = std::sync::Arc::clone(&routes);
            g.set_deployment_source(move || {
                r.deployment_status()
                    .into_iter()
                    .map(|d| aigw_observe::DeploymentGauge {
                        model_group: d.model_group,
                        deployment: d.deployment,
                        breaker: match d.breaker {
                            aigw_proxy::routing::BreakerState::Closed => 0,
                            aigw_proxy::routing::BreakerState::Open => 1,
                            aigw_proxy::routing::BreakerState::HalfOpen => 2,
                        },
                        in_flight: d.in_flight,
                    })
                    .collect()
            });
        }
        // Plugin-Ketten je (Org, Team, Key, Modellgruppe), Neuaufbau je config_version (AIGW-36).
        // Guardrails mit externen Diensten (AIGW-38): Zugangsdaten aus dem Snapshot, SSRF-Regeln
        // wie für Deployments, Prüfmodelle über das eigene Routing.
        let guard_host = aigw_proxy::GatewayHost::with_defaults(
            routes.clone(),
            std::sync::Arc::clone(&runtime_state.counters),
            Some(runtime_state.snapshot.clone()),
            aigw_admin::UpstreamPolicy::from_config(&config.admin)?,
        )
        .map_err(|e| format!("Plugin-Host: {e}"))?;
        let mut registry =
            aigw_plugins::Registry::with_builtin_host(std::sync::Arc::new(guard_host));
        // Plugin `webhook` (AIGW-61) mit SSRF-Regeln aus `admin.*` und Credential-Referenzen
        // über den Snapshot.
        registry.register(std::sync::Arc::new(
            aigw_plugins::builtin::webhook::Factory::new(
                runtime_state.webhooks.clone(),
                Some(std::sync::Arc::new(runtime_state.snapshot.clone())),
            ),
        ));
        // WASM-Plugins (AIGW-63): eine wasmtime-Laufzeit je Prozess (Feature `wasm`).
        #[cfg(feature = "wasm")]
        let wasm_rt = if config.wasm.enabled {
            Some(aigw_wasm::WasmRuntime::new(&config.wasm).map_err(|e| format!("wasm: {e}"))?)
        } else {
            None
        };
        #[cfg(feature = "wasm")]
        registry.register(std::sync::Arc::new(aigw_wasm::Factory::new(
            wasm_rt.clone(),
        )));
        let plugins = std::sync::Arc::new(aigw_proxy::PluginChains::new(
            registry,
            aigw_plugins::IndexOptions::default(),
        ));
        let plugins_task = plugins.spawn(runtime_state.snapshot.clone());
        let plugin_metrics = plugins.metrics();
        // Antwort-Cache (AIGW-39/40): RAM-LRU, optional Redis, Vektor-Backend; Warmstart des
        // eigenen HNSW aus der Datenbank (verschlüsselt mit dem Master-Key).
        let cache_rt = aigw_cache::start(
            &config,
            Some(aigw_cache::Persistence {
                store: runtime_state.store.clone(),
                keyring: std::sync::Arc::clone(&runtime_state.keyring),
            }),
        )
        .await
        .map_err(|e| format!("cache: {e}"))?;
        // Tool-Inventar (AIGW-87): RAM-Abgleich, gepuffertes Schreiben, Ereignisse
        // `tool.changed`/`tool.suspicious` an die Ereignis-Webhooks.
        let tools = tool_inventory(&config, &runtime_state).await;
        let tools_task = tools.enabled().then(|| {
            tools.spawn(std::time::Duration::from_millis(
                config.tools.flush_interval_ms,
            ))
        });
        let tool_metrics = tools.metrics();
        let state = aigw_proxy::ProxyState {
            routes: routes.clone(),
            auth: std::sync::Arc::new(aigw_proxy::SnapshotAuthenticator::new(
                runtime_state.snapshot.clone(),
            )),
            observer: runtime_state.observer.clone(),
            // Limits, Budgets, Kosten (AIGW-30/31) und Deployment-Kapazität über counters.
            limits: std::sync::Arc::new(aigw_proxy::Limiter::new(
                std::sync::Arc::clone(&runtime_state.counters),
                Some(std::sync::Arc::clone(&runtime_state.budgets)),
                &config.limits,
            )),
            plugins,
            cache: Some(std::sync::Arc::clone(&cache_rt.cache)),
            tools: std::sync::Arc::clone(&tools),
            config: aigw_proxy::ProxyConfig::from(&config.server.data),
        };
        // Semantischer Cache: Embeddings über das interne Routing (AIGW-21/40).
        cache_rt.cache.set_embedder(std::sync::Arc::new(
            aigw_proxy::cache::RoutedEmbedder::for_state(&state),
        ));
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
        // F177: settings.caFile nur aus upstream.ca_dir (Admin-API, GitOps, Routing).
        let upstreams = aigw_admin::UpstreamPolicy::from_config(&config.admin)?
            .with_ca_dir(config.upstream.ca_dir.clone());
        // MCP-Gateway (AIGW-41): `/mcp`, `/mcp/{server}` auf der Datenebene; Zustand der
        // Upstreams über die Admin-API (`/mcp-servers:status`).
        let gateway = if config.mcp.enabled {
            Some(std::sync::Arc::new(
                aigw_proxy::McpGateway::new(
                    &config.mcp,
                    aigw_admin::UpstreamPolicy::from_config(&config.admin)?,
                )
                .map_err(|e| format!("MCP-Gateway: {e}"))?,
            ))
        } else {
            None
        };
        // Preisvorschläge (AIGW-85): Azure Retail Prices + Preiskatalog, SSRF-Regeln aus
        // `admin.*`, fest erlaubte Hosts aus der Startkonfiguration.
        let price_suggester = if config.billing.price_suggestions.enabled {
            let client = aigw_outbound::OutboundClient::new(aigw_outbound::AddressGuard::policy(
                aigw_admin::UpstreamPolicy::from_config(&config.admin)?,
            ))
            .map_err(|e| format!("Preisvorschläge: {e}"))?;
            let s = std::sync::Arc::new(aigw_billing::suggest::Suggester::from_config(
                &config.billing.price_suggestions,
                &client,
            )?);
            let every = config.billing.price_suggestions.check_interval_secs;
            if every > 0 {
                aigw_admin::price_suggestions::spawn_check(
                    runtime_state.store.clone(),
                    std::sync::Arc::clone(&s),
                    std::time::Duration::from_secs(every.max(60)),
                );
            }
            Some(s)
        } else {
            None
        };
        let control = aigw_admin::router_with(
            admin
                .with_price_suggester(price_suggester)
                .with_keyring(std::sync::Arc::clone(&runtime_state.keyring))
                .with_upstream_policy(upstreams.clone())
                .with_wasm_max_module_bytes(config.wasm.max_module_bytes)
                .with_wasm_trust(std::sync::Arc::new(
                    aigw_core::wasm::TrustPolicy::from_config(&config.wasm)?,
                ))
                .with_read_only(config.gitops.read_only_admin)
                .with_ui_mock(config.admin.ui.mock)
                .with_trace_url(config.admin.ui.trace_url.clone()),
        )
        .merge(aigw_admin::gitops::router(
            aigw_admin::gitops::GitopsState {
                store: runtime_state.store.clone(),
                keyring: std::sync::Arc::clone(&runtime_state.keyring),
                auth: std::sync::Arc::clone(&auth),
                upstreams,
                // F52: valueFrom.file über die Admin-API nur innerhalb von gitops.secret_dir.
                secret_dir: config.gitops.secret_dir.clone(),
                manage_users: config.gitops.manage_users,
            },
        ))
        .merge(aigw_admin::usage::router(aigw_admin::usage::UsageState {
            store: runtime_state.store.clone(),
            calendar: std::sync::Arc::clone(&runtime_state.calendar),
            auth: std::sync::Arc::clone(&auth),
            counters: config.counters.backend,
        }))
        .merge(aigw_admin::analytics::router(
            aigw_admin::analytics::AnalyticsState {
                store: runtime_state.store.clone(),
                auth: std::sync::Arc::clone(&auth),
                // Tagesschritte nach billing.timezone (F143).
                calendar: std::sync::Arc::clone(&runtime_state.calendar),
                rollups: config.observe.rollups,
            },
        ))
        .merge(aigw_admin::mcp_status::router(
            aigw_admin::mcp_status::McpStatusState {
                store: runtime_state.store.clone(),
                auth: std::sync::Arc::clone(&auth),
                gateway: gateway.clone(),
            },
        ))
        // Anbieter-Übersicht (AIGW-84): Deployments je Anbieter/Endpunkt mit Laufzeitzustand.
        .merge(aigw_admin::providers::router(
            aigw_admin::providers::ProvidersState {
                store: runtime_state.store.clone(),
                auth: std::sync::Arc::clone(&auth),
                routes: Some(std::sync::Arc::clone(&routes)),
            },
        ))
        // Playground der Admin-UI (AIGW-54): Chat im Namen eines Keys über die Pipeline.
        .merge(aigw_admin::playground::router(
            aigw_admin::playground::PlaygroundState {
                store: runtime_state.store.clone(),
                auth: std::sync::Arc::clone(&auth),
                snapshot: runtime_state.snapshot.clone(),
                proxy: std::sync::Arc::new(aigw_proxy::playground::state_for(&state)),
            },
        ))
        .merge(observe::metrics_router(observe::MetricsSources {
            cache: Some(std::sync::Arc::clone(&cache_rt.cache)),
            plugins: Some(plugin_metrics),
            server: Some(listeners.metrics()),
            tools: Some(tool_metrics),
            #[cfg(feature = "wasm")]
            wasm: wasm_rt.as_ref().map(|r| std::sync::Arc::clone(r.metrics())),
            ..runtime_state.metrics_sources()
        }));
        let control = aigw_admin::read_only::apply(control, config.gitops.read_only_admin);
        let mut data_router = aigw_proxy::router(state.clone());
        let mut mcp_task = None;
        if let Some(gateway) = gateway {
            mcp_task = Some(gateway.spawn(runtime_state.snapshot.clone()));
            data_router = data_router.merge(aigw_proxy::mcp::router(state, gateway));
        }
        // SIGHUP: Exportdateien neu öffnen (externes logrotate, F31).
        let reopen_task = runtime_state.spawn_reopen_on_hup();
        // Snapshot-Zustand für GET /admin/api/v1/version (F15, AIGW-75).
        let control = control.layer(axum::Extension(runtime_state.snapshot.clone()));
        server::run(
            listeners,
            data_router,
            control,
            runtime_state.snapshot.clone(),
            shutdown_signal(),
        )
        .await?;
        if let Some(t) = reopen_task {
            t.abort();
        }
        drop(mcp_task);
        drop(routes_task);
        drop(plugins_task);
        drop(cache_rt);
        // Gepuffertes Tool-Inventar schreiben (AIGW-87).
        drop(tools_task);
        tools.flush().await;
        runtime_state.shutdown().await;
        tracing::info!("ai-gw beendet");
        Ok(())
    })
}

/// Tool-Inventar mit Store-Persistenz, Warmstart der bekannten Hashes und Ereignis-Webhooks.
/// Objekte, die Entra Workload/Managed Identity nutzen (Zugangsdaten mit `spec.entra`,
/// Deployments mit `settings.entraCredential`).
fn entra_ambient_users(snap: &aigw_core::snapshot::Snapshot) -> Vec<String> {
    use aigw_core::resource::EntraCredentialKind as K;
    let mut out: Vec<String> =
        snap.credentials
            .values()
            .filter(|c| {
                c.spec.entra.as_ref().is_some_and(|e| {
                    matches!(e.credential, K::ManagedIdentity | K::WorkloadIdentity)
                })
            })
            .map(|c| format!("provider-credential/{}", c.metadata.name))
            .collect();
    for (name, g) in &snap.model_groups {
        // Aliasse zeigen auf dieselbe Gruppe.
        if g.group.metadata.name != *name {
            continue;
        }
        for d in &g.deployments {
            let m = d
                .deployment
                .spec
                .settings
                .get("entraCredential")
                .and_then(serde_json::Value::as_str);
            if matches!(m, Some("managedIdentity" | "workloadIdentity")) {
                out.push(format!("deployment/{}", d.deployment.metadata.name));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Startmeldung zu `upstream.entra.ambient_identity` (AIGW-90): ohne Freigabe eine Warnung,
/// wenn die Konfiguration Workload/Managed Identity nutzt – diese Deployments sind gesperrt.
fn entra_ambient_notice(config: &aigw_core::Config, snap: &aigw_core::snapshot::Snapshot) {
    let users = entra_ambient_users(snap);
    if config.upstream.entra.ambient_identity {
        tracing::info!(
            objects = users.len(),
            "Entra Workload/Managed Identity freigegeben ({} = true)",
            aigw_proxy::ENTRA_AMBIENT_IDENTITY_KEY
        );
    } else if !users.is_empty() {
        tracing::warn!(
            objects = %users.join(", "),
            "Entra Workload/Managed Identity nicht freigegeben – betroffene Deployments sind \
             nicht nutzbar; zum Freigeben in der Startkonfiguration {}: true setzen \
             (ENV AIGW_UPSTREAM__ENTRA__AMBIENT_IDENTITY=true)",
            aigw_proxy::ENTRA_AMBIENT_IDENTITY_KEY
        );
    }
}

async fn tool_inventory(
    config: &aigw_core::Config,
    rt: &runtime::Runtime,
) -> std::sync::Arc<aigw_proxy::tools::ToolInventory> {
    let sink = aigw_store::tools::StoreToolSink::new(
        rt.store.clone(),
        aigw_store::tools::ToolSinkOptions {
            on_change: config.tools.on_change,
        },
    );
    let mut inv = aigw_proxy::tools::ToolInventory::new(std::sync::Arc::new(sink), &config.tools);
    if let Some(events) = rt.event_dispatcher() {
        inv = inv.with_hook(std::sync::Arc::new(
            move |ev: &aigw_core::tools::ToolEvent| {
                if ev.kind == aigw_core::tools::ToolEventKind::Discovered {
                    return;
                }
                events.emit(aigw_webhook::Event::new(
                    ev.kind.event_type(),
                    serde_json::json!({
                        "toolId": ev.id,
                        "tool": &*ev.key.name,
                        "origin": ev.origin,
                        "organizationId": ev.key.organization,
                        "teamId": ev.key.team,
                        "apiKeyId": ev.key.api_key,
                        "signals": ev.signals,
                        "blockedBy": ev.blocked_by,
                    }),
                ));
            },
        ));
    }
    if inv.enabled() {
        let limit = u32::try_from(config.tools.max_entries).unwrap_or(u32::MAX);
        match rt.store.tool_inventory_known(limit).await {
            Ok(known) => inv.preload(known),
            Err(e) => tracing::warn!(error = %e, "Tool-Inventar: Warmstart fehlgeschlagen"),
        }
    }
    std::sync::Arc::new(inv)
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
        // Kaltstart aus dem Snapshot-Cache (F14): Datenbank nicht erreichbar → sicherheitshalber
        // kein Bootstrap-Token.
        BootstrapMode::Auto
            if store.bootstrap_disabled().await.unwrap_or_else(|e| {
                tracing::warn!(error = %e, "Bootstrap-Zustand nicht lesbar – Token abgeschaltet");
                true
            }) =>
        {
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
            // AIGW-58: Das Token nicht über `tracing` ausgeben – Logs landen oft in zentralen
            // Systemen (OTLP, Log-Aggregation). Nur einmal direkt auf stderr.
            tracing::warn!(
                "Admin-API: Bootstrap-Token erzeugt und einmalig auf stderr ausgegeben (gilt nur \
                 für diesen Prozess; fest vorgeben über {BOOTSTRAP_TOKEN_ENV})"
            );
            #[allow(clippy::print_stderr)]
            {
                eprintln!("aigw: Bootstrap-Token (Admin-API, nur dieser Prozess): {secret}");
            }
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
