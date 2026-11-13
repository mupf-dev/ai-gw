// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Geheimnisse ausgehender Webhooks: `ProviderCredential`-Referenz (über den Snapshot),
//! Umgebungsvariable oder Datei.

use std::fmt;
use std::sync::Arc;

use aigw_core::config::WebhookSecret;
use aigw_core::resource::{SecretRef, SecretString, ValueFrom};
use aigw_core::snapshot::SnapshotHandle;

/// Liefert entschlüsselte Felder von `ProviderCredential`s (Betrieb: der aktuelle Snapshot).
pub trait CredentialSource: Send + Sync + fmt::Debug {
    /// Feld `field` der Zugangsdaten `name`.
    fn credential_field(&self, name: &str, field: &str) -> Option<SecretString>;

    /// Feld `field` des Geheimnisses (`Secret`, F149) `name`.
    fn secret_field(&self, _name: &str, _field: &str) -> Option<SecretString> {
        None
    }
}

impl CredentialSource for SnapshotHandle {
    fn credential_field(&self, name: &str, field: &str) -> Option<SecretString> {
        self.load()
            .credential_data
            .get(name)
            .and_then(|d| d.get(field))
            .cloned()
    }

    fn secret_field(&self, name: &str, field: &str) -> Option<SecretString> {
        self.load()
            .secret_data
            .get(name)
            .and_then(|d| d.get(field))
            .cloned()
    }
}

/// Aufgelöstes bzw. auflösbares Geheimnis eines Webhooks.
#[derive(Clone, Default)]
pub enum SecretResolver {
    /// Kein Geheimnis (unsignierte Aufrufe).
    #[default]
    None,
    /// Beim Bau gelesen (Umgebungsvariable, Datei).
    Fixed(SecretString),
    /// Bei jedem Aufruf aus einem `Secret` gelesen (F149; Rotation ohne Neustart).
    Secret {
        /// Name des `Secret`.
        name: String,
        /// Feld.
        field: String,
        /// Quelle (ohne: Fehler beim Aufruf).
        source: Option<Arc<dyn CredentialSource>>,
    },
    /// Bei jedem Aufruf aus den Zugangsdaten gelesen (Rotation ohne Neustart).
    Credential {
        /// Name der `ProviderCredential`.
        name: String,
        /// Feld.
        field: String,
        /// Quelle (ohne: Fehler beim Aufruf).
        source: Option<Arc<dyn CredentialSource>>,
    },
}

impl fmt::Debug for SecretResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::Fixed(_) => f.write_str("Fixed(<redacted>)"),
            Self::Secret { name, field, .. } => f
                .debug_struct("Secret")
                .field("name", name)
                .field("field", field)
                .finish_non_exhaustive(),
            Self::Credential { name, field, .. } => f
                .debug_struct("Credential")
                .field("name", name)
                .field("field", field)
                .finish_non_exhaustive(),
        }
    }
}

impl SecretResolver {
    /// Aus der Konfiguration; liest Umgebungsvariable bzw. Datei sofort.
    pub fn from_config(
        cfg: Option<&WebhookSecret>,
        source: Option<Arc<dyn CredentialSource>>,
    ) -> Result<Self, String> {
        let Some(cfg) = cfg else {
            return Ok(Self::None);
        };
        cfg.validate()?;
        if let Some(name) = &cfg.secret {
            if name.trim().is_empty() {
                return Err("secret.secret darf nicht leer sein".into());
            }
            return Ok(Self::Secret {
                name: name.clone(),
                field: cfg.field().to_owned(),
                source,
            });
        }
        if let Some(name) = &cfg.credential {
            if name.trim().is_empty() {
                return Err("secret.credential darf nicht leer sein".into());
            }
            return Ok(Self::Credential {
                name: name.clone(),
                field: cfg.field().to_owned(),
                source,
            });
        }
        let from = match (&cfg.env, &cfg.file) {
            (Some(e), None) => ValueFrom::Env(e.clone()),
            (None, Some(f)) => ValueFrom::File(f.display().to_string()),
            _ => return Err("secret: genau eines von credential, env oder file angeben".into()),
        };
        let value = SecretRef {
            value: None,
            value_from: Some(from),
        }
        .resolve()
        .map_err(|e| format!("secret: {e}"))?;
        if value.expose().is_empty() {
            return Err("secret: Geheimnis ist leer".into());
        }
        Ok(Self::Fixed(value))
    }

    /// Ist ein Geheimnis konfiguriert?
    pub fn is_some(&self) -> bool {
        !matches!(self, Self::None)
    }

    /// Aktueller Wert (`None` ohne Geheimnis).
    pub fn get(&self) -> Result<Option<SecretString>, String> {
        match self {
            Self::None => Ok(None),
            Self::Fixed(s) => Ok(Some(s.clone())),
            Self::Secret {
                name,
                field,
                source,
            } => {
                let source = source
                    .as_ref()
                    .ok_or_else(|| "keine Quelle für Secret-Referenzen".to_owned())?;
                source
                    .secret_field(name, field)
                    .filter(|s| !s.expose().is_empty())
                    .map(Some)
                    .ok_or_else(|| {
                        format!("Secret {name:?} ohne Feld {field:?} (Webhook-Geheimnis)")
                    })
            }
            Self::Credential {
                name,
                field,
                source,
            } => {
                let source = source
                    .as_ref()
                    .ok_or_else(|| "keine Quelle für Credential-Referenzen".to_owned())?;
                source
                    .credential_field(name, field)
                    .filter(|s| !s.expose().is_empty())
                    .map(Some)
                    .ok_or_else(|| {
                        format!("Zugangsdaten {name:?} ohne Feld {field:?} (Webhook-Geheimnis)")
                    })
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Src;

    impl CredentialSource for Src {
        fn credential_field(&self, name: &str, field: &str) -> Option<SecretString> {
            (name == "c" && field == WebhookSecret::DEFAULT_FIELD)
                .then(|| SecretString::new("cred"))
        }

        fn secret_field(&self, name: &str, field: &str) -> Option<SecretString> {
            (name == "s" && field == "value").then(|| SecretString::new("sec"))
        }
    }

    fn resolve(cfg: WebhookSecret) -> Result<Option<String>, String> {
        let src: Arc<dyn CredentialSource> = Arc::new(Src);
        SecretResolver::from_config(Some(&cfg), Some(src))?
            .get()
            .map(|s| s.map(|s| s.expose().to_owned()))
    }

    #[test]
    fn secret_and_credential_references() {
        let secret = WebhookSecret {
            secret: Some("s".into()),
            ..WebhookSecret::default()
        };
        assert_eq!(resolve(secret.clone()).unwrap().as_deref(), Some("sec"));
        let credential = WebhookSecret {
            credential: Some("c".into()),
            ..WebhookSecret::default()
        };
        assert_eq!(resolve(credential).unwrap().as_deref(), Some("cred"));
        // Unbekanntes Secret: Fehler beim Aufruf (nicht beim Bau – Anlage/Rotation später).
        let missing = WebhookSecret {
            secret: Some("x".into()),
            ..WebhookSecret::default()
        };
        assert!(resolve(missing).is_err());
        // Nur eine Quelle.
        let both = WebhookSecret {
            credential: Some("c".into()),
            ..secret
        };
        assert!(both.validate().is_err());
        let field_alone = WebhookSecret {
            field: Some("x".into()),
            env: Some("E".into()),
            ..WebhookSecret::default()
        };
        assert!(field_alone.validate().is_err());
    }
}
