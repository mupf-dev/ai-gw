// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Rangtabellen `o200k_base`/`cl100k_base` zur Laufzeit (AIGW-95, AIGW-92).
//!
//! ai-gw enthält die Tabellen nicht (Lizenz der OpenAI-Dateien ungeklärt). Beim Start:
//!
//! - Tabellen aus `tokenizer.ranks_dir` laden (SHA-256-Prüfung in [`aigw_tokenizer::ranks`]).
//! - Fehlende Dateien lädt `tokenizer.download_ranks: true` (Opt-in) im Hintergrund von der
//!   offiziellen Quelle (gemeinsamer Client `aigw-outbound` mit den SSRF-Regeln aus
//!   `admin.allow_*_upstreams`), prüft sie und legt sie im Verzeichnis ab (ist es nicht
//!   beschreibbar, nur im Speicher).
//! - Bis dahin bzw. ohne Tabellen zählt die Heuristik; das meldet eine Warnung beim Start.
//!
//! `aigw tokenizer fetch|verify` ([`TokenizerCommand`]) lädt bzw. prüft die Dateien vorab
//! (Init-Container, Image-Ableitung, Offline-Spiegel).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use aigw_core::Config;
use aigw_outbound::{FetchLimits, OutboundClient};
use aigw_tokenizer::{DirOutcome, Encoding, ranks};
use bytes::Bytes;
use clap::Subcommand;

/// Zeitlimit eines Downloads (o200k_base ≈ 3,6 MB).
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
/// Größte angenommene Datei.
const DOWNLOAD_MAX_BYTES: usize = 16 * 1024 * 1024;

/// Lädt die Tabellen nach `config.tokenizer` (siehe Moduldoku). Fehler nur bei ungültiger
/// Konfiguration; fehlende oder defekte Dateien führen zur Heuristik mit Warnung.
pub(crate) async fn init(config: &Config) -> Result<(), String> {
    let cfg = &config.tokenizer;
    cfg.validate()?;
    let Some(dir) = ranks_dir(config) else {
        tracing::warn!(
            "tokenizer.ranks_dir nicht gesetzt: Tokens für o200k_base/cl100k_base (GPT-4o, \
             GPT-4, …) werden nur geschätzt (Heuristik). Tabellen mit `aigw tokenizer fetch` \
             bereitstellen"
        );
        return Ok(());
    };
    let missing = load_dir(dir.clone()).await;
    if missing.is_empty() {
        return Ok(());
    }
    let files = missing
        .iter()
        .map(|e| e.file_name())
        .collect::<Vec<_>>()
        .join(", ");
    if !cfg.download_ranks {
        tracing::warn!(
            dir = %dir.display(),
            files,
            "Rangtabellen fehlen bzw. sind ungültig: diese Kodierungen werden nur geschätzt \
             (Heuristik). `aigw tokenizer fetch` ausführen oder tokenizer.download_ranks: true \
             setzen (Quelle {})",
            ranks::SOURCE_BASE_URL
        );
        return Ok(());
    }
    let client = client(config).map_err(|e| format!("tokenizer.download_ranks: {e}"))?;
    tracing::warn!(
        dir = %dir.display(),
        files,
        "Rangtabellen fehlen – Download von {} im Hintergrund; bis dahin Heuristik",
        ranks::SOURCE_BASE_URL
    );
    tokio::spawn(async move {
        for e in missing {
            match download(&client, ranks::SOURCE_BASE_URL, &dir, e, e.sha256()).await {
                Ok(Stored::File) => {
                    tracing::info!(file = e.file_name(), "Rangtabelle geladen (Download)");
                }
                Ok(Stored::MemoryOnly(err)) => tracing::warn!(
                    file = e.file_name(),
                    error = %err,
                    "Rangtabelle geladen (Download), aber nicht gespeichert – nächster Start lädt erneut"
                ),
                Err(err) => tracing::warn!(
                    file = e.file_name(),
                    error = %err,
                    "Download der Rangtabelle fehlgeschlagen – Heuristik bleibt aktiv"
                ),
            }
        }
    });
    Ok(())
}

/// `tokenizer.ranks_dir`, falls gesetzt.
fn ranks_dir(config: &Config) -> Option<PathBuf> {
    config
        .tokenizer
        .ranks_dir
        .as_deref()
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
}

/// Ausgehender Client mit den SSRF-Regeln aus `admin.allow_*_upstreams`.
fn client(config: &Config) -> Result<OutboundClient, String> {
    OutboundClient::new(aigw_outbound::AddressGuard::policy(
        aigw_core::upstream::UpstreamPolicy::from_config(&config.admin)
            .map_err(|e| e.to_string())?,
    ))
    .map_err(|e| e.to_string())
}

/// Lädt alle Tabellen aus `dir`; Rückgabe = fehlende bzw. ungültige Kodierungen.
async fn load_dir(dir: PathBuf) -> Vec<Encoding> {
    let shown = dir.display().to_string();
    let outcomes = tokio::task::spawn_blocking(move || ranks::install_from_dir(&dir))
        .await
        .unwrap_or_default();
    let mut missing = Vec::new();
    for (e, o) in outcomes {
        match o {
            DirOutcome::Installed => {
                tracing::info!(dir = %shown, file = e.file_name(), "Rangtabelle geladen");
            }
            DirOutcome::Missing => missing.push(e),
            DirOutcome::Invalid(err) => {
                tracing::warn!(dir = %shown, error = %err, "Rangtabelle unbrauchbar");
                missing.push(e);
            }
        }
    }
    missing
}

/// Ergebnis von [`download`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Stored {
    /// Installiert und als Datei abgelegt.
    File,
    /// Installiert, aber das Verzeichnis war nicht beschreibbar (Fehlertext).
    MemoryOnly(String),
}

/// Lädt `<base><name>.tiktoken` und prüft die SHA-256-Summe (`expected`, im Betrieb immer
/// [`Encoding::sha256`]).
async fn fetch(
    client: &OutboundClient,
    base: &str,
    e: Encoding,
    expected: &str,
) -> Result<Bytes, String> {
    let url = format!("{base}{}", e.file_name());
    let req = http::Request::get(&url)
        .header(http::header::USER_AGENT, aigw_outbound::USER_AGENT)
        .body(Bytes::new())
        .map_err(|err| format!("{url}: {err}"))?;
    let reply = client
        .fetch(req, &FetchLimits::new(DOWNLOAD_TIMEOUT, DOWNLOAD_MAX_BYTES))
        .await
        .map_err(|err| format!("{url}: {err}"))?;
    if reply.status != http::StatusCode::OK {
        return Err(format!("{url}: HTTP {}", reply.status.as_u16()));
    }
    let actual = ranks::sha256_hex(&reply.body);
    if actual != expected {
        return Err(format!(
            "{url}: SHA-256 {actual} stimmt nicht mit der erwarteten Summe {expected} überein"
        ));
    }
    Ok(reply.body)
}

/// Schreibt `body` atomar als `dir/<name>.tiktoken` (temporäre Datei + rename).
fn store(dir: &Path, e: Encoding, body: &[u8]) -> Result<(), String> {
    let target = dir.join(e.file_name());
    let tmp = dir.join(format!(".{}.{}.tmp", e.file_name(), std::process::id()));
    std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    std::fs::write(&tmp, body).map_err(|err| format!("{}: {err}", tmp.display()))?;
    std::fs::rename(&tmp, &target).map_err(|err| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {err}", target.display())
    })
}

/// Lädt eine Tabelle ([`fetch`]), installiert sie und legt sie in `dir` ab. Eine falsche
/// Datei landet nie im Verzeichnis; ist `dir` nicht beschreibbar, bleibt die Tabelle im
/// Speicher installiert ([`Stored::MemoryOnly`]).
pub(crate) async fn download(
    client: &OutboundClient,
    base: &str,
    dir: &Path,
    e: Encoding,
    expected: &str,
) -> Result<Stored, String> {
    let body = fetch(client, base, e, expected).await?;
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<Stored, String> {
        // Prüfsumme ist geprüft (`fetch`); hier nur Format und Tabelle.
        let bpe = ranks::parse_tiktoken_unverified(e, &body).map_err(|err| err.to_string())?;
        ranks::install_parsed(e, bpe);
        Ok(match store(&dir, e, &body) {
            Ok(()) => Stored::File,
            Err(err) => Stored::MemoryOnly(err),
        })
    })
    .await
    .map_err(|err| err.to_string())?
}

/// `aigw tokenizer …`: Rangtabellen vorab bereitstellen bzw. prüfen.
#[derive(Debug, Subcommand)]
pub(crate) enum TokenizerCommand {
    /// Lädt `cl100k_base.tiktoken` und `o200k_base.tiktoken` von der offiziellen Quelle (oder
    /// einem Spiegel), prüft die SHA-256-Summen und legt sie im Verzeichnis ab. Vorhandene
    /// gültige Dateien werden nicht erneut geladen.
    Fetch {
        /// Zielverzeichnis (Standard: `tokenizer.ranks_dir` aus `--config`).
        #[arg(short, long)]
        dir: Option<PathBuf>,
        /// Konfiguration für `tokenizer.ranks_dir` und die SSRF-Regeln
        /// (`admin.allow_*_upstreams`, wichtig für einen Spiegel im internen Netz).
        #[arg(short, long, env = "AIGW_CONFIG")]
        config: Option<PathBuf>,
        /// Basis-URL eines Spiegels (mit abschließendem `/`); Prüfsummen bleiben die
        /// offiziellen.
        #[arg(long, default_value = ranks::SOURCE_BASE_URL)]
        base_url: String,
    },
    /// Prüft die Dateien im Verzeichnis (Exit-Code 1, wenn eine fehlt oder ungültig ist).
    Verify {
        /// Verzeichnis (Standard: `tokenizer.ranks_dir` aus `--config`).
        #[arg(short, long)]
        dir: Option<PathBuf>,
        /// Konfiguration für `tokenizer.ranks_dir`.
        #[arg(short, long, env = "AIGW_CONFIG")]
        config: Option<PathBuf>,
    },
}

/// Führt einen `aigw tokenizer`-Befehl aus.
pub(crate) fn run(cmd: TokenizerCommand) -> ExitCode {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return fail(&e.to_string()),
    };
    match rt.block_on(exec(cmd)) {
        Ok(code) => code,
        Err(e) => fail(&e),
    }
}

fn fail(msg: &str) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "aigw tokenizer: {msg}");
    ExitCode::FAILURE
}

/// Konfiguration (Datei bzw. nur Umgebung) und Verzeichnis (`--dir` vor
/// `tokenizer.ranks_dir`).
fn resolve(dir: Option<PathBuf>, config: Option<&Path>) -> Result<(Config, PathBuf), String> {
    let cfg = match config {
        Some(p) => Config::load(p).map_err(|e| format!("{}: {e}", p.display()))?,
        // Ohne Datei: Standardwerte + `AIGW_…`-Overrides (z. B. AIGW_TOKENIZER__RANKS_DIR).
        None => Config::from_yaml_with_env("", std::env::vars()).map_err(|e| e.to_string())?,
    };
    let dir = dir
        .or_else(|| ranks_dir(&cfg))
        .ok_or("kein Verzeichnis: --dir angeben oder tokenizer.ranks_dir konfigurieren")?;
    Ok((cfg, dir))
}

/// Prüft `dir/<name>.tiktoken` gegen die offizielle Prüfsumme.
fn check_file(dir: &Path, e: Encoding) -> Result<(), String> {
    let path = dir.join(e.file_name());
    let data = std::fs::read(&path).map_err(|err| format!("{}: {err}", path.display()))?;
    ranks::verify(e, &data).map_err(|err| err.to_string())
}

async fn exec(cmd: TokenizerCommand) -> Result<ExitCode, String> {
    let mut out = std::io::stdout().lock();
    let mut line = |s: String| writeln!(out, "{s}").map_err(|e| e.to_string());
    match cmd {
        TokenizerCommand::Verify { dir, config } => {
            let (_, dir) = resolve(dir, config.as_deref())?;
            let mut ok = true;
            for e in Encoding::ALL {
                match check_file(&dir, e) {
                    Ok(()) => line(format!("OK      {}", dir.join(e.file_name()).display()))?,
                    Err(err) => {
                        ok = false;
                        line(format!("FEHLER  {err}"))?;
                    }
                }
            }
            Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
        }
        TokenizerCommand::Fetch {
            dir,
            config,
            base_url,
        } => {
            let (cfg, dir) = resolve(dir, config.as_deref())?;
            if !base_url.ends_with('/') {
                return Err(format!("--base-url {base_url:?} muss mit / enden"));
            }
            let client = client(&cfg)?;
            let mut ok = true;
            for e in Encoding::ALL {
                if check_file(&dir, e).is_ok() {
                    line(format!("vorhanden {}", dir.join(e.file_name()).display()))?;
                    continue;
                }
                let result = match fetch(&client, &base_url, e, e.sha256()).await {
                    Ok(body) => store(&dir, e, &body),
                    Err(err) => Err(err),
                };
                match result {
                    Ok(()) => line(format!("geladen   {}", dir.join(e.file_name()).display()))?,
                    Err(err) => {
                        ok = false;
                        line(format!("FEHLER    {err}"))?;
                    }
                }
            }
            Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use aigw_outbound::AddressGuard;
    use axum::Router;
    use axum::routing::get;

    /// Synthetische `*.tiktoken` (256 Einzelbytes + "ab"); die echten Tabellen liegen nicht
    /// im Repository (AIGW-92).
    fn synthetic_tiktoken() -> Vec<u8> {
        use base64::Engine as _;
        let mut out = Vec::new();
        let tokens = (0..=255u8).map(|b| vec![b]).chain([b"ab".to_vec()]);
        for (rank, tok) in tokens.enumerate() {
            out.extend_from_slice(
                base64::engine::general_purpose::STANDARD
                    .encode(tok)
                    .as_bytes(),
            );
            out.extend_from_slice(format!(" {rank}\n").as_bytes());
        }
        out
    }

    async fn serve(router: Router) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, router).await });
        format!("http://{addr}/encodings/")
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aigw-ranks-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[tokio::test]
    async fn download_verifies_and_stores() {
        let good = Bytes::from(synthetic_tiktoken());
        let expected = ranks::sha256_hex(&good);
        let mut bad = good.to_vec();
        bad[10] ^= 1;
        let bad = Bytes::from(bad);
        let base = serve(
            Router::new()
                .route(
                    "/encodings/cl100k_base.tiktoken",
                    get(move || std::future::ready(good.clone())),
                )
                .route(
                    "/encodings/o200k_base.tiktoken",
                    get(move || std::future::ready(bad.clone())),
                ),
        )
        .await;
        let client = OutboundClient::new(AddressGuard::Trusted).unwrap();
        let dir = temp_dir("dl");

        assert_eq!(
            download(&client, &base, &dir, Encoding::Cl100kBase, &expected)
                .await
                .unwrap(),
            Stored::File
        );
        let stored = std::fs::read(dir.join("cl100k_base.tiktoken")).unwrap();
        assert_eq!(ranks::sha256_hex(&stored), expected);
        assert!(Encoding::Cl100kBase.is_available());
        assert_eq!(Encoding::Cl100kBase.count("abab"), 2);

        // Falsche Prüfsumme: Fehler, keine Datei, keine Tabelle.
        let err = download(&client, &base, &dir, Encoding::O200kBase, &expected)
            .await
            .unwrap_err();
        assert!(err.contains("SHA-256"), "{err}");
        assert!(!dir.join("o200k_base.tiktoken").exists());
        assert!(!Encoding::O200kBase.is_available());
        // Die offizielle Prüfsumme passt nicht zur synthetischen Datei.
        let err = fetch(&client, &base, Encoding::Cl100kBase, Encoding::Cl100kBase.sha256())
            .await
            .unwrap_err();
        assert!(err.contains("SHA-256"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn download_respects_ssrf_policy() {
        let base = serve(Router::new()).await;
        // Standardregeln: private/Loopback-Ziele gesperrt.
        let client = OutboundClient::new(AddressGuard::policy(
            aigw_core::upstream::UpstreamPolicy::from_config(&Default::default()).unwrap(),
        ))
        .unwrap();
        let dir = temp_dir("ssrf");
        let err = download(&client, &base, &dir, Encoding::O200kBase, "x")
            .await
            .unwrap_err();
        assert!(!dir.exists(), "{err}");
    }

    #[tokio::test]
    async fn init_validates_config() {
        let mut cfg = Config::default();
        cfg.tokenizer.download_ranks = true;
        assert!(init(&cfg).await.unwrap_err().contains("ranks_dir"));
        cfg.tokenizer.ranks_dir = Some(temp_dir("init").display().to_string());
        cfg.tokenizer.download_ranks = false;
        init(&cfg).await.unwrap();
    }
}

#[cfg(test)]
mod cli_tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn resolve_needs_dir() {
        assert!(resolve(None, None).unwrap_err().contains("--dir"));
        let (_, d) = resolve(Some("/x".into()), None).unwrap();
        assert_eq!(d, PathBuf::from("/x"));
    }

    #[tokio::test]
    async fn verify_reports_missing_and_invalid() {
        let dir = std::env::temp_dir().join(format!("aigw-ranks-verify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("o200k_base.tiktoken"), b"YQ== 0\n").unwrap();
        let code = exec(TokenizerCommand::Verify {
            dir: Some(dir.clone()),
            config: None,
        })
        .await
        .unwrap();
        assert_eq!(code, ExitCode::FAILURE);
        assert!(check_file(&dir, Encoding::O200kBase).unwrap_err().contains("SHA-256"));
        assert!(check_file(&dir, Encoding::Cl100kBase).is_err());
        // Fetch verweigert eine Basis-URL ohne abschließendes `/` (vor jedem Netzzugriff).
        let err = exec(TokenizerCommand::Fetch {
            dir: Some(dir.clone()),
            config: None,
            base_url: "https://example.invalid/x".into(),
        })
        .await
        .unwrap_err();
        assert!(err.contains("muss mit / enden"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
