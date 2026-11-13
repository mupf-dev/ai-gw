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
/// [`SecretPolicy::restricted`] (nur Variablen mit Präfix, keine Dateien).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SecretPolicy {
    /// Nur Umgebungsvariablen mit diesem Präfix (`None` = alle).
    pub env_prefix: Option<String>,
    /// `valueFrom.file` verboten.
    pub deny_files: bool,
}

impl SecretPolicy {
    /// Eingeschränkt: nur `env` mit Präfix `prefix`, keine Dateien.
    pub fn restricted(prefix: &str) -> Self {
        Self {
            env_prefix: Some(prefix.to_owned()),
            deny_files: true,
        }
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
                    "valueFrom.file ({path}) ist hier nicht erlaubt, nur valueFrom.env"
                ));
            }
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
}
