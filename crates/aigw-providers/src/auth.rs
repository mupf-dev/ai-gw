// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Erweiterungspunkt für tokenbasierte Anbieter-Authentifizierung (Azure Entra ID, später
//! Vertex-OAuth).
//!
//! Der Hot Path ruft [`TokenSource::bearer_token`] synchron auf: Implementierungen halten ein
//! gültiges Token im RAM und erneuern es im Hintergrund vor Ablauf (kein Netzwerkzugriff pro
//! Anfrage). Eine Entra-ID-Implementierung (Client-Credentials, Managed Identity, Workload
//! Identity) ist noch nicht umgesetzt.

use std::fmt;

use aigw_core::ir::ApiError;
use aigw_core::resource::SecretString;

/// Liefert das aktuelle Bearer-Token eines Anbieters.
pub trait TokenSource: Send + Sync + fmt::Debug {
    /// Aktuelles Token (ohne `Bearer `-Präfix). Fehler, wenn (noch) keines vorliegt.
    fn bearer_token(&self) -> Result<SecretString, ApiError>;
}

/// Festes Token (Tests, kurzlebige Umgebungen).
#[derive(Debug, Clone)]
pub struct StaticToken(pub SecretString);

impl TokenSource for StaticToken {
    fn bearer_token(&self) -> Result<SecretString, ApiError> {
        Ok(self.0.clone())
    }
}
