// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Rangtabellen der OpenAI-Kodierungen zur Laufzeit (AIGW-95, AIGW-92).
//!
//! ai-gw enthält die Tabellen nicht (weder im Repository noch im Binary oder Image; ihre
//! Lizenz ist nicht eindeutig geklärt). Der Betreiber liefert die Originaldateien von OpenAI
//! (`cl100k_base.tiktoken`, `o200k_base.tiktoken` – dieselben Dateien, die tiktoken lädt) in
//! einem Verzeichnis ([`install_from_dir`]) oder lädt sie herunter ([`Encoding::source_url`],
//! [`install_tiktoken`], `aigw tokenizer fetch`). Jede Datei wird vor der Verwendung gegen die
//! eingebaute SHA-256-Summe geprüft; eine veränderte oder andere Datei wird abgelehnt. Ohne
//! Tabellen zählt die Heuristik.
//!
//! Tests mit den echten Tabellen lesen sie aus `$AIGW_TOKENIZER_RANKS_DIR`
//! ([`install_from_env`]) und werden ohne die Variable übersprungen.
//!
//! Format einer `*.tiktoken`-Datei: je Zeile `<Base64 des Tokens> <Rang>\n`, Ränge lückenlos
//! aufsteigend ab 0.

use std::path::Path;
use std::sync::OnceLock;

use base64::Engine as _;

use crate::bpe::{Ranks, TableError};
use crate::{CoreBpe, Encoding};

/// Basis-URL der offiziellen Quelle (wie in `tiktoken_ext/openai_public.py`).
pub const SOURCE_BASE_URL: &str = "https://openaipublic.blob.core.windows.net/encodings/";

/// Umgebungsvariable mit dem Verzeichnis der echten Tabellen für Tests ([`install_from_env`]).
pub const TEST_DIR_ENV: &str = "AIGW_TOKENIZER_RANKS_DIR";
/// Ist diese Variable `1`, schlagen Tests ohne Tabellen fehl statt übersprungen zu werden.
pub const TEST_REQUIRE_ENV: &str = "AIGW_TOKENIZER_REQUIRE_RANKS";

/// Fehler beim Laden einer `*.tiktoken`-Datei.
#[derive(Debug, thiserror::Error)]
pub enum RanksError {
    /// Datei nicht lesbar.
    #[error("{file}: {source}")]
    Io {
        /// Dateiname.
        file: &'static str,
        /// Ursache.
        source: std::io::Error,
    },
    /// Prüfsumme weicht ab (andere oder veränderte Datei).
    #[error("{file}: SHA-256 {actual} stimmt nicht mit der erwarteten Summe {expected} überein")]
    Checksum {
        /// Dateiname.
        file: &'static str,
        /// Erwartete Summe (hex).
        expected: &'static str,
        /// Tatsächliche Summe (hex).
        actual: String,
    },
    /// Inhalt entspricht nicht dem tiktoken-Format.
    #[error("{file}: Zeile {line}: {msg}")]
    Format {
        /// Dateiname.
        file: &'static str,
        /// Zeilennummer (ab 1).
        line: usize,
        /// Beschreibung.
        msg: &'static str,
    },
    /// Tabelle ungültig.
    #[error("{file}: {source}")]
    Table {
        /// Dateiname.
        file: &'static str,
        /// Ursache.
        source: TableError,
    },
}

/// Ergebnis von [`install_from_dir`] je Kodierung.
#[derive(Debug)]
pub enum DirOutcome {
    /// Aus dem Verzeichnis geladen (oder bereits zuvor installiert).
    Installed,
    /// Datei fehlt.
    Missing,
    /// Datei vorhanden, aber nicht verwendbar.
    Invalid(RanksError),
}

impl Encoding {
    /// Dateiname wie bei tiktoken (`<name>.tiktoken`).
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::Cl100kBase => "cl100k_base.tiktoken",
            Self::O200kBase => "o200k_base.tiktoken",
        }
    }

    /// Erwartete SHA-256-Summe (hex) der Datei [`Encoding::file_name`] (wie
    /// `expected_hash` in tiktoken).
    pub const fn sha256(self) -> &'static str {
        match self {
            Self::Cl100kBase => "223921b76ee99bde995b7ff738513eef100fb51d18c93597a113bcffe865b2a7",
            Self::O200kBase => "446a9538cb6c348e3516120d7c08b09f57c36495e2acfffe59a5bf8b0cfb1a2d",
        }
    }

    /// Offizielle Download-Adresse der Datei.
    pub fn source_url(self) -> String {
        format!("{SOURCE_BASE_URL}{}", self.file_name())
    }

    /// `true`, wenn die Tabelle zur Laufzeit installiert ist (sonst zählt [`Encoding::count`]
    /// mit der Heuristik).
    pub fn is_available(self) -> bool {
        self.load().is_ok()
    }
}

/// Zur Laufzeit installierte Tabellen.
pub(crate) fn installed(e: Encoding) -> &'static OnceLock<CoreBpe> {
    static CL: OnceLock<CoreBpe> = OnceLock::new();
    static O2: OnceLock<CoreBpe> = OnceLock::new();
    match e {
        Encoding::Cl100kBase => &CL,
        Encoding::O200kBase => &O2,
    }
}

/// SHA-256 von `data` (hex, klein).
pub fn sha256_hex(data: &[u8]) -> String {
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, data);
    digest.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// Prüft die SHA-256-Summe von `data` gegen die eingebaute Summe von `e`.
pub fn verify(e: Encoding, data: &[u8]) -> Result<(), RanksError> {
    let actual = sha256_hex(data);
    if actual == e.sha256() {
        Ok(())
    } else {
        Err(RanksError::Checksum {
            file: e.file_name(),
            expected: e.sha256(),
            actual,
        })
    }
}

/// Prüft und liest eine `*.tiktoken`-Datei (Prüfsumme, Format, Tabelle).
///
/// Der Speicher der Tabelle lebt bis zum Prozessende (einmal je Kodierung gedacht).
pub fn parse_tiktoken(e: Encoding, data: &[u8]) -> Result<CoreBpe, RanksError> {
    verify(e, data)?;
    parse_tiktoken_unverified(e, data)
}

/// Wie [`parse_tiktoken`], aber **ohne** Prüfsumme – nur Format und Tabelle. Für Tests mit
/// eigenen (synthetischen) Tabellen; das Gateway verwendet immer [`parse_tiktoken`].
pub fn parse_tiktoken_unverified(e: Encoding, data: &[u8]) -> Result<CoreBpe, RanksError> {
    let file = e.file_name();
    let fmt = |line: usize, msg: &'static str| RanksError::Format { file, line, msg };
    // In das kompakte Längenpräfix-Format umsetzen (`Ranks::from_blob`).
    let mut blob = Vec::with_capacity(data.len() / 2);
    blob.extend_from_slice(b"AGTK");
    blob.extend_from_slice(&[0; 4]);
    let mut n: u32 = 0;
    let mut tok = Vec::with_capacity(256);
    for (i, line) in data.split(|&b| b == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let (b64, rank) = line
            .iter()
            .position(|&b| b == b' ')
            .map(|p| (&line[..p], &line[p + 1..]))
            .ok_or_else(|| fmt(i + 1, "Leerzeichen fehlt"))?;
        let rank: u32 = std::str::from_utf8(rank)
            .ok()
            .and_then(|r| r.parse().ok())
            .ok_or_else(|| fmt(i + 1, "Rang ist keine Zahl"))?;
        if rank != n {
            return Err(fmt(i + 1, "Ränge nicht lückenlos aufsteigend"));
        }
        tok.clear();
        base64::engine::general_purpose::STANDARD
            .decode_vec(b64, &mut tok)
            .map_err(|_| fmt(i + 1, "Base64 ungültig"))?;
        let len = u8::try_from(tok.len()).map_err(|_| fmt(i + 1, "Token länger als 255 Byte"))?;
        blob.push(len);
        blob.extend_from_slice(&tok);
        n = n
            .checked_add(1)
            .ok_or_else(|| fmt(i + 1, "zu viele Ränge"))?;
    }
    if let Some(count) = blob.get_mut(4..8) {
        count.copy_from_slice(&n.to_le_bytes());
    }
    let blob: &'static [u8] = Box::leak(blob.into_boxed_slice());
    let ranks = Ranks::from_blob(blob).map_err(|source| RanksError::Table { file, source })?;
    crate::unicode::warm_up();
    Ok(CoreBpe {
        ranks,
        pattern: e.pattern(),
    })
}

/// Prüft `data` und installiert die Tabelle für `e`.
///
/// Ist bereits eine Tabelle installiert, wird nur geprüft; die installierte bleibt bestehen.
pub fn install_tiktoken(e: Encoding, data: &[u8]) -> Result<(), RanksError> {
    if installed(e).get().is_some() {
        return verify(e, data);
    }
    let bpe = parse_tiktoken(e, data)?;
    let _ = installed(e).set(bpe);
    Ok(())
}

/// Installiert eine bereits gelesene Tabelle (z. B. aus [`parse_tiktoken_unverified`] in
/// Tests). `false`, wenn für `e` schon eine Tabelle installiert war.
pub fn install_parsed(e: Encoding, bpe: CoreBpe) -> bool {
    installed(e).set(bpe).is_ok()
}

/// Lädt alle Kodierungen aus `dir` (`<name>.tiktoken`), soweit vorhanden.
pub fn install_from_dir(dir: &Path) -> Vec<(Encoding, DirOutcome)> {
    Encoding::ALL
        .into_iter()
        .map(|e| {
            if installed(e).get().is_some() {
                return (e, DirOutcome::Installed);
            }
            let file = e.file_name();
            let outcome = match std::fs::read(dir.join(file)) {
                Ok(data) => match install_tiktoken(e, &data) {
                    Ok(()) => DirOutcome::Installed,
                    Err(err) => DirOutcome::Invalid(err),
                },
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => DirOutcome::Missing,
                Err(source) => DirOutcome::Invalid(RanksError::Io { file, source }),
            };
            (e, outcome)
        })
        .collect()
}

/// Für Tests mit den echten Tabellen: lädt sie aus `$AIGW_TOKENIZER_RANKS_DIR`
/// ([`TEST_DIR_ENV`]).
///
/// - `Ok(true)`: beide Tabellen verfügbar.
/// - `Ok(false)`: Variable nicht gesetzt – der Test soll sich überspringen.
/// - `Err`: Variable gesetzt, aber Dateien fehlen bzw. sind ungültig, oder
///   `AIGW_TOKENIZER_REQUIRE_RANKS=1` ohne Variable.
pub fn install_from_env() -> Result<bool, String> {
    let required = std::env::var(TEST_REQUIRE_ENV).is_ok_and(|v| v == "1");
    let Some(dir) = std::env::var_os(TEST_DIR_ENV).filter(|d| !d.is_empty()) else {
        return if required {
            Err(format!(
                "{TEST_REQUIRE_ENV}=1, aber {TEST_DIR_ENV} ist nicht gesetzt"
            ))
        } else {
            Ok(false)
        };
    };
    let dir = std::path::PathBuf::from(dir);
    let mut problems = Vec::new();
    for (e, outcome) in install_from_dir(&dir) {
        match outcome {
            DirOutcome::Installed => {}
            DirOutcome::Missing => problems.push(format!("{}: fehlt", e.file_name())),
            DirOutcome::Invalid(err) => problems.push(err.to_string()),
        }
    }
    if problems.is_empty() {
        Ok(true)
    } else {
        Err(format!("{}: {}", dir.display(), problems.join("; ")))
    }
}

/// Wie [`install_from_env`] für Tests: `true` = Tabellen da, `false` = überspringen (mit
/// Hinweis auf stderr); bei Fehlern Abbruch des Tests.
///
/// # Panics
///
/// Wenn [`install_from_env`] einen Fehler liefert.
#[allow(clippy::panic, clippy::print_stderr)]
pub fn tables_for_test(test: &str) -> bool {
    match install_from_env() {
        Ok(true) => true,
        Ok(false) => {
            eprintln!(
                "{test}: übersprungen – echte tiktoken-Tabellen fehlen ({TEST_DIR_ENV} setzen, \
                 z. B. nach `scripts/fetch-tokenizers.sh`)"
            );
            false
        }
        Err(e) => panic!("{test}: {e}"),
    }
}
