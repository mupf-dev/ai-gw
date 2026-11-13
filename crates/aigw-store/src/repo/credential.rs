// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Zugangsdaten der Anbieter: verschlüsselt speichern, nie im Klartext ausgeben.
//!
//! F54: Zu jedem Feld, dessen Wert aus einem `valueFrom`-Verweis kam (`env`/`file`), wird der
//! Verweis selbst unverschlüsselt mitgespeichert (Spalte `refs`, nur Namen, keine Werte). Der
//! GitOps-Export gibt diese Verweise originalgetreu aus; Felder ohne Verweis (Klartext über die
//! Admin-API) erscheinen dort weiter als Platzhalter `AIGW_SECRET_…`.

use std::collections::{BTreeMap, HashMap};

use aigw_core::resource::{
    CredentialData, Id, ObjectMeta, ProviderCredential, ProviderCredentialInfo,
    ProviderCredentialSpec, ResourceKind, SecretString, ValueFrom, new_id, validate_name,
};
use aigw_core::types::ProviderKind;

use super::{Action, AdminTx, Row, Val};
use crate::crypto::{Keyring, Sealed};
use crate::db::{DbRow, Q};
use crate::{Store, StoreError};

const NO_REFS: &[super::RefCol] = &[];

/// `valueFrom`-Verweise je Feld (F54).
pub(crate) type CredentialRefs = BTreeMap<String, ValueFrom>;

fn info_from_row(row: &DbRow) -> Result<(ProviderCredential, Sealed), StoreError> {
    let r = Row(row);
    let p = r.text("provider")?;
    let sealed = Sealed::from_bytes(
        r.text("key_id")?,
        &row.bytes("nonce")?,
        &row.bytes("ciphertext")?,
    );
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

/// Gelesene Sicht (ohne Werte) aus einer Zeile.
pub(crate) fn credential_from_row(row: &DbRow) -> Result<ProviderCredential, StoreError> {
    Ok(info_from_row(row)?.0)
}

/// `valueFrom`-Verweise einer Eingabe (Felder mit Klartext fehlen).
pub(crate) fn refs_of(spec: &ProviderCredentialSpec) -> CredentialRefs {
    spec.data
        .iter()
        .filter_map(|(f, r)| r.value_from.clone().map(|v| (f.clone(), v)))
        .collect()
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
    provider: ProviderKind,
    fields: &[String],
    refs: &CredentialRefs,
    sealed: Sealed,
) -> Result<Vec<(&'static str, Val)>, StoreError> {
    let (nonce, ciphertext) = sealed.to_bytes()?;
    // Nur Verweise auf vorhandene Felder.
    let refs: CredentialRefs = refs
        .iter()
        .filter(|(f, _)| fields.contains(f))
        .map(|(f, v)| (f.clone(), v.clone()))
        .collect();
    Ok(vec![
        ("provider", Val::text(provider.as_str())),
        ("fields", Val::json(&fields)?),
        ("refs", Val::json(&refs)?),
        ("key_id", Val::text(sealed.key_id)),
        ("nonce", Val::Bytes(Some(nonce))),
        ("ciphertext", Val::Bytes(Some(ciphertext))),
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
            sealed_cols(spec.provider, &fields, &refs_of(&spec), sealed)?,
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
            sealed_cols(spec.provider, &fields, &refs_of(&spec), sealed)?,
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

    /// Ändert Zugangsdaten teilweise (Admin-API `PATCH`): `changes` setzt (`Some`) bzw.
    /// entfernt (`None`) einzelne Felder, nicht genannte Felder behalten ihren Wert. Labels
    /// und Anbieter werden nur ersetzt, wenn angegeben. Die Werte werden entschlüsselt,
    /// zusammengeführt und mit dem aktuellen Master-Key neu verschlüsselt.
    pub async fn patch_credential(
        &self,
        actor: &str,
        keyring: &Keyring,
        name: &str,
        labels: Option<BTreeMap<String, String>>,
        provider: Option<ProviderKind>,
        changes: BTreeMap<String, Option<SecretString>>,
    ) -> Result<ProviderCredential, StoreError> {
        let row = self
            .fetch_one_row(ResourceKind::ProviderCredential, NO_REFS, name)
            .await?;
        let (before, sealed) = info_from_row(&row)?;
        // Verweise unveränderter Felder bleiben; geänderte Felder sind jetzt Klartext.
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
        if values.is_empty() {
            return Err(StoreError::Invalid(
                "Zugangsdaten brauchen mindestens ein Feld".into(),
            ));
        }
        let provider = provider.unwrap_or(before.spec.provider);
        let mut meta = before.metadata.clone();
        if let Some(l) = labels {
            meta.labels = l;
        }
        let json = serde_json::to_vec(&values)?;
        let sealed = keyring.seal(before.id.as_bytes(), &json)?;
        let fields: Vec<String> = values.into_keys().collect();
        let mut tx = AdminTx::begin(self, actor).await?;
        tx.update_row(
            ResourceKind::ProviderCredential,
            before.id,
            &meta,
            sealed_cols(provider, &fields, &refs, sealed)?,
        )
        .await?;
        let after = ProviderCredential {
            id: before.id,
            metadata: meta,
            spec: ProviderCredentialInfo {
                provider,
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

    /// Gespeicherte `valueFrom`-Verweise aller Zugangsdaten (F54), Schlüssel: Name.
    pub async fn credential_refs(&self) -> Result<HashMap<String, CredentialRefs>, StoreError> {
        let rows = Q::new("SELECT name, refs FROM provider_credential")
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|r| Ok((r.text("name")?, r.json("refs")?)))
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
            Q::new(
                "UPDATE provider_credential SET key_id = $1, nonce = $2, ciphertext = $3 WHERE id = $4",
            )
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
