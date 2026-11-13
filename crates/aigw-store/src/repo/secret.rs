// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Allgemeine Geheimnisse (`Secret`, F149): wie Zugangsdaten verschlüsselt gespeichert
//! (AES-256-GCM mit dem Master-Key, gebunden an die ID), nie im Klartext ausgegeben.
//! `valueFrom`-Verweise je Feld werden wie bei Zugangsdaten (F54) für den originalgetreuen
//! GitOps-Export mitgespeichert.

use std::collections::{BTreeMap, HashMap};

use aigw_core::resource::{
    CredentialData, Id, ObjectMeta, ResourceKind, Secret, SecretInfo, SecretSpec, SecretString,
    new_id, validate_name,
};

use super::credential::{CredentialRefs, seal_data};

use super::{Action, AdminTx, Row, Val};
use crate::crypto::{Keyring, Sealed};
use crate::db::{DbRow, Q};
use crate::{Store, StoreError};

const NO_REFS: &[super::RefCol] = &[];
const KIND: ResourceKind = ResourceKind::Secret;
/// Höchstlänge der Beschreibung.
const MAX_DESCRIPTION: usize = 1024;

fn info_from_row(row: &DbRow) -> Result<(Secret, Sealed), StoreError> {
    let r = Row(row);
    let sealed = Sealed::from_bytes(
        r.text("key_id")?,
        &row.bytes("nonce")?,
        &row.bytes("ciphertext")?,
    );
    let obj = Secret {
        id: r.id()?,
        metadata: r.meta()?,
        spec: SecretInfo {
            description: r.opt_text("description")?,
            fields: r.json("fields")?,
            key_id: sealed.key_id.clone(),
        },
    };
    Ok((obj, sealed))
}

/// Gelesene Sicht (ohne Werte) aus einer Zeile.
pub(crate) fn secret_from_row(row: &DbRow) -> Result<Secret, StoreError> {
    Ok(info_from_row(row)?.0)
}

/// `valueFrom`-Verweise einer Eingabe.
pub(crate) fn refs_of(spec: &SecretSpec) -> CredentialRefs {
    spec.data
        .iter()
        .filter_map(|(f, r)| r.value_from.clone().map(|v| (f.clone(), v)))
        .collect()
}

/// Prüft Beschreibung und Feldnamen (gleiche Regeln wie Zugangsdaten beliebiger Anbieter).
pub fn validate_secret<'a>(
    description: Option<&str>,
    fields: impl IntoIterator<Item = &'a str>,
) -> Result<(), String> {
    if description.is_some_and(|d| d.len() > MAX_DESCRIPTION) {
        return Err(format!("description: höchstens {MAX_DESCRIPTION} Zeichen"));
    }
    let fields: Vec<&str> = fields.into_iter().collect();
    if fields.is_empty() {
        return Err("data: mindestens ein Feld angeben (z. B. `value`)".into());
    }
    for f in fields {
        let ok = !f.is_empty()
            && f.len() <= 64
            && f.bytes().next().is_some_and(|b| b.is_ascii_alphabetic())
            && f.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !ok {
            return Err(format!(
                "data: ungültiger Feldname {f:?} (Buchstabe, dann Buchstaben/Ziffern/_; max. 64)"
            ));
        }
    }
    Ok(())
}

pub(crate) fn sealed_cols(
    description: Option<&str>,
    fields: &[String],
    refs: &CredentialRefs,
    sealed: Sealed,
) -> Result<Vec<(&'static str, Val)>, StoreError> {
    let (nonce, ciphertext) = sealed.to_bytes()?;
    let refs: CredentialRefs = refs
        .iter()
        .filter(|(f, _)| fields.contains(f))
        .map(|(f, v)| (f.clone(), v.clone()))
        .collect();
    Ok(vec![
        ("description", Val::Text(description.map(str::to_owned))),
        ("fields", Val::json(&fields)?),
        ("refs", Val::json(&refs)?),
        ("key_id", Val::text(sealed.key_id)),
        ("nonce", Val::Bytes(Some(nonce))),
        ("ciphertext", Val::Bytes(Some(ciphertext))),
    ])
}

/// Schreibt ein (bereits aufgelöstes) Geheimnis in der Transaktion – für Admin-API und GitOps.
pub(crate) async fn write_secret(
    tx: &mut AdminTx,
    keyring: &Keyring,
    existing: Option<(Id, &serde_json::Value)>,
    meta: ObjectMeta,
    spec: &SecretSpec,
    refs: &CredentialRefs,
) -> Result<Secret, StoreError> {
    validate_secret(
        spec.description.as_deref(),
        spec.data.keys().map(String::as_str),
    )
    .map_err(StoreError::Invalid)?;
    let id = existing.map_or_else(new_id, |(id, _)| id);
    let (fields, sealed) = seal_data(keyring, id, &spec.data)?;
    let key_id = sealed.key_id.clone();
    let cols = sealed_cols(spec.description.as_deref(), &fields, refs, sealed)?;
    let action = match existing {
        Some(_) => {
            tx.update_row(KIND, id, &meta, cols).await?;
            Action::Update
        }
        None => {
            tx.insert_row(KIND, id, &meta, cols).await?;
            Action::Create
        }
    };
    let after = Secret {
        id,
        metadata: meta,
        spec: SecretInfo {
            description: spec.description.clone(),
            fields,
            key_id,
        },
    };
    tx.audit(
        action,
        KIND,
        Some(id),
        &after.metadata.name,
        existing.map(|(_, before)| before),
        Some(&after),
    )
    .await?;
    Ok(after)
}

impl Store {
    /// Legt ein Geheimnis an (Werte werden mit dem aktuellen Master-Key verschlüsselt).
    pub async fn create_secret(
        &self,
        actor: &str,
        keyring: &Keyring,
        meta: ObjectMeta,
        spec: SecretSpec,
    ) -> Result<Secret, StoreError> {
        validate_name(&meta.name).map_err(StoreError::Invalid)?;
        let mut tx = AdminTx::begin(self, actor).await?;
        let s = write_secret(&mut tx, keyring, None, meta, &spec, &refs_of(&spec)).await?;
        tx.commit().await?;
        self.get_secret(&s.metadata.name).await
    }

    /// Ersetzt Beschreibung, Labels und **alle** Werte eines Geheimnisses.
    pub async fn update_secret(
        &self,
        actor: &str,
        keyring: &Keyring,
        meta: ObjectMeta,
        spec: SecretSpec,
    ) -> Result<Secret, StoreError> {
        let before = self.get_secret(&meta.name).await?;
        let before_json = serde_json::to_value(&before)?;
        let mut tx = AdminTx::begin(self, actor).await?;
        write_secret(
            &mut tx,
            keyring,
            Some((before.id, &before_json)),
            meta,
            &spec,
            &refs_of(&spec),
        )
        .await?;
        tx.commit().await?;
        self.get_secret(&before.metadata.name).await
    }

    /// Ändert ein Geheimnis teilweise (Admin-API `PATCH`): `changes` setzt (`Some`) bzw.
    /// entfernt (`None`) Felder; Labels und Beschreibung nur, wenn angegeben
    /// (`description: Some(None)` entfernt sie).
    pub async fn patch_secret(
        &self,
        actor: &str,
        keyring: &Keyring,
        name: &str,
        labels: Option<BTreeMap<String, String>>,
        description: Option<Option<String>>,
        changes: BTreeMap<String, Option<SecretString>>,
    ) -> Result<Secret, StoreError> {
        let row = self.fetch_one_row(KIND, NO_REFS, name).await?;
        let (before, sealed) = info_from_row(&row)?;
        let mut refs: CredentialRefs = row.json("refs")?;
        let plain = keyring.open(before.id.as_bytes(), &sealed)?;
        let mut values: BTreeMap<String, String> = serde_json::from_slice(&plain)?;
        for (field, v) in changes {
            refs.remove(&field);
            match v {
                Some(v) => values.insert(field, v.expose().to_owned()),
                None => values.remove(&field),
            };
        }
        let mut meta = before.metadata.clone();
        if let Some(l) = labels {
            meta.labels = l;
        }
        let spec = SecretSpec {
            description: description.unwrap_or_else(|| before.spec.description.clone()),
            data: values
                .into_iter()
                .map(|(k, v)| (k, aigw_core::resource::SecretRef::value(v)))
                .collect(),
        };
        let before_json = serde_json::to_value(&before)?;
        let mut tx = AdminTx::begin(self, actor).await?;
        write_secret(
            &mut tx,
            keyring,
            Some((before.id, &before_json)),
            meta,
            &spec,
            &refs,
        )
        .await?;
        tx.commit().await?;
        self.get_secret(name).await
    }

    /// Liest ein Geheimnis (nur Metadaten und Feldnamen).
    pub async fn get_secret(&self, name: &str) -> Result<Secret, StoreError> {
        let row = self.fetch_one_row(KIND, NO_REFS, name).await?;
        Ok(info_from_row(&row)?.0)
    }

    /// Listet alle Geheimnisse (nur Metadaten und Feldnamen).
    pub async fn list_secrets(&self) -> Result<Vec<Secret>, StoreError> {
        self.fetch_rows(KIND, NO_REFS, "ORDER BY t.name", None)
            .await?
            .iter()
            .map(secret_from_row)
            .collect()
    }


    /// Gespeicherte `valueFrom`-Verweise aller Geheimnisse, Schlüssel: Name.
    pub async fn secret_refs(&self) -> Result<HashMap<String, CredentialRefs>, StoreError> {
        let rows = Q::new("SELECT name, refs FROM secret")
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|r| Ok((r.text("name")?, r.json("refs")?)))
            .collect()
    }

    /// Entschlüsselt alle Geheimnisse (für den Snapshot), Schlüssel: Name.
    pub async fn decrypt_secrets(
        &self,
        keyring: &Keyring,
    ) -> Result<HashMap<String, (Secret, CredentialData)>, StoreError> {
        let rows = self.fetch_rows(KIND, NO_REFS, "", None).await?;
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

    /// Verschlüsselt Geheimnisse mit altem Master-Key neu (Rotation).
    pub(crate) async fn rotate_secrets(
        &self,
        actor: &str,
        keyring: &Keyring,
    ) -> Result<usize, StoreError> {
        let rows = self
            .fetch_rows(
                KIND,
                NO_REFS,
                "WHERE t.key_id <> $1",
                Some(Val::text(keyring.primary_id())),
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
            let (nonce, ciphertext) = resealed.to_bytes()?;
            Q::new("UPDATE secret SET key_id = $1, nonce = $2, ciphertext = $3 WHERE id = $4")
                .bind(Val::text(resealed.key_id.clone()))
                .bind(Val::Bytes(Some(nonce)))
                .bind(Val::Bytes(Some(ciphertext)))
                .bind(Val::uuid(info.id))
                .execute(&mut tx.tx)
                .await?;
            let mut after = info.clone();
            after.spec.key_id = resealed.key_id;
            tx.audit(
                Action::Rotate,
                KIND,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_names_and_description() {
        assert!(validate_secret(None, ["value"]).is_ok());
        assert!(validate_secret(Some("HMAC für hooks"), ["value", "apiKey", "a_1"]).is_ok());
        assert!(validate_secret(None, []).is_err());
        assert!(validate_secret(None, ["1x"]).is_err());
        assert!(validate_secret(None, ["a-b"]).is_err());
        assert!(validate_secret(Some(&"x".repeat(2000)), ["value"]).is_err());
    }
}
