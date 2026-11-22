// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Rangtabellen `o200k_base`/`cl100k_base` beim Start (AIGW-95).
//!
//! - Build **mit** `embedded-tokenizers` (Standard): nichts zu tun; `tokenizer.ranks_dir` wird
//!   nicht gelesen (Hinweis im Log).
//! - Build **ohne** das Feature: Tabellen aus `tokenizer.ranks_dir` laden (SHA-256-Prüfung in
//!   [`aigw_tokenizer::ranks`]). Fehlende Dateien lädt `tokenizer.download_ranks: true`
//!   einmalig im Hintergrund von der offiziellen Quelle (gemeinsamer Client mit den
//!   SSRF-Regeln aus `admin.allow_*_upstreams`) und legt sie im Verzeichnis ab. Bis dahin –
//!   bzw. ohne Tabellen – zählt die Heuristik; das meldet eine Warnung beim Start.

use std::path::{Path, PathBuf};
use std::time::Duration;

use aigw_core::Config;
use aigw_outbound::{FetchLimits, OutboundClient};
use aigw_tokenizer::{DirOutcome, Encoding, ranks};
use bytes::Bytes;

/// Zeitlimit eines Downloads (o200k_base ≈ 3,6 MB).
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
/// Größte angenommene Datei.
const DOWNLOAD_MAX_BYTES: usize = 16 * 1024 * 1024;

/// Lädt die Tabellen nach `config.tokenizer` (siehe Moduldoku). Fehler nur bei ungültiger
/// Konfiguration; fehlende oder defekte Dateien führen zur Heuristik mit Warnung.
pub(crate) async fn init(config: &Config) -> Result<(), String> {
    let cfg = &config.tokenizer;
    cfg.validate()?;
    let dir = cfg
        .ranks_dir
        .as_deref()
        .filter(|d| !d.is_empty())
        .map(PathBuf::from);
    if ranks::EMBEDDED {
        if dir.is_some() {
            tracing::info!(
                "Rangtabellen o200k_base/cl100k_base sind eingebettet – tokenizer.ranks_dir \
                 wird nicht gelesen"
            );
        }
        return Ok(());
    }
    let Some(dir) = dir else {
        tracing::warn!(
            "Build ohne eingebettete Rangtabellen und ohne tokenizer.ranks_dir: Tokens für \
             o200k_base/cl100k_base (GPT-4o, GPT-4, …) werden nur geschätzt (Heuristik)"
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
             (Heuristik). Dateien von {} ablegen oder tokenizer.download_ranks: true setzen",
            ranks::SOURCE_BASE_URL
        );
        return Ok(());
    }
    let client = OutboundClient::new(aigw_outbound::AddressGuard::policy(
        aigw_core::upstream::UpstreamPolicy::from_config(&config.admin)
            .map_err(|e| format!("tokenizer.download_ranks: {e}"))?,
    ))
    .map_err(|e| format!("tokenizer.download_ranks: {e}"))?;
    tracing::warn!(
        dir = %dir.display(),
        files,
        "Rangtabellen fehlen – Download von {} im Hintergrund; bis dahin Heuristik",
        ranks::SOURCE_BASE_URL
    );
    tokio::spawn(async move {
        for e in missing {
            match download(&client, ranks::SOURCE_BASE_URL, &dir, e).await {
                Ok(()) => tracing::info!(file = e.file_name(), "Rangtabelle geladen (Download)"),
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

/// Lädt alle Tabellen aus `dir`; Rückgabe = fehlende bzw. ungültige Kodierungen.
async fn load_dir(dir: PathBuf) -> Vec<Encoding> {
    let shown = dir.display().to_string();
    let outcomes = tokio::task::spawn_blocking(move || ranks::install_from_dir(&dir))
        .await
        .unwrap_or_default();
    let mut missing = Vec::new();
    for (e, o) in outcomes {
        match o {
            DirOutcome::Embedded | DirOutcome::Installed => {
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

/// Lädt `<base><name>.tiktoken`, prüft die SHA-256-Summe, legt die Datei atomar in `dir` ab
/// und installiert die Tabelle.
pub(crate) async fn download(
    client: &OutboundClient,
    base: &str,
    dir: &Path,
    e: Encoding,
) -> Result<(), String> {
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
    let body = reply.body;
    // Erst prüfen, dann schreiben: eine falsche Datei landet nie im Verzeichnis.
    ranks::verify(e, &body).map_err(|err| err.to_string())?;
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        let target = dir.join(e.file_name());
        let tmp = dir.join(format!(".{}.{}.tmp", e.file_name(), std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
        std::fs::write(&tmp, &body).map_err(|err| format!("{}: {err}", tmp.display()))?;
        std::fs::rename(&tmp, &target).map_err(|err| {
            let _ = std::fs::remove_file(&tmp);
            format!("{}: {err}", target.display())
        })?;
        ranks::install_tiktoken(e, &body).map_err(|err| err.to_string())
    })
    .await
    .map_err(|err| err.to_string())?
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use aigw_outbound::AddressGuard;
    use axum::Router;
    use axum::routing::get;

    /// Tabelle aus `crates/aigw-tokenizer/data/<name>.bin` als `*.tiktoken` (byte-genau).
    fn tiktoken_file(e: Encoding) -> Vec<u8> {
        use base64::Engine as _;
        let bin = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../aigw-tokenizer/data")
                .join(format!("{}.bin", e.name())),
        )
        .unwrap();
        let n = u32::from_le_bytes(bin[4..8].try_into().unwrap());
        let mut rest = &bin[8..];
        let mut out = Vec::new();
        for rank in 0..n {
            let len = usize::from(rest[0]);
            out.extend_from_slice(
                base64::engine::general_purpose::STANDARD
                    .encode(&rest[1..=len])
                    .as_bytes(),
            );
            out.extend_from_slice(format!(" {rank}\n").as_bytes());
            rest = &rest[len + 1..];
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
        let good = Bytes::from(tiktoken_file(Encoding::Cl100kBase));
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

        download(&client, &base, &dir, Encoding::Cl100kBase)
            .await
            .unwrap();
        let stored = std::fs::read(dir.join("cl100k_base.tiktoken")).unwrap();
        ranks::verify(Encoding::Cl100kBase, &stored).unwrap();
        assert!(Encoding::Cl100kBase.is_available());

        // Falsche Prüfsumme: Fehler, keine Datei.
        let err = download(&client, &base, &dir, Encoding::O200kBase)
            .await
            .unwrap_err();
        assert!(err.contains("SHA-256"), "{err}");
        assert!(!dir.join("o200k_base.tiktoken").exists());
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
        let err = download(&client, &base, &dir, Encoding::O200kBase)
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
