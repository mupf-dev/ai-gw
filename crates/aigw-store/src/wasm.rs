// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

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
//!
//! Sigstore und Vier-Augen-Freigabe (AIGW-106, Migration 0014): optional ein Sigstore-Bundle
//! (JSON) je Modul; mit `wasm.four_eyes` landet ein Upload zuerst in `wasm_module_pending`
//! ([`Store::submit_wasm_module`], Audit-Art [`APPROVAL_AUDIT_KIND`], kein Snapshot-Objekt) und
//! wird erst mit [`Store::approve_wasm_module`] aktiv (Audit `wasm-module` mit `object_id`,
//! Journal). Wer freigeben darf, prüft die Admin-API.

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

/// Art der Audit-Einträge für Einreichen/Freigeben/Verwerfen (AIGW-106; nicht im Snapshot).
pub const APPROVAL_AUDIT_KIND: &str = "wasm-module-approval";

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
    /// Sigstore-Bundle (JSON, AIGW-106).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sigstore_bundle: Option<String>,
    /// Hochgeladen von (aktuelle Version; ältere Zeilen: `None` = `created_by`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uploaded_by: Option<String>,
    /// Freigegeben von (Vier-Augen-Freigabe, AIGW-106); `None` = ohne Freigabe aktiv.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_by: Option<String>,
    /// Zeitpunkt der Freigabe.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub approved_at: Option<OffsetDateTime>,
}

/// Eingereichte, noch nicht freigegebene Version eines Moduls (AIGW-106).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingWasmModuleInfo {
    /// ID der Einreichung.
    pub id: Id,
    /// Name des Moduls.
    pub name: String,
    /// SHA-256 (hex).
    pub sha256: String,
    /// Größe in Byte.
    pub size: u64,
    /// Beschreibung.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Hochgeladen von (Principal).
    pub uploaded_by: String,
    /// Person hinter dem Hochlader (Admin-Keys → anlegender Nutzer), beim Upload ermittelt.
    pub uploader_person: String,
    /// Hochgeladen am.
    #[serde(with = "time::serde::rfc3339")]
    pub uploaded_at: OffsetDateTime,
    /// Ed25519-Signatur.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<ModuleSignature>,
    /// Sigstore-Bundle (JSON).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sigstore_bundle: Option<String>,
}

/// Ergebnis von [`Store::submit_wasm_module`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// Eingereicht (wartet auf Freigabe); `replaced`: eine ältere Einreichung wurde ersetzt.
    Pending {
        /// Die Einreichung.
        info: PendingWasmModuleInfo,
        /// Ältere Einreichung ersetzt.
        replaced: bool,
    },
    /// Gleich der aktiven Version – nichts eingereicht.
    Unchanged(WasmModuleInfo),
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
    /// Sigstore-Bundle (JSON, AIGW-106); der Store prüft nur das Format.
    pub sigstore_bundle: Option<String>,
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
                         signature, signature_key_id, sigstore_bundle, uploaded_by, approved_by, \
                         approved_at";

const PENDING_COLS: &str = "id, name, sha256, size, description, signature, signature_key_id, \
                            sigstore_bundle, uploaded_by, uploader_person, uploaded_at";

fn signature_from_row(row: &DbRow) -> Result<Option<ModuleSignature>, StoreError> {
    Ok(
        match (
            row.opt_text("signature_key_id")?,
            row.opt_text("signature")?,
        ) {
            (Some(key_id), Some(signature)) => Some(ModuleSignature { key_id, signature }),
            _ => None,
        },
    )
}

fn pending_from_row(row: &DbRow) -> Result<PendingWasmModuleInfo, StoreError> {
    Ok(PendingWasmModuleInfo {
        id: row.uuid("id")?,
        name: row.text("name")?,
        sha256: row.text("sha256")?,
        size: u64::try_from(row.i64("size")?).unwrap_or(0),
        description: row.opt_text("description")?,
        uploaded_by: row.text("uploaded_by")?,
        uploader_person: row.text("uploader_person")?,
        uploaded_at: row.time("uploaded_at")?,
        signature: signature_from_row(row)?,
        sigstore_bundle: row.opt_text("sigstore_bundle")?,
    })
}

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
        signature: signature_from_row(row)?,
        sigstore_bundle: row.opt_text("sigstore_bundle")?,
        uploaded_by: row.opt_text("uploaded_by")?,
        approved_by: row.opt_text("approved_by")?,
        approved_at: row.opt_time("approved_at")?,
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
        sigstore_bundle: info.sigstore_bundle,
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

/// Freigabe einer Version (Vier-Augen-Prinzip).
struct Approval {
    uploaded_by: String,
    approved_by: String,
    at: OffsetDateTime,
}

/// Prüft ein neues Modul und liefert den SHA-256 des Inhalts.
fn validate_new(m: &NewWasmModule, max_bytes: u64) -> Result<String, StoreError> {
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
    if let Some(b) = &m.sigstore_bundle {
        aigw_core::wasm::check_bundle_format(b).map_err(StoreError::Invalid)?;
    }
    let sha = sha256_hex(&m.content);
    if let Some(expected) = &m.sha256 {
        let expected = aigw_core::wasm::normalize_sha256(expected).map_err(StoreError::Invalid)?;
        if expected != sha {
            return Err(StoreError::Invalid(format!(
                "sha256 stimmt nicht: erwartet {expected}, Inhalt hat {sha}"
            )));
        }
    }
    Ok(sha)
}

/// Gleiche Version (Inhalt, Beschreibung, Signaturen) wie die gespeicherte?
fn same_version(old: &WasmModuleInfo, sha: &str, m: &NewWasmModule) -> bool {
    old.sha256 == sha
        && old.description == m.description
        && old.signature == m.signature
        && old.sigstore_bundle == m.sigstore_bundle
}

/// Schreibt die aktive Version (Anlegen oder Ersetzen) samt Audit-Eintrag `wasm-module`.
async fn write_active(
    tx: &mut AdminTx,
    existing: Option<WasmModuleInfo>,
    m: NewWasmModule,
    sha: String,
    actor: &str,
    approval: Option<Approval>,
) -> Result<PutOutcome, StoreError> {
    let t = now();
    let size = i64::try_from(m.content.len()).unwrap_or(i64::MAX);
    let uploaded_by = approval
        .as_ref()
        .map_or_else(|| actor.to_owned(), |a| a.uploaded_by.clone());
    let approved_by = approval.as_ref().map(|a| a.approved_by.clone());
    let approved_at = approval.as_ref().map(|a| a.at);
    match existing {
        Some(old) => {
            Q::new(
                "UPDATE wasm_module SET sha256 = $1, size = $2, description = $3, \
                 content = $4, updated_at = $5, signature = $6, signature_key_id = $7, \
                 sigstore_bundle = $8, uploaded_by = $9, approved_by = $10, approved_at = $11 \
                 WHERE id = $12",
            )
            .bind(Val::text(sha.clone()))
            .bind(Val::int(size))
            .bind(Val::Text(m.description.clone()))
            .bind(Val::Bytes(Some(m.content)))
            .bind(Val::time(t))
            .bind(Val::Text(m.signature.as_ref().map(|s| s.signature.clone())))
            .bind(Val::Text(m.signature.as_ref().map(|s| s.key_id.clone())))
            .bind(Val::Text(m.sigstore_bundle.clone()))
            .bind(Val::text(uploaded_by.clone()))
            .bind(Val::Text(approved_by.clone()))
            .bind(Val::opt_time(approved_at))
            .bind(Val::uuid(old.id))
            .execute(&mut tx.tx)
            .await?;
            let new = WasmModuleInfo {
                sha256: sha,
                size: size as u64,
                description: m.description,
                updated_at: t,
                signature: m.signature,
                sigstore_bundle: m.sigstore_bundle,
                uploaded_by: Some(uploaded_by),
                approved_by,
                approved_at,
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
            Ok(PutOutcome::Replaced(new))
        }
        None => {
            let info = WasmModuleInfo {
                id: new_id(),
                name: m.name,
                sha256: sha,
                size: size as u64,
                description: m.description,
                created_by: uploaded_by.clone(),
                created_at: t,
                updated_at: t,
                signature: m.signature,
                sigstore_bundle: m.sigstore_bundle,
                uploaded_by: Some(uploaded_by),
                approved_by,
                approved_at,
            };
            Q::new(
                "INSERT INTO wasm_module (id, name, sha256, size, description, content, \
                 created_by, created_at, updated_at, signature, signature_key_id, \
                 sigstore_bundle, uploaded_by, approved_by, approved_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
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
            .bind(Val::Text(info.sigstore_bundle.clone()))
            .bind(Val::Text(info.uploaded_by.clone()))
            .bind(Val::Text(info.approved_by.clone()))
            .bind(Val::opt_time(info.approved_at))
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
            Ok(PutOutcome::Created(info))
        }
    }
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
    /// Ohne Freigabe (Vier-Augen-Prinzip aus bzw. Startkonfiguration).
    pub async fn put_wasm_module(
        &self,
        actor: &str,
        m: NewWasmModule,
        max_bytes: u64,
    ) -> Result<PutOutcome, StoreError> {
        let sha = validate_new(&m, max_bytes)?;
        let existing = self.wasm_module_info(&m.name).await?;
        if let Some(old) = &existing
            && same_version(old, &sha, &m)
        {
            return Ok(PutOutcome::Unchanged(old.clone()));
        }
        let mut tx = AdminTx::begin(self, actor).await?;
        let outcome = write_active(&mut tx, existing, m, sha, actor, None).await?;
        tx.commit().await?;
        Ok(outcome)
    }

    /// Reicht ein Modul zur Freigabe ein (Vier-Augen-Prinzip, AIGW-106): Prüfungen wie
    /// [`Self::put_wasm_module`], aber geschrieben wird nach `wasm_module_pending` (höchstens
    /// eine Einreichung je Name; eine neue ersetzt die alte). Die aktive Version bleibt bis zur
    /// Freigabe unverändert. Gleich der aktiven Version: nichts eingereicht.
    pub async fn submit_wasm_module(
        &self,
        actor: &str,
        person: &str,
        m: NewWasmModule,
        max_bytes: u64,
    ) -> Result<SubmitOutcome, StoreError> {
        let sha = validate_new(&m, max_bytes)?;
        if let Some(old) = self.wasm_module_info(&m.name).await?
            && same_version(&old, &sha, &m)
        {
            return Ok(SubmitOutcome::Unchanged(old));
        }
        let previous = self.pending_wasm_module_info(&m.name).await?;
        let info = PendingWasmModuleInfo {
            id: new_id(),
            name: m.name,
            sha256: sha,
            size: m.content.len() as u64,
            description: m.description,
            uploaded_by: actor.to_owned(),
            uploader_person: person.to_owned(),
            uploaded_at: now(),
            signature: m.signature,
            sigstore_bundle: m.sigstore_bundle,
        };
        let mut tx = AdminTx::begin(self, actor).await?;
        Q::new("DELETE FROM wasm_module_pending WHERE name = $1")
            .bind(Val::text(info.name.clone()))
            .execute(&mut tx.tx)
            .await?;
        Q::new(
            "INSERT INTO wasm_module_pending (id, name, sha256, size, description, content, \
             signature, signature_key_id, sigstore_bundle, uploaded_by, uploader_person, \
             uploaded_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
        )
        .bind(Val::uuid(info.id))
        .bind(Val::text(info.name.clone()))
        .bind(Val::text(info.sha256.clone()))
        .bind(Val::int(i64::try_from(info.size).unwrap_or(i64::MAX)))
        .bind(Val::Text(info.description.clone()))
        .bind(Val::Bytes(Some(m.content)))
        .bind(Val::Text(
            info.signature.as_ref().map(|s| s.signature.clone()),
        ))
        .bind(Val::Text(info.signature.as_ref().map(|s| s.key_id.clone())))
        .bind(Val::Text(info.sigstore_bundle.clone()))
        .bind(Val::text(info.uploaded_by.clone()))
        .bind(Val::text(info.uploader_person.clone()))
        .bind(Val::time(info.uploaded_at))
        .execute(&mut tx.tx)
        .await
        .map_err(|e| crate::repo::map_db(e, "WASM-Modul (Einreichung)"))?;
        tx.audit_kind(
            Action::Submit,
            APPROVAL_AUDIT_KIND,
            Some(info.id),
            &info.name,
            previous.as_ref(),
            Some(&info),
        )
        .await?;
        tx.commit().await?;
        Ok(SubmitOutcome::Pending {
            info,
            replaced: previous.is_some(),
        })
    }

    /// Alle Einreichungen (Metadaten), nach Name sortiert.
    pub async fn list_pending_wasm_modules(
        &self,
    ) -> Result<Vec<PendingWasmModuleInfo>, StoreError> {
        Q::new(format!(
            "SELECT {PENDING_COLS} FROM wasm_module_pending ORDER BY name"
        ))
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(pending_from_row)
        .collect()
    }

    /// Einreichung eines Moduls (Metadaten).
    pub async fn pending_wasm_module_info(
        &self,
        name: &str,
    ) -> Result<Option<PendingWasmModuleInfo>, StoreError> {
        Q::new(format!(
            "SELECT {PENDING_COLS} FROM wasm_module_pending WHERE name = $1"
        ))
        .bind(Val::text(name))
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(pending_from_row)
        .transpose()
    }

    /// Einreichung mit Inhalt (Prüfen vor der Freigabe).
    pub async fn pending_wasm_module(
        &self,
        name: &str,
    ) -> Result<Option<(PendingWasmModuleInfo, Bytes)>, StoreError> {
        let Some(row) = Q::new(format!(
            "SELECT {PENDING_COLS}, content FROM wasm_module_pending WHERE name = $1"
        ))
        .bind(Val::text(name))
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        Ok(Some((
            pending_from_row(&row)?,
            Bytes::from(row.bytes("content")?),
        )))
    }

    /// Gibt die Einreichung `name` frei: sie wird (in einer Transaktion) die aktive Version
    /// (Audit `wasm-module`, `config_version` + 1, Hot Reload) und die Einreichung entfällt
    /// (Audit `approve`). `sha256` muss der eingereichten Version entsprechen (sonst
    /// [`StoreError::Conflict`]: inzwischen ersetzt). `Ok(None)`: keine Einreichung. Ob
    /// `actor` freigeben darf (andere Person als der Hochlader), prüft der Aufrufer.
    pub async fn approve_wasm_module(
        &self,
        actor: &str,
        name: &str,
        sha256: &str,
    ) -> Result<Option<PutOutcome>, StoreError> {
        let sha = aigw_core::wasm::normalize_sha256(sha256).map_err(StoreError::Invalid)?;
        let Some((p, content)) = self.pending_wasm_module(name).await? else {
            return Ok(None);
        };
        if p.sha256 != sha {
            return Err(StoreError::Conflict(format!(
                "Einreichung von {name:?} hat SHA-256 {} (inzwischen ersetzt?)",
                p.sha256
            )));
        }
        let existing = self.wasm_module_info(name).await?;
        let mut tx = AdminTx::begin(self, actor).await?;
        let n = Q::new("DELETE FROM wasm_module_pending WHERE id = $1")
            .bind(Val::uuid(p.id))
            .execute(&mut tx.tx)
            .await?;
        if n == 0 {
            return Err(StoreError::Conflict(format!(
                "Einreichung von {name:?} wurde gerade geändert"
            )));
        }
        let m = NewWasmModule {
            name: p.name.clone(),
            description: p.description.clone(),
            content: content.to_vec(),
            sha256: Some(p.sha256.clone()),
            signature: p.signature.clone(),
            sigstore_bundle: p.sigstore_bundle.clone(),
        };
        let approval = Approval {
            uploaded_by: p.uploaded_by.clone(),
            approved_by: actor.to_owned(),
            at: now(),
        };
        let outcome = write_active(
            &mut tx,
            existing,
            m,
            p.sha256.clone(),
            actor,
            Some(approval),
        )
        .await?;
        tx.audit_kind(
            Action::Approve,
            APPROVAL_AUDIT_KIND,
            Some(p.id),
            &p.name,
            Some(&p),
            None::<&()>,
        )
        .await?;
        tx.commit().await?;
        Ok(Some(outcome))
    }

    /// Verwirft die Einreichung `name` (Audit `reject`). `Ok(None)`: keine Einreichung.
    pub async fn reject_wasm_module(
        &self,
        actor: &str,
        name: &str,
    ) -> Result<Option<PendingWasmModuleInfo>, StoreError> {
        let Some(p) = self.pending_wasm_module_info(name).await? else {
            return Ok(None);
        };
        let mut tx = AdminTx::begin(self, actor).await?;
        let n = Q::new("DELETE FROM wasm_module_pending WHERE id = $1")
            .bind(Val::uuid(p.id))
            .execute(&mut tx.tx)
            .await?;
        if n == 0 {
            return Ok(None);
        }
        tx.audit_kind(
            Action::Reject,
            APPROVAL_AUDIT_KIND,
            Some(p.id),
            &p.name,
            Some(&p),
            None::<&()>,
        )
        .await?;
        tx.commit().await?;
        Ok(Some(p))
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
