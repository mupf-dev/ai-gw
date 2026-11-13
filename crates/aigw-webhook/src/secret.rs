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
}

impl CredentialSource for SnapshotHandle {
    fn credential_field(&self, name: &str, field: &str) -> Option<SecretString> {
        self.load()
            .credential_data
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
