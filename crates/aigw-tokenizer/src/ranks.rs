// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Rangtabellen der OpenAI-Kodierungen zur Laufzeit (AIGW-95).
//!
//! Standard-Builds betten die Tabellen ein (Feature `embedded-tokenizers`). Ohne das Feature
//! liefert der Betreiber die Originaldateien von OpenAI (`cl100k_base.tiktoken`,
//! `o200k_base.tiktoken` – dieselben Dateien, die tiktoken lädt) in einem Verzeichnis
//! ([`install_from_dir`]) oder lädt sie einmalig herunter ([`Encoding::source_url`],
//! [`install_tiktoken`]). Jede Datei wird vor der Verwendung gegen die eingebaute SHA-256-Summe
//! geprüft; eine veränderte oder andere Datei wird abgelehnt.
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

/// `true`, wenn dieser Build die Tabellen eingebettet hat (Feature `embedded-tokenizers`).
pub const EMBEDDED: bool = cfg!(feature = "embedded-tokenizers");

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
    /// Tabelle ist eingebettet; das Verzeichnis wurde nicht gelesen.
    Embedded,
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

    /// `true`, wenn die Tabelle eingebettet oder zur Laufzeit installiert ist (sonst zählt
    /// [`Encoding::count`] mit der Heuristik).
    pub fn is_available(self) -> bool {
        self.load().is_ok()
    }
}

/// Zur Laufzeit installierte Tabellen (Build ohne `embedded-tokenizers`).
pub(crate) fn installed(e: Encoding) -> &'static OnceLock<CoreBpe> {
    static CL: OnceLock<CoreBpe> = OnceLock::new();
    static O2: OnceLock<CoreBpe> = OnceLock::new();
    match e {
        Encoding::Cl100kBase => &CL,
        Encoding::O200kBase => &O2,
    }
}

/// Prüft die SHA-256-Summe von `data` gegen die eingebaute Summe von `e`.
pub fn verify(e: Encoding, data: &[u8]) -> Result<(), RanksError> {
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, data);
    let actual: String = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
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
    let file = e.file_name();
    let fmt = |line: usize, msg: &'static str| RanksError::Format { file, line, msg };
    // In das kompakte Format der eingebetteten Tabellen umsetzen (gleicher Leser).
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
/// In einem Build mit eingebetteten Tabellen wird nur geprüft (die eingebettete Tabelle bleibt
/// maßgeblich). Ist bereits eine Tabelle installiert, bleibt sie bestehen.
pub fn install_tiktoken(e: Encoding, data: &[u8]) -> Result<(), RanksError> {
    if EMBEDDED {
        return verify(e, data);
    }
    if installed(e).get().is_some() {
        return Ok(());
    }
    let bpe = parse_tiktoken(e, data)?;
    let _ = installed(e).set(bpe);
    Ok(())
}

/// Lädt alle Kodierungen aus `dir` (`<name>.tiktoken`), soweit vorhanden.
///
/// Mit eingebetteten Tabellen wird `dir` nicht gelesen ([`DirOutcome::Embedded`]).
pub fn install_from_dir(dir: &Path) -> Vec<(Encoding, DirOutcome)> {
    Encoding::ALL
        .into_iter()
        .map(|e| {
            if EMBEDDED {
                return (e, DirOutcome::Embedded);
            }
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
