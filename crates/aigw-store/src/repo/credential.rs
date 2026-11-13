// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Zugangsdaten der Anbieter: verschlüsselt speichern, nie im Klartext ausgeben.

use std::collections::{BTreeMap, HashMap};

use aigw_core::resource::{
    CredentialData, Id, ObjectMeta, ProviderCredential, ProviderCredentialInfo,
    ProviderCredentialSpec, ResourceKind, SecretString, new_id, validate_name,
};
use aigw_core::types::ProviderKind;
use sqlx::any::AnyRow;

use super::{Action, AdminTx, Row, Val};
use crate::crypto::{Keyring, Sealed};
use crate::{Store, StoreError};

const NO_REFS: &[super::RefCol] = &[];

fn info_from_row(row: &AnyRow) -> Result<(ProviderCredential, Sealed), StoreError> {
    let r = Row(row);
    let p = r.text("provider")?;
    let sealed = Sealed {
        key_id: r.text("key_id")?,
        nonce: r.text("nonce")?,
        ciphertext: r.text("ciphertext")?,
    };
    let obj = ProviderCredential {
        id: r.id()?,
        metadata: r.meta()?,
        spec: ProviderCredentialInfo {
            provider: ProviderKind::parse(&p)
                .ok_or_else(|| StoreError::Corrupt(format!("unbekannter Anbieter {p:?}")))?,
            fields: r.json("fields")?,
            key_id: sealed.key_id.clone(),
        },
    };
    Ok((obj, sealed))
}

/// Löst die Werte auf (Klartext oder `valueFrom`) und verschlüsselt sie, gebunden an `id`.
pub(crate) fn seal_spec(
    keyring: &Keyring,
    id: Id,
    spec: &ProviderCredentialSpec,
) -> Result<(Vec<String>, Sealed), StoreError> {
    let mut plain = BTreeMap::new();
    for (field, r) in &spec.data {
        let v = r
            .resolve()
            .map_err(|e| StoreError::Invalid(format!("Feld {field:?}: {e}")))?;
        plain.insert(field.clone(), v.expose().to_owned());
    }
    let json = serde_json::to_vec(&plain)?;
    let sealed = keyring.seal(id.as_bytes(), &json)?;
    Ok((plain.into_keys().collect(), sealed))
}

pub(crate) fn sealed_cols(
    spec: &ProviderCredentialSpec,
    fields: &[String],
    sealed: Sealed,
) -> Result<Vec<(&'static str, Val)>, StoreError> {
    Ok(vec![
        (
            "provider",
            Val::Text(Some(spec.provider.as_str().to_owned())),
        ),
        ("fields", Val::Text(Some(serde_json::to_string(fields)?))),
        ("key_id", Val::Text(Some(sealed.key_id))),
        ("nonce", Val::Text(Some(sealed.nonce))),
        ("ciphertext", Val::Text(Some(sealed.ciphertext))),
    ])
}

impl Store {
    /// Legt Zugangsdaten an (Werte werden mit dem aktuellen Master-Key verschlüsselt).
    pub async fn create_credential(
        &self,
        actor: &str,
        keyring: &Keyring,
        meta: ObjectMeta,
        spec: ProviderCredentialSpec,
    ) -> Result<ProviderCredential, StoreError> {
        validate_name(&meta.name).map_err(StoreError::Invalid)?;
        let id = new_id();
        let (fields, sealed) = seal_spec(keyring, id, &spec)?;
        let mut tx = AdminTx::begin(self, actor).await?;
        tx.insert_row(
            ResourceKind::ProviderCredential,
            id,
            &meta,
            sealed_cols(&spec, &fields, sealed)?,
        )
        .await?;
        let after = ProviderCredential {
            id,
            metadata: meta,
            spec: ProviderCredentialInfo {
                provider: spec.provider,
                fields,
                key_id: keyring.primary_id().to_owned(),
            },
        };
        tx.audit(
            Action::Create,
            ResourceKind::ProviderCredential,
            Some(id),
            &after.metadata.name,
            None::<&()>,
            Some(&after),
        )
        .await?;
        tx.commit().await?;
        self.get_credential(&after.metadata.name).await
    }

    /// Ersetzt die Werte von Zugangsdaten vollständig.
    pub async fn update_credential(
        &self,
        actor: &str,
        keyring: &Keyring,
        meta: ObjectMeta,
        spec: ProviderCredentialSpec,
    ) -> Result<ProviderCredential, StoreError> {
        let before = self.get_credential(&meta.name).await?;
        let (fields, sealed) = seal_spec(keyring, before.id, &spec)?;
        let mut tx = AdminTx::begin(self, actor).await?;
        tx.update_row(
            ResourceKind::ProviderCredential,
            before.id,
            &meta,
            sealed_cols(&spec, &fields, sealed)?,
        )
        .await?;
        let after = ProviderCredential {
            id: before.id,
            metadata: meta,
            spec: ProviderCredentialInfo {
                provider: spec.provider,
                fields,
                key_id: keyring.primary_id().to_owned(),
            },
        };
        tx.audit(
            Action::Update,
            ResourceKind::ProviderCredential,
            Some(before.id),
            &after.metadata.name,
            Some(&before),
            Some(&after),
        )
        .await?;
        tx.commit().await?;
        self.get_credential(&after.metadata.name).await
    }

    /// Liest Zugangsdaten (nur Metadaten und Feldnamen).
    pub async fn get_credential(&self, name: &str) -> Result<ProviderCredential, StoreError> {
        let row = self
            .fetch_one_row(ResourceKind::ProviderCredential, NO_REFS, name)
            .await?;
        Ok(info_from_row(&row)?.0)
    }

    /// Listet Zugangsdaten (nur Metadaten und Feldnamen).
    pub async fn list_credentials(&self) -> Result<Vec<ProviderCredential>, StoreError> {
        self.fetch_rows(
            ResourceKind::ProviderCredential,
            NO_REFS,
            "ORDER BY t.name",
            None,
        )
        .await?
        .iter()
        .map(|r| info_from_row(r).map(|(c, _)| c))
        .collect()
    }

    /// Entschlüsselt alle Zugangsdaten (für den Snapshot), Schlüssel: Name.
    pub async fn decrypt_credentials(
        &self,
        keyring: &Keyring,
    ) -> Result<HashMap<String, (ProviderCredential, CredentialData)>, StoreError> {
        let rows = self
            .fetch_rows(ResourceKind::ProviderCredential, NO_REFS, "", None)
            .await?;
        let mut out = HashMap::with_capacity(rows.len());
        for row in &rows {
            let (info, sealed) = info_from_row(row)?;
            let plain = keyring.open(info.id.as_bytes(), &sealed)?;
            let map: BTreeMap<String, String> = serde_json::from_slice(&plain)?;
            let data = CredentialData(
                map.into_iter()
                    .map(|(k, v)| (k, SecretString::new(v)))
                    .collect(),
            );
            out.insert(info.metadata.name.clone(), (info, data));
        }
        Ok(out)
    }

    /// Verschlüsselt alle Zugangsdaten, die nicht mit dem aktuellen Key verschlüsselt sind,
    /// neu (Rotation). Gibt die Anzahl umgeschlüsselter Datensätze zurück.
    pub async fn rotate_credentials(
        &self,
        actor: &str,
        keyring: &Keyring,
    ) -> Result<usize, StoreError> {
        let rows = self
            .fetch_rows(
                ResourceKind::ProviderCredential,
                NO_REFS,
                "WHERE t.key_id <> $1",
                Some(keyring.primary_id().to_owned()),
            )
            .await?;
        if rows.is_empty() {
            return Ok(0);
        }
        let mut tx = AdminTx::begin(self, actor).await?;
        for row in &rows {
            let (info, sealed) = info_from_row(row)?;
            let plain = keyring.open(info.id.as_bytes(), &sealed)?;
            let resealed = keyring.seal(info.id.as_bytes(), &plain)?;
            sqlx::query(
                "UPDATE provider_credential SET key_id = $1, nonce = $2, ciphertext = $3 WHERE id = $4",
            )
            .bind(resealed.key_id.clone())
            .bind(resealed.nonce)
            .bind(resealed.ciphertext)
            .bind(info.id.to_string())
            .execute(&mut *tx.tx)
            .await?;
            let mut after = info.clone();
            after.spec.key_id = resealed.key_id;
            tx.audit(
                Action::Rotate,
                ResourceKind::ProviderCredential,
                Some(info.id),
                &info.metadata.name,
                Some(&info),
                Some(&after),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(rows.len())
    }
}
