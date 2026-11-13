// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Zusätzliche Schreibvorgänge in der Transaktion einer Änderung (AIGW-67): Teams und Keys
//! auf neue Grenzen kürzen (`?cascade=clamp`) und Audit-Hinweis bei `?force=true`.

use aigw_core::resource::{ApiKey, ApiKeySpec, Id, Object, ResourceKind, TeamSpec};

use super::{Action, AdminTx, Resource};
use crate::StoreError;

/// Zusätzliche Schreibvorgänge zu einer Änderung (alles in **einer** Transaktion).
#[derive(Debug, Clone, Default)]
pub struct WriteExtras {
    /// Teams mit gekürzter Spezifikation (bisheriger Stand, neue Spezifikation); Audit
    /// je Team mit Aktion `clamp`.
    pub clamp_teams: Vec<(Object<TeamSpec>, TeamSpec)>,
    /// Keys mit gekürzter Spezifikation (bisheriger Stand, neue Spezifikation); Audit je
    /// Key mit Aktion `clamp`.
    pub clamp_api_keys: Vec<(ApiKey, ApiKeySpec)>,
    /// Hinweis zum geänderten Objekt (Audit-Eintrag mit Aktion `force`, Inhalt in `after`).
    pub force_note: Option<serde_json::Value>,
}

impl WriteExtras {
    /// `true`, wenn nichts zusätzlich zu schreiben ist.
    pub fn is_empty(&self) -> bool {
        self.clamp_teams.is_empty() && self.clamp_api_keys.is_empty() && self.force_note.is_none()
    }
}

impl AdminTx {
    /// Schreibt `extras` zur Änderung von `(kind, id, name)`.
    pub(crate) async fn apply_extras(
        &mut self,
        extras: &WriteExtras,
        kind: ResourceKind,
        id: Id,
        name: &str,
    ) -> Result<(), StoreError> {
        if let Some(note) = &extras.force_note {
            self.audit(Action::Force, kind, Some(id), name, None::<&()>, Some(note))
                .await?;
        }
        for (before, spec) in &extras.clamp_teams {
            let cols = self.resolve_cols(spec.to_columns()?).await?;
            self.update_row(ResourceKind::Team, before.id, &before.metadata, cols)
                .await?;
            let after = Object {
                id: before.id,
                metadata: before.metadata.clone(),
                spec: spec.clone(),
            };
            self.audit(
                Action::Clamp,
                ResourceKind::Team,
                Some(before.id),
                &before.metadata.name,
                Some(before),
                Some(&after),
            )
            .await?;
        }
        for (before, spec) in &extras.clamp_api_keys {
            let cols = self.resolve_cols(spec.to_columns()?).await?;
            self.update_row(ResourceKind::ApiKey, before.id, &before.metadata, cols)
                .await?;
            let after = ApiKey {
                spec: spec.clone(),
                ..before.clone()
            };
            self.audit(
                Action::Clamp,
                ResourceKind::ApiKey,
                Some(before.id),
                &before.metadata.name,
                Some(before),
                Some(&after),
            )
            .await?;
        }
        Ok(())
    }
}
