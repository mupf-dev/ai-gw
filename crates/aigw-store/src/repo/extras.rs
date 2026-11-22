// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Zusätzliche Schreibvorgänge in der Transaktion einer Änderung (AIGW-67): Teams und Keys
//! auf neue Grenzen kürzen (`?cascade=clamp`), Audit-Hinweis bei `?force=true`, Verweise auf
//! gelöschte Modellgruppen/Aliasse entfernen (`?cascade=remove`, F66) und der erwartete Stand
//! des geänderten Objekts (F38).

use aigw_core::resource::{
    ApiKey, ApiKeySpec, Id, Object, OrganizationSpec, ResourceKind, TeamSpec,
};

use super::{Action, AdminTx, Expect, Resource};
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
    /// Organisationen ohne Verweise auf eine gelöschte Modellgruppe bzw. einen entfernten
    /// Alias (F66, `?cascade=remove`); Audit je Objekt mit Aktion `cascade-remove`.
    pub unref_organizations: Vec<(Object<OrganizationSpec>, OrganizationSpec)>,
    /// Teams ohne diese Verweise (F66).
    pub unref_teams: Vec<(Object<TeamSpec>, TeamSpec)>,
    /// Keys ohne diese Verweise (F66).
    pub unref_api_keys: Vec<(ApiKey, ApiKeySpec)>,
    /// Erwarteter Stand des geänderten Objekts (F38, `If-Match`).
    pub expect: Expect,
}

impl WriteExtras {
    /// `true`, wenn nichts zusätzlich zu schreiben ist.
    pub fn is_empty(&self) -> bool {
        self.clamp_teams.is_empty()
            && self.clamp_api_keys.is_empty()
            && self.force_note.is_none()
            && self.unref_organizations.is_empty()
            && self.unref_teams.is_empty()
            && self.unref_api_keys.is_empty()
    }

    /// Mit erwartetem Stand (F38).
    #[must_use]
    pub fn expecting(mut self, expect: Expect) -> Self {
        self.expect = expect;
        self
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
        for (before, spec) in &extras.unref_organizations {
            self.rewrite(
                ResourceKind::Organization,
                before,
                spec,
                Action::CascadeRemove,
            )
            .await?;
        }
        for (before, spec) in &extras.unref_teams {
            self.rewrite(ResourceKind::Team, before, spec, Action::CascadeRemove)
                .await?;
        }
        for (before, spec) in &extras.unref_api_keys {
            self.rewrite_key(before, spec, Action::CascadeRemove)
                .await?;
        }
        for (before, spec) in &extras.clamp_teams {
            self.rewrite(ResourceKind::Team, before, spec, Action::Clamp)
                .await?;
        }
        for (before, spec) in &extras.clamp_api_keys {
            self.rewrite_key(before, spec, Action::Clamp).await?;
        }
        Ok(())
    }

    /// Ersetzt die Spezifikation eines Objekts (Teil einer anderen Änderung) mit Audit.
    async fn rewrite<R: Resource>(
        &mut self,
        kind: ResourceKind,
        before: &Object<R>,
        spec: &R,
        action: Action,
    ) -> Result<(), StoreError> {
        let cols = self.resolve_cols(spec.to_columns()?).await?;
        self.update_row(kind, before.id, &before.metadata, cols)
            .await?;
        let after = Object {
            id: before.id,
            metadata: before.metadata.clone(),
            spec: spec.clone(),
        };
        self.audit(
            action,
            kind,
            Some(before.id),
            &before.metadata.name,
            Some(before),
            Some(&after),
        )
        .await
    }

    /// Wie [`AdminTx::rewrite`] für API-Keys (Status bleibt).
    async fn rewrite_key(
        &mut self,
        before: &ApiKey,
        spec: &ApiKeySpec,
        action: Action,
    ) -> Result<(), StoreError> {
        let cols = self.resolve_cols(spec.to_columns()?).await?;
        self.update_row(ResourceKind::ApiKey, before.id, &before.metadata, cols)
            .await?;
        let after = ApiKey {
            spec: spec.clone(),
            ..before.clone()
        };
        self.audit(
            action,
            ResourceKind::ApiKey,
            Some(before.id),
            &before.metadata.name,
            Some(before),
            Some(&after),
        )
        .await
    }
}
