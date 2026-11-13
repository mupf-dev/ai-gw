// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! API-Keys: Erzeugung (Klartext nur einmal), Lesen mit Status, Sperren.

use aigw_core::resource::{
    ApiKey, ApiKeySpec, ApiKeyState, ApiKeyStatus, CreatedApiKey, Id, ObjectMeta, ResourceKind,
    new_id, validate_name,
};
use aigw_core::snapshot::KeyHash;
use sqlx::any::AnyRow;
use time::OffsetDateTime;

use super::{Action, AdminTx, Col, Resource, Row, Val, millis, now};
use crate::keys::generate_api_key;
use crate::{Store, StoreError};

pub(crate) fn api_key_from_row(row: &AnyRow) -> Result<(ApiKey, KeyHash), StoreError> {
    let r = Row(row);
    let state = r.text("state")?;
    let hash = r.text("key_hash")?;
    let key = ApiKey {
        id: r.id()?,
        metadata: r.meta()?,
        spec: ApiKeySpec::from_row(&r)?,
        status: ApiKeyStatus {
            prefix: r.text("prefix")?,
            state: ApiKeyState::parse(&state)
                .ok_or_else(|| StoreError::Corrupt(format!("unbekannter Key-Zustand {state:?}")))?,
            last_used_at: r.opt_time("last_used_at")?,
        },
    };
    let hash = KeyHash::from_hex(&hash)
        .ok_or_else(|| StoreError::Corrupt("ungültiger Key-Hash".into()))?;
    Ok((key, hash))
}

impl Store {
    /// Erzeugt einen API-Key. Der Klartext steht nur im Ergebnis und wird nicht gespeichert.
    pub async fn create_api_key(
        &self,
        actor: &str,
        meta: ObjectMeta,
        spec: ApiKeySpec,
    ) -> Result<CreatedApiKey, StoreError> {
        validate_name(&meta.name).map_err(StoreError::Invalid)?;
        let generated = generate_api_key()?;
        let id = new_id();
        let mut tx = AdminTx::begin(self, actor).await?;
        let mut cols = spec.to_columns()?;
        cols.push(("prefix", Col::text(generated.prefix.clone())));
        cols.push(("key_hash", Col::text(generated.hash.to_hex())));
        cols.push(("state", Col::text(ApiKeyState::Active.as_str())));
        let cols = tx.resolve_cols(cols).await?;
        tx.insert_row(ResourceKind::ApiKey, id, &meta, cols).await?;
        let audit = ApiKey {
            id,
            metadata: meta,
            spec,
            status: ApiKeyStatus {
                prefix: generated.prefix.clone(),
                state: ApiKeyState::Active,
                last_used_at: None,
            },
        };
        tx.audit(
            Action::Create,
            ResourceKind::ApiKey,
            Some(id),
            &audit.metadata.name,
            None::<&()>,
            Some(&audit),
        )
        .await?;
        tx.commit().await?;
        Ok(CreatedApiKey {
            key: self.get_api_key_by_id(id).await?,
            secret: generated.secret,
        })
    }

    /// Liest einen API-Key (ohne Klartext/Hash).
    pub async fn get_api_key(&self, name: &str) -> Result<ApiKey, StoreError> {
        let row = self
            .fetch_one_row(ResourceKind::ApiKey, ApiKeySpec::REFS, name)
            .await?;
        Ok(api_key_from_row(&row)?.0)
    }

    async fn get_api_key_by_id(&self, id: Id) -> Result<ApiKey, StoreError> {
        let rows = self
            .fetch_rows(
                ResourceKind::ApiKey,
                ApiKeySpec::REFS,
                "WHERE t.id = $1",
                Some(id.to_string()),
            )
            .await?;
        let row = rows.first().ok_or_else(|| StoreError::NotFound {
            kind: ResourceKind::ApiKey,
            name: id.to_string(),
        })?;
        Ok(api_key_from_row(row)?.0)
    }

    /// Listet alle API-Keys.
    pub async fn list_api_keys(&self) -> Result<Vec<ApiKey>, StoreError> {
        self.fetch_rows(
            ResourceKind::ApiKey,
            ApiKeySpec::REFS,
            "ORDER BY t.name",
            None,
        )
        .await?
        .iter()
        .map(|r| api_key_from_row(r).map(|(k, _)| k))
        .collect()
    }

    /// Ändert Spezifikation und Labels eines Keys (Präfix/Hash bleiben).
    pub async fn update_api_key(
        &self,
        actor: &str,
        meta: ObjectMeta,
        spec: ApiKeySpec,
    ) -> Result<ApiKey, StoreError> {
        let before = self.get_api_key(&meta.name).await?;
        let mut tx = AdminTx::begin(self, actor).await?;
        let cols = tx.resolve_cols(spec.to_columns()?).await?;
        tx.update_row(ResourceKind::ApiKey, before.id, &meta, cols)
            .await?;
        let after = ApiKey {
            id: before.id,
            metadata: meta,
            spec,
            status: before.status.clone(),
        };
        tx.audit(
            Action::Update,
            ResourceKind::ApiKey,
            Some(before.id),
            &after.metadata.name,
            Some(&before),
            Some(&after),
        )
        .await?;
        tx.commit().await?;
        self.get_api_key_by_id(before.id).await
    }

    /// Sperrt einen Key.
    pub async fn revoke_api_key(&self, actor: &str, name: &str) -> Result<ApiKey, StoreError> {
        self.set_api_key_state(actor, name, ApiKeyState::Revoked)
            .await
    }

    /// Setzt den Zustand eines Keys (Sperren bzw. Entsperren, AIGW-28).
    pub async fn set_api_key_state(
        &self,
        actor: &str,
        name: &str,
        state: ApiKeyState,
    ) -> Result<ApiKey, StoreError> {
        let before = self.get_api_key(name).await?;
        let mut tx = AdminTx::begin(self, actor).await?;
        tx.update_row(
            ResourceKind::ApiKey,
            before.id,
            &before.metadata,
            vec![("state", Val::Text(Some(state.as_str().into())))],
        )
        .await?;
        let mut after = before.clone();
        after.status.state = state;
        tx.audit(
            Action::Update,
            ResourceKind::ApiKey,
            Some(before.id),
            name,
            Some(&before),
            Some(&after),
        )
        .await?;
        tx.commit().await?;
        self.get_api_key_by_id(before.id).await
    }

    /// Rotiert einen Key (AIGW-28): legt in **einer** Transaktion einen neuen Key `new_meta`
    /// mit derselben Spezifikation an und setzt beim alten Key den Ablauf auf
    /// `old_expires_at` (ein früherer, bereits gesetzter Ablauf bleibt). Der Klartext des neuen
    /// Keys steht nur im Ergebnis.
    pub async fn rotate_api_key(
        &self,
        actor: &str,
        name: &str,
        new_meta: ObjectMeta,
        old_expires_at: OffsetDateTime,
    ) -> Result<(CreatedApiKey, ApiKey), StoreError> {
        validate_name(&new_meta.name).map_err(StoreError::Invalid)?;
        let before = self.get_api_key(name).await?;
        let generated = generate_api_key()?;
        let new_id_ = new_id();
        let mut tx = AdminTx::begin(self, actor).await?;

        // Neuer Key mit der Spezifikation des alten.
        let spec = before.spec.clone();
        let mut cols = spec.to_columns()?;
        cols.push(("prefix", Col::text(generated.prefix.clone())));
        cols.push(("key_hash", Col::text(generated.hash.to_hex())));
        cols.push(("state", Col::text(ApiKeyState::Active.as_str())));
        let cols = tx.resolve_cols(cols).await?;
        tx.insert_row(ResourceKind::ApiKey, new_id_, &new_meta, cols)
            .await?;
        let created = ApiKey {
            id: new_id_,
            metadata: new_meta,
            spec,
            status: ApiKeyStatus {
                prefix: generated.prefix.clone(),
                state: ApiKeyState::Active,
                last_used_at: None,
            },
        };
        tx.audit(
            Action::Create,
            ResourceKind::ApiKey,
            Some(new_id_),
            &created.metadata.name,
            None::<&()>,
            Some(&created),
        )
        .await?;

        // Alter Key läuft ab.
        let expires = before
            .spec
            .expires_at
            .map_or(old_expires_at, |e| e.min(old_expires_at));
        let mut after = before.clone();
        after.spec.expires_at = Some(expires);
        let cols = tx.resolve_cols(after.spec.to_columns()?).await?;
        tx.update_row(ResourceKind::ApiKey, before.id, &before.metadata, cols)
            .await?;
        tx.audit(
            Action::Rotate,
            ResourceKind::ApiKey,
            Some(before.id),
            name,
            Some(&before),
            Some(&after),
        )
        .await?;
        tx.commit().await?;
        Ok((
            CreatedApiKey {
                key: self.get_api_key_by_id(new_id_).await?,
                secret: generated.secret,
            },
            self.get_api_key_by_id(before.id).await?,
        ))
    }

    /// Vermerkt die letzte Nutzung (vom Log-Writer, ohne Versionserhöhung/Audit).
    pub async fn touch_api_keys(&self, ids: &[Id]) -> Result<(), StoreError> {
        let ts = millis(now());
        for id in ids {
            sqlx::query("UPDATE api_key SET last_used_at = $1 WHERE id = $2")
                .bind(ts)
                .bind(id.to_string())
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }
}
