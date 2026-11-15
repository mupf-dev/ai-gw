// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! WASM-Module (AIGW-63, Migration 0009): Komponenten für Plugins der Art `wasm`.
//!
//! Module sind **keine** generische Ressource (kein YAML, kein GitOps-Inhalt): der Inhalt ist
//! binär, und GitOps verweist nur per Name (+ optional SHA-256) darauf. Trotzdem gehören sie
//! zum Snapshot – jede Änderung läuft als Admin-Transaktion (`config_version` + 1) mit einem
//! Audit-Eintrag der Art [`AUDIT_KIND`] und `object_id`, damit der inkrementelle Neubau sie
//! über das Journal findet. Der Audit-Eintrag enthält nur Metadaten (Name, SHA-256, Größe),
//! nie den Inhalt.
//!
//! Signaturen (AIGW-101, Migration 0011): optional Ed25519-Signatur + Schlüssel-ID je Modul.
//! Der Store prüft nur das Format; ob eine Signatur gültig bzw. Pflicht ist, entscheidet die
//! [`aigw_core::wasm::TrustPolicy`] der Aufrufer (Admin-API, Startkonfiguration) und der
//! Laufzeit (vor dem Kompilieren).

use aigw_core::resource::{Id, PluginBindingSpec, new_id};
use aigw_core::wasm::{ModuleSignature, WasmModule};
use bytes::Bytes;
use serde::Serialize;
use time::OffsetDateTime;

use crate::db::{DbRow, Q, Val};
use crate::repo::{Action, AdminTx, now};
use crate::{Store, StoreError};

/// Art der Audit-Einträge (und des Journals) für WASM-Module.
pub const AUDIT_KIND: &str = "wasm-module";

/// Akteur für Module aus der Startkonfiguration (`wasm.modules`).
pub const STARTUP_ACTOR: &str = "startup-config";

/// SHA-256 als Hex (klein).
pub fn sha256_hex(content: &[u8]) -> String {
    aigw_core::wasm::sha256_hex(content)
}

/// Metadaten eines Moduls (ohne Inhalt).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WasmModuleInfo {
    /// ID.
    pub id: Id,
    /// Name.
    pub name: String,
    /// SHA-256 (hex).
    pub sha256: String,
    /// Größe in Byte.
    pub size: u64,
    /// Beschreibung.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Hochgeladen von.
    pub created_by: String,
    /// Angelegt.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Zuletzt ersetzt.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    /// Signatur (AIGW-101); fehlt = unsigniert.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<ModuleSignature>,
}

/// Neues bzw. ersetzendes Modul.
#[derive(Debug, Clone)]
pub struct NewWasmModule {
    /// Name.
    pub name: String,
    /// Beschreibung.
    pub description: Option<String>,
    /// Inhalt.
    pub content: Vec<u8>,
    /// Erwarteter SHA-256 des Inhalts (Integrität des Uploads); Abweichung = Fehler.
    pub sha256: Option<String>,
    /// Ed25519-Signatur (AIGW-101); vom Aufrufer gegen die Richtlinie geprüft.
    pub signature: Option<ModuleSignature>,
}

/// Ergebnis von [`Store::put_wasm_module`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutOutcome {
    /// Neu angelegt.
    Created(WasmModuleInfo),
    /// Inhalt oder Beschreibung ersetzt.
    Replaced(WasmModuleInfo),
    /// Gleicher Inhalt und gleiche Beschreibung – nichts geschrieben (keine neue Version).
    Unchanged(WasmModuleInfo),
}

impl PutOutcome {
    /// Metadaten des Moduls.
    pub fn info(&self) -> &WasmModuleInfo {
        match self {
            Self::Created(i) | Self::Replaced(i) | Self::Unchanged(i) => i,
        }
    }
}

const META_COLS: &str = "id, name, sha256, size, description, created_by, created_at, updated_at, \
                         signature, signature_key_id";

fn info_from_row(row: &DbRow) -> Result<WasmModuleInfo, StoreError> {
    Ok(WasmModuleInfo {
        id: row.uuid("id")?,
        name: row.text("name")?,
        sha256: row.text("sha256")?,
        size: u64::try_from(row.i64("size")?).unwrap_or(0),
        description: row.opt_text("description")?,
        created_by: row.text("created_by")?,
        created_at: row.time("created_at")?,
        updated_at: row.time("updated_at")?,
        signature: match (row.opt_text("signature_key_id")?, row.opt_text("signature")?) {
            (Some(key_id), Some(signature)) => Some(ModuleSignature { key_id, signature }),
            _ => None,
        },
    })
}

fn module_from_row(row: &DbRow) -> Result<WasmModule, StoreError> {
    let info = info_from_row(row)?;
    Ok(WasmModule {
        id: info.id,
        name: info.name,
        sha256: info.sha256,
        description: info.description,
        created_by: info.created_by,
        created_at: info.created_at,
        updated_at: info.updated_at,
        content: Bytes::from(row.bytes("content")?),
        signature: info.signature,
    })
}

/// Name des Moduls, auf das eine Bindung der Art `wasm` verweist.
pub fn binding_module(spec: &PluginBindingSpec) -> Option<&str> {
    (spec.plugin == "wasm")
        .then(|| {
            spec.config
                .get("module")
                .and_then(serde_json::Value::as_str)
        })
        .flatten()
}

impl Store {
    /// Alle Module (Metadaten), nach Name sortiert.
    pub async fn list_wasm_modules(&self) -> Result<Vec<WasmModuleInfo>, StoreError> {
        Q::new(format!("SELECT {META_COLS} FROM wasm_module ORDER BY name"))
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(info_from_row)
            .collect()
    }

    /// Metadaten eines Moduls.
    pub async fn wasm_module_info(&self, name: &str) -> Result<Option<WasmModuleInfo>, StoreError> {
        Q::new(format!(
            "SELECT {META_COLS} FROM wasm_module WHERE name = $1"
        ))
        .bind(Val::text(name))
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(info_from_row)
        .transpose()
    }

    /// Modul mit Inhalt.
    pub async fn wasm_module(&self, name: &str) -> Result<Option<WasmModule>, StoreError> {
        Q::new(format!(
            "SELECT {META_COLS}, content FROM wasm_module WHERE name = $1"
        ))
        .bind(Val::text(name))
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(module_from_row)
        .transpose()
    }

    /// Alle Module mit Inhalt (Snapshot-Aufbau).
    pub(crate) async fn all_wasm_modules(&self) -> Result<Vec<WasmModule>, StoreError> {
        Q::new(format!("SELECT {META_COLS}, content FROM wasm_module"))
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(module_from_row)
            .collect()
    }

    /// Module nach ID (inkrementeller Snapshot); fehlende sind gelöscht.
    pub(crate) async fn wasm_modules_by_id(
        &self,
        ids: &[Id],
    ) -> Result<Vec<WasmModule>, StoreError> {
        let mut out = Vec::new();
        for chunk in ids.chunks(200) {
            let mut q = Q::new(String::new());
            let list = q.list(chunk.iter().map(|id| Val::uuid(*id)));
            q.sql = format!("SELECT {META_COLS}, content FROM wasm_module WHERE id IN ({list})");
            for row in q.fetch_all(&self.pool).await? {
                out.push(module_from_row(&row)?);
            }
        }
        Ok(out)
    }

    /// Legt ein Modul an oder ersetzt es (gleicher Name). Prüft Name, Größe (`max_bytes`),
    /// Komponenten-Format und – falls angegeben – den SHA-256. Schreibt einen Audit-Eintrag
    /// (ohne Inhalt) und erhöht `config_version`; unveränderte Module schreiben nichts.
    pub async fn put_wasm_module(
        &self,
        actor: &str,
        m: NewWasmModule,
        max_bytes: u64,
    ) -> Result<PutOutcome, StoreError> {
        aigw_core::resource::validate_name(&m.name).map_err(StoreError::Invalid)?;
        if m.content.is_empty() || m.content.len() as u64 > max_bytes {
            return Err(StoreError::Invalid(format!(
                "Modul: 1–{max_bytes} Byte erlaubt (erhalten: {})",
                m.content.len()
            )));
        }
        if !aigw_core::wasm::looks_like_component(&m.content) {
            return Err(StoreError::Invalid(
                "Modul ist keine WebAssembly-Komponente (Component Model); Kern-Module bitte \
                 mit `wasm-tools component new` bzw. Ziel wasm32-wasip2 bauen"
                    .into(),
            ));
        }
        if let Some(d) = &m.description
            && d.len() > 1024
        {
            return Err(StoreError::Invalid(
                "description: höchstens 1024 Zeichen".into(),
            ));
        }
        let sha = sha256_hex(&m.content);
        if let Some(expected) = &m.sha256 {
            let expected =
                aigw_core::wasm::normalize_sha256(expected).map_err(StoreError::Invalid)?;
            if expected != sha {
                return Err(StoreError::Invalid(format!(
                    "sha256 stimmt nicht: erwartet {expected}, Inhalt hat {sha}"
                )));
            }
        }
        let existing = self.wasm_module_info(&m.name).await?;
        if let Some(old) = &existing
            && old.sha256 == sha
            && old.description == m.description
            && old.signature == m.signature
        {
            return Ok(PutOutcome::Unchanged(old.clone()));
        }
        let mut tx = AdminTx::begin(self, actor).await?;
        let t = now();
        let size = i64::try_from(m.content.len()).unwrap_or(i64::MAX);
        let outcome = match existing {
            Some(old) => {
                Q::new(
                    "UPDATE wasm_module SET sha256 = $1, size = $2, description = $3, \
                     content = $4, updated_at = $5, signature = $6, signature_key_id = $7 \
                     WHERE id = $8",
                )
                .bind(Val::text(sha.clone()))
                .bind(Val::int(size))
                .bind(Val::Text(m.description.clone()))
                .bind(Val::Bytes(Some(m.content)))
                .bind(Val::time(t))
                .bind(Val::Text(m.signature.as_ref().map(|s| s.signature.clone())))
                .bind(Val::Text(m.signature.as_ref().map(|s| s.key_id.clone())))
                .bind(Val::uuid(old.id))
                .execute(&mut tx.tx)
                .await?;
                let new = WasmModuleInfo {
                    sha256: sha,
                    size: size as u64,
                    description: m.description,
                    updated_at: t,
                    signature: m.signature,
                    ..old.clone()
                };
                tx.audit_kind(
                    Action::Update,
                    AUDIT_KIND,
                    Some(old.id),
                    &old.name,
                    Some(&old),
                    Some(&new),
                )
                .await?;
                PutOutcome::Replaced(new)
            }
            None => {
                let info = WasmModuleInfo {
                    id: new_id(),
                    name: m.name,
                    sha256: sha,
                    size: size as u64,
                    description: m.description,
                    created_by: actor.to_owned(),
                    created_at: t,
                    updated_at: t,
                    signature: m.signature,
                };
                Q::new(
                    "INSERT INTO wasm_module (id, name, sha256, size, description, content, \
                     created_by, created_at, updated_at, signature, signature_key_id) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
                )
                .bind(Val::uuid(info.id))
                .bind(Val::text(info.name.clone()))
                .bind(Val::text(info.sha256.clone()))
                .bind(Val::int(size))
                .bind(Val::Text(info.description.clone()))
                .bind(Val::Bytes(Some(m.content)))
                .bind(Val::text(info.created_by.clone()))
                .bind(Val::time(t))
                .bind(Val::time(t))
                .bind(Val::Text(
                    info.signature.as_ref().map(|s| s.signature.clone()),
                ))
                .bind(Val::Text(info.signature.as_ref().map(|s| s.key_id.clone())))
                .execute(&mut tx.tx)
                .await
                .map_err(|e| crate::repo::map_db(e, "WASM-Modul"))?;
                tx.audit_kind(
                    Action::Create,
                    AUDIT_KIND,
                    Some(info.id),
                    &info.name,
                    None::<&()>,
                    Some(&info),
                )
                .await?;
                PutOutcome::Created(info)
            }
        };
        tx.commit().await?;
        Ok(outcome)
    }

    /// Plugin-Bindungen (Namen), die auf das Modul `name` verweisen.
    pub async fn wasm_module_users(&self, name: &str) -> Result<Vec<String>, StoreError> {
        let mut out: Vec<String> = self
            .list::<PluginBindingSpec>()
            .await?
            .into_iter()
            .filter(|b| binding_module(&b.spec) == Some(name))
            .map(|b| b.metadata.name)
            .collect();
        out.sort();
        Ok(out)
    }

    /// Löscht ein Modul. `Ok(None)`: unbekannt. Verweisen noch Bindungen darauf, ist das ein
    /// [`StoreError::Conflict`] (erst die Bindungen entfernen).
    pub async fn delete_wasm_module(
        &self,
        actor: &str,
        name: &str,
    ) -> Result<Option<WasmModuleInfo>, StoreError> {
        let Some(info) = self.wasm_module_info(name).await? else {
            return Ok(None);
        };
        let users = self.wasm_module_users(name).await?;
        if !users.is_empty() {
            return Err(StoreError::Conflict(format!(
                "WASM-Modul {name:?} wird noch von Plugin-Bindungen verwendet: {}",
                users.join(", ")
            )));
        }
        let mut tx = AdminTx::begin(self, actor).await?;
        let n = Q::new("DELETE FROM wasm_module WHERE id = $1")
            .bind(Val::uuid(info.id))
            .execute(&mut tx.tx)
            .await?;
        if n == 0 {
            return Ok(None);
        }
        tx.audit_kind(
            Action::Delete,
            AUDIT_KIND,
            Some(info.id),
            &info.name,
            Some(&info),
            None::<&()>,
        )
        .await?;
        tx.commit().await?;
        Ok(Some(info))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha_hex() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
