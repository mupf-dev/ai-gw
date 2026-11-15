// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! `aigw keygen` (F6): erzeugt einen Master-Key (32 Byte) als Hex oder Base64 – auf die
//! Standardausgabe oder in eine neue Datei mit Rechten `0600`.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use aigw_store::crypto::MasterKey;
use clap::{Args, ValueEnum};

/// Kodierung des Keys.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum KeyFormat {
    /// 64 Hex-Zeichen.
    #[default]
    Hex,
    /// Base64 (Standard-Alphabet, 44 Zeichen).
    Base64,
}

/// Parameter von `aigw keygen`.
#[derive(Debug, Clone, Args)]
pub(crate) struct KeygenArgs {
    /// Kodierung (beide werden von `secrets.master_key_file` bzw. `AIGW_MASTER_KEY` gelesen).
    #[arg(long, value_enum, default_value_t = KeyFormat::Hex)]
    pub(crate) format: KeyFormat,
    /// In diese Datei schreiben (neu, Rechte 0600) statt auf die Standardausgabe.
    #[arg(short, long)]
    pub(crate) out: Option<PathBuf>,
    /// Vorhandene Datei überschreiben (Vorsicht: der alte Key ist danach verloren).
    #[arg(long, requires = "out")]
    pub(crate) force: bool,
}

/// Kodiert 32 Rohbytes.
pub(crate) fn encode(bytes: &[u8], format: KeyFormat) -> String {
    match format {
        KeyFormat::Hex => bytes.iter().map(|b| format!("{b:02x}")).collect(),
        KeyFormat::Base64 => {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(bytes)
        }
    }
}

/// Schreibt `text` + Zeilenumbruch in eine Datei mit Rechten 0600 (neu, außer `force`).
pub(crate) fn write_private(path: &Path, text: &str, force: bool) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true);
    if force {
        opts.create(true).truncate(true);
    } else {
        opts.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    #[cfg(unix)]
    {
        // Bei `force` hatte eine bestehende Datei evtl. weitere Rechte.
        use std::os::unix::fs::PermissionsExt as _;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    writeln!(f, "{text}")?;
    f.sync_all()
}

/// Führt `aigw keygen` aus.
pub(crate) fn run(args: &KeygenArgs) -> Result<(), String> {
    let bytes = MasterKey::random_bytes().map_err(|e| e.to_string())?;
    let text = encode(&bytes, args.format);
    // Selbstprüfung: der Text muss sich wieder als Key lesen lassen.
    let key = MasterKey::parse(&text).map_err(|e| e.to_string())?;
    match &args.out {
        None => {
            let mut out = std::io::stdout().lock();
            writeln!(out, "{text}").map_err(|e| e.to_string())?;
        }
        Some(path) => {
            write_private(path, &text, args.force).map_err(|e| {
                let hint = if e.kind() == std::io::ErrorKind::AlreadyExists {
                    " (überschreiben nur mit --force)"
                } else {
                    ""
                };
                format!("{}: {e}{hint}", path.display())
            })?;
            let _ = writeln!(
                std::io::stderr(),
                "Master-Key {} geschrieben nach {} (Rechte 0600) – in \
                 secrets.master_key_file eintragen",
                key.id(),
                path.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_roundtrip() {
        let raw = [9u8; 32];
        for f in [KeyFormat::Hex, KeyFormat::Base64] {
            let t = encode(&raw, f);
            assert!(MasterKey::parse(&t).is_ok(), "{f:?}");
        }
        assert_eq!(encode(&raw, KeyFormat::Hex).len(), 64);
        assert_eq!(encode(&raw, KeyFormat::Base64).len(), 44);
    }

    #[test]
    fn writes_private_file_once() {
        let dir = std::env::temp_dir().join(format!("aigw-keygen-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("master.key");
        let _ = std::fs::remove_file(&path);
        let args = KeygenArgs {
            format: KeyFormat::Base64,
            out: Some(path.clone()),
            force: false,
        };
        run(&args).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(MasterKey::parse(&text).is_ok());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        // kein stilles Überschreiben
        let err = run(&args).unwrap_err();
        assert!(err.contains("--force"), "{err}");
        run(&KeygenArgs {
            force: true,
            ..args
        })
        .unwrap();
        assert_ne!(std::fs::read_to_string(&path).unwrap(), text);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
