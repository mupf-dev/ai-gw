// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Auflösung von `valueFrom` zur Apply-Zeit (Umgebungsvariable oder Datei).

use aigw_core::resource::{SecretRef, SecretString, ValueFrom};

/// Quelle für Geheimnisse (austauschbar für Tests).
pub trait SecretSource: Send + Sync {
    /// Wert einer Umgebungsvariablen.
    fn env(&self, name: &str) -> Option<String>;
    /// Inhalt einer Datei.
    fn file(&self, path: &str) -> std::io::Result<String>;
    /// Pfad innerhalb von `dir` (F52): normalisiert, Symlinks aufgelöst, muss im Verzeichnis
    /// bleiben. Standard: über das Dateisystem ([`aigw_core::paths::resolve_within`]).
    fn file_within(&self, dir: &std::path::Path, path: &str) -> Result<String, String> {
        aigw_core::paths::resolve_within(dir, path).map(|p| p.to_string_lossy().into_owned())
    }
}

/// Umgebung und Dateisystem des laufenden Prozesses.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessSecrets;

impl SecretSource for ProcessSecrets {
    fn env(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn file(&self, path: &str) -> std::io::Result<String> {
        std::fs::read_to_string(path)
    }
}

/// Erlaubte Herkunft von Geheimnissen.
///
/// Die CLI läuft mit den Rechten des Betreibers (alles erlaubt). Über die Admin-API dürfen
/// Manifeste nicht beliebige Variablen oder Dateien des Servers lesen: dort gilt
/// [`SecretPolicy::restricted`] (nur Variablen mit Präfix, keine Dateien) – Dateien nur mit
/// `gitops.secret_dir` ([`SecretPolicy::with_file_dir`], F52).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SecretPolicy {
    /// Nur Umgebungsvariablen mit diesem Präfix (`None` = alle).
    pub env_prefix: Option<String>,
    /// `valueFrom.file` verboten.
    pub deny_files: bool,
    /// `valueFrom.file` nur innerhalb dieses Verzeichnisses (F52; relativ = darin, keine
    /// `..`, keine Symlinks hinaus).
    pub file_dir: Option<std::path::PathBuf>,
}

impl SecretPolicy {
    /// Eingeschränkt: nur `env` mit Präfix `prefix`, keine Dateien.
    pub fn restricted(prefix: &str) -> Self {
        Self {
            env_prefix: Some(prefix.to_owned()),
            deny_files: true,
            file_dir: None,
        }
    }

    /// Erlaubt `valueFrom.file` innerhalb von `dir` (`gitops.secret_dir`, F52); `None` lässt
    /// die Regel unverändert.
    #[must_use]
    pub fn with_file_dir(mut self, dir: Option<std::path::PathBuf>) -> Self {
        if let Some(d) = dir {
            self.deny_files = false;
            self.file_dir = Some(d);
        }
        self
    }
}

/// Löst einen Verweis auf (Klartext `value` ist in Manifesten bereits ausgeschlossen).
pub(crate) fn resolve(
    src: &dyn SecretSource,
    policy: &SecretPolicy,
    r: &SecretRef,
) -> Result<SecretString, String> {
    match (&r.value, &r.value_from) {
        (None, Some(ValueFrom::Env(name))) => {
            if let Some(prefix) = &policy.env_prefix
                && !name.starts_with(prefix.as_str())
            {
                return Err(format!(
                    "Umgebungsvariable {name} nicht erlaubt (hier nur mit Präfix {prefix})"
                ));
            }
            src.env(name)
                .map(SecretString::new)
                .ok_or_else(|| format!("Umgebungsvariable {name} nicht gesetzt"))
        }
        (None, Some(ValueFrom::File(path))) => {
            if policy.deny_files {
                return Err(format!(
                    "valueFrom.file ({path}) ist hier nicht erlaubt, nur valueFrom.env \
                     (Dateien nur mit gitops.secret_dir)"
                ));
            }
            let real;
            let path = match &policy.file_dir {
                Some(dir) => {
                    real = src
                        .file_within(dir, path)
                        .map_err(|e| format!("valueFrom.file: {e}"))?;
                    real.as_str()
                }
                None => path.as_str(),
            };
            src.file(path)
                .map(|s| SecretString::new(s.trim_end_matches(['\r', '\n'])))
                .map_err(|e| format!("Geheimnis-Datei {path} nicht lesbar: {e}"))
        }
        _ => Err("genau `valueFrom` (env oder file) angeben, kein Klartext".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed;
    impl SecretSource for Fixed {
        fn env(&self, name: &str) -> Option<String> {
            (name.starts_with("AIGW_SECRET_") || name == "OTHER").then(|| format!("v-{name}"))
        }
        fn file(&self, _: &str) -> std::io::Result<String> {
            Ok("datei\n".into())
        }
    }

    #[test]
    fn policy_restricts_env_and_files() {
        let open = SecretPolicy::default();
        let r = SecretPolicy::restricted("AIGW_SECRET_");
        let env = SecretRef::env("OTHER");
        assert_eq!(resolve(&Fixed, &open, &env).unwrap().expose(), "v-OTHER");
        assert!(resolve(&Fixed, &r, &env).is_err());
        let ok = SecretRef::env("AIGW_SECRET_X");
        assert!(resolve(&Fixed, &r, &ok).is_ok());
        let file = SecretRef {
            value: None,
            value_from: Some(ValueFrom::File("/x".into())),
        };
        assert_eq!(resolve(&Fixed, &open, &file).unwrap().expose(), "datei");
        assert!(resolve(&Fixed, &r, &file).is_err());
        assert!(resolve(&Fixed, &open, &SecretRef::env("FEHLT")).is_err());
        assert!(resolve(&Fixed, &open, &SecretRef::value("x")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn secret_dir_allows_files_inside_only() {
        let root = std::env::temp_dir().join(format!("aigw-secret-dir-{}", std::process::id()));
        let dir = root.join("secrets");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("openai"), "sk-inside\n").unwrap();
        std::fs::write(root.join("outside"), "geheim").unwrap();
        std::os::unix::fs::symlink(root.join("outside"), dir.join("link")).unwrap();
        let p = SecretPolicy::restricted("AIGW_SECRET_").with_file_dir(Some(dir.clone()));
        let file = |path: &str| SecretRef {
            value: None,
            value_from: Some(ValueFrom::File(path.into())),
        };
        assert_eq!(
            resolve(&ProcessSecrets, &p, &file("openai"))
                .unwrap()
                .expose(),
            "sk-inside"
        );
        let abs = dir.join("openai").to_string_lossy().into_owned();
        assert!(resolve(&ProcessSecrets, &p, &file(&abs)).is_ok());
        for bad in ["link", "../outside", "/etc/hostname", "fehlt"] {
            let err = resolve(&ProcessSecrets, &p, &file(bad)).unwrap_err();
            assert!(!err.contains("geheim"), "{err}");
            assert!(err.starts_with("valueFrom.file"), "{bad}: {err}");
        }
        // Ohne Verzeichnis bleibt es beim Verbot.
        let r = SecretPolicy::restricted("AIGW_SECRET_").with_file_dir(None);
        assert!(resolve(&ProcessSecrets, &r, &file("openai")).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
