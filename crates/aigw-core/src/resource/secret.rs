// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Geheimnisse: Klartext-Wrapper, der nie serialisiert wird, und Verweise für GitOps-YAML.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Platzhalter, der statt eines Geheimnisses ausgegeben wird.
pub const REDACTED: &str = "<redacted>";

/// Geheimer Klartext (z. B. API-Key eines Anbieters).
///
/// `Debug`, `Display` und `Serialize` geben nur [`REDACTED`] aus; der Wert ist ausschließlich
/// über [`SecretString::expose`] erreichbar. Deserialisieren ist erlaubt (Eingabe über die
/// Admin-API).
#[derive(Clone, PartialEq, Eq, utoipa::ToSchema)]
#[schema(value_type = String, format = Password)]
pub struct SecretString(String);

impl SecretString {
    /// Umschließt einen Klartext.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Gibt den Klartext frei. Aufrufer dürfen ihn nicht loggen oder serialisieren.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl Serialize for SecretString {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(REDACTED)
    }
}

impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s == REDACTED {
            // Schutz vor Rundreisen (GET → PUT), die den Platzhalter als Wert speichern würden.
            return Err(serde::de::Error::custom(
                "Platzhalter <redacted> ist kein gültiger Geheimwert",
            ));
        }
        Ok(Self(s))
    }
}

/// Herkunft eines Geheimnisses in GitOps-YAML (`valueFrom`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum ValueFrom {
    /// Umgebungsvariable des Prozesses, der die Ressource anwendet.
    Env(String),
    /// Datei (Inhalt ohne abschließenden Zeilenumbruch).
    File(String),
}

/// Ein Geheimwert: entweder direkt (`value`, nur Admin-API) oder als Verweis (`valueFrom`).
///
/// In GitOps-YAML ist nur `valueFrom` erlaubt (siehe [`crate::resource::Manifest::validate`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SecretRef {
    /// Klartext (nur über die Admin-API, wird sofort verschlüsselt).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<SecretString>,
    /// Verweis auf Umgebungsvariable oder Datei.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_from: Option<ValueFrom>,
}

/// Fehler beim Auflösen eines [`SecretRef`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SecretError {
    /// Weder `value` noch `valueFrom` (oder beides) gesetzt.
    #[error("Geheimnis braucht genau eines von `value` oder `valueFrom`")]
    Ambiguous,
    /// Umgebungsvariable fehlt.
    #[error("Umgebungsvariable {0} nicht gesetzt")]
    MissingEnv(String),
    /// Datei nicht lesbar.
    #[error("Geheimnis-Datei {path} nicht lesbar: {source}")]
    File {
        /// Pfad.
        path: PathBuf,
        /// Ursache.
        #[source]
        source: std::io::Error,
    },
}

impl SecretRef {
    /// Direkter Wert.
    pub fn value(v: impl Into<String>) -> Self {
        Self {
            value: Some(SecretString::new(v)),
            value_from: None,
        }
    }

    /// Verweis auf eine Umgebungsvariable.
    pub fn env(name: impl Into<String>) -> Self {
        Self {
            value: None,
            value_from: Some(ValueFrom::Env(name.into())),
        }
    }

    /// Löst den Wert auf (liest ggf. Umgebungsvariable oder Datei).
    pub fn resolve(&self) -> Result<SecretString, SecretError> {
        match (&self.value, &self.value_from) {
            (Some(v), None) => Ok(v.clone()),
            (None, Some(ValueFrom::Env(name))) => std::env::var(name)
                .map(SecretString)
                .map_err(|_| SecretError::MissingEnv(name.clone())),
            (None, Some(ValueFrom::File(path))) => std::fs::read_to_string(path.as_str())
                .map(|s| SecretString(s.trim_end_matches(['\r', '\n']).to_owned()))
                .map_err(|source| SecretError::File {
                    path: PathBuf::from(path),
                    source,
                }),
            _ => Err(SecretError::Ambiguous),
        }
    }
}

/// Entschlüsselte Zugangsdaten eines Anbieters (Feldname → Wert), nur im RAM.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CredentialData(pub BTreeMap<String, SecretString>);

impl CredentialData {
    /// Wert eines Felds (z. B. `api_key`).
    pub fn get(&self, field: &str) -> Option<&SecretString> {
        self.0.get(field)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_is_never_serialized() {
        let s = SecretString::new("sk-live-123");
        assert_eq!(format!("{s:?}"), REDACTED);
        assert_eq!(serde_json::to_string(&s).unwrap(), "\"<redacted>\"");
        assert!(serde_json::from_str::<SecretString>("\"<redacted>\"").is_err());
        let r = SecretRef::value("sk-live-123");
        assert!(!serde_json::to_string(&r).unwrap().contains("sk-live"));
    }

    #[test]
    fn value_from_shapes() {
        let r: SecretRef = serde_json::from_str(r#"{"valueFrom":{"env":"OPENAI_KEY"}}"#).unwrap();
        assert_eq!(r, SecretRef::env("OPENAI_KEY"));
        assert!(serde_json::from_str::<SecretRef>(r#"{"valueFrom":{"vault":"x"}}"#).is_err());
        assert!(matches!(
            SecretRef {
                value: None,
                value_from: None
            }
            .resolve(),
            Err(SecretError::Ambiguous)
        ));
    }
}
