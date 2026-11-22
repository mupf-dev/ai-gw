// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Dateipfade innerhalb eines freigegebenen Verzeichnisses (F52 `gitops.secret_dir`,
//! F177 `upstream.ca_dir`).
//!
//! Zwei Stufen:
//! - [`lexical_within`]: rein textuell (ohne Dateisystem, auch auf einem anderen Rechner
//!   prüfbar) – relative Pfade gelten innerhalb des Verzeichnisses, `..`-Komponenten und
//!   Steuerzeichen sind verboten, absolute Pfade müssen im Verzeichnis liegen.
//! - [`resolve_within`]: zusätzlich über das Dateisystem – Symlinks werden aufgelöst
//!   (`canonicalize`), das Ziel muss weiterhin im (ebenfalls aufgelösten) Verzeichnis liegen.
//!   Davor immer prüfen, wenn die Datei tatsächlich gelesen wird.

use std::path::{Component, Path, PathBuf};

/// Höchstlänge eines Pfads.
const MAX_PATH_LEN: usize = 4096;

/// Prüft `path` textuell gegen `dir` und liefert den absoluten Pfad (relativ → `dir/path`).
pub fn lexical_within(dir: &Path, path: &str) -> Result<PathBuf, String> {
    if path.trim().is_empty() || path.len() > MAX_PATH_LEN || path.chars().any(char::is_control) {
        return Err("Pfad leer, zu lang oder mit Steuerzeichen".into());
    }
    let p = Path::new(path);
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!("Pfad {path:?} darf kein \"..\" enthalten"));
    }
    let full = if p.is_absolute() {
        p.to_path_buf()
    } else {
        dir.join(p)
    };
    // `CurDir`-Komponenten (`./`) entfernen; `..` ist oben ausgeschlossen.
    let full: PathBuf = full
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect();
    let base: PathBuf = dir
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect();
    if full == base || !full.starts_with(&base) {
        return Err(format!(
            "Pfad {path:?} liegt nicht im erlaubten Verzeichnis {}",
            dir.display()
        ));
    }
    Ok(full)
}

/// Wie [`lexical_within`], dazu Auflösung von Symlinks: das tatsächliche Ziel muss eine Datei
/// im aufgelösten Verzeichnis sein.
pub fn resolve_within(dir: &Path, path: &str) -> Result<PathBuf, String> {
    let full = lexical_within(dir, path)?;
    let base = dir
        .canonicalize()
        .map_err(|e| format!("Verzeichnis {} nicht lesbar: {e}", dir.display()))?;
    let real = full
        .canonicalize()
        .map_err(|e| format!("Datei {path:?} nicht lesbar: {e}"))?;
    if !real.starts_with(&base) || real == base {
        return Err(format!(
            "Pfad {path:?} führt über einen Symlink aus dem erlaubten Verzeichnis {} hinaus",
            dir.display()
        ));
    }
    if !real.is_file() {
        return Err(format!("Pfad {path:?} ist keine Datei"));
    }
    Ok(real)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_rules() {
        let d = Path::new("/run/aigw/secrets");
        assert_eq!(
            lexical_within(d, "openai").unwrap(),
            PathBuf::from("/run/aigw/secrets/openai")
        );
        assert_eq!(
            lexical_within(d, "./sub/x").unwrap(),
            PathBuf::from("/run/aigw/secrets/sub/x")
        );
        assert!(lexical_within(d, "/run/aigw/secrets/a").is_ok());
        for bad in [
            "",
            "../x",
            "a/../../etc/passwd",
            "/etc/passwd",
            "/run/aigw/secrets-other/x",
            "/run/aigw/secrets",
            "a\nb",
        ] {
            assert!(lexical_within(d, bad).is_err(), "{bad:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_out_are_rejected() {
        let root = std::env::temp_dir().join(format!("aigw-paths-{}", std::process::id()));
        let dir = root.join("allowed");
        let outside = root.join("outside");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(dir.join("ok.pem"), "x").unwrap();
        std::fs::write(outside.join("secret"), "y").unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), dir.join("link")).unwrap();
        std::os::unix::fs::symlink(dir.join("ok.pem"), dir.join("inner-link")).unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let d = dir.as_path();
        assert!(resolve_within(d, "ok.pem").is_ok());
        assert!(resolve_within(d, "inner-link").is_ok());
        let err = resolve_within(d, "link").unwrap_err();
        assert!(err.contains("Symlink"), "{err}");
        assert!(resolve_within(d, "fehlt").is_err());
        assert!(resolve_within(d, "sub").is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
