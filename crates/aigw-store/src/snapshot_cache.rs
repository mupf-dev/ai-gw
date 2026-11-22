// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Verschlüsselter Snapshot-Cache auf Platte (F14, AIGW-75).
//!
//! Zweck: Kaltstart mit dem letzten bekannten Konfigurationsstand, wenn die Datenbank beim
//! Start nicht erreichbar ist (Readiness meldet dann `degraded`). Sobald die Datenbank wieder
//! da ist, ersetzt der Reloader den Cache-Stand durch einen vollständigen Neubau.
//!
//! Dateiformat (alle Zahlen Big Endian):
//!
//! ```text
//! "AIGWSNAP" | Format u16 | Länge Key-ID u8 | Key-ID | Nonce (12) | Chiffrat + Tag
//! ```
//!
//! * Inhalt: JSON der Snapshot-Rohdaten (inkl. entschlüsselter Zugangsdaten), als Ganzes mit
//!   AES-256-GCM und dem aktuellen Master-Key verschlüsselt; Kopf (Magic, Format, Key-ID) als
//!   AAD → jede Veränderung der Datei fällt beim Entschlüsseln auf (Integrität).
//! * Versionsprüfung: unbekanntes Format bzw. nicht lesbarer Inhalt → Cache wird verworfen.
//!   Die `config_version` und der Schreibzeitpunkt stehen im (verschlüsselten) Inhalt.
//! * Atomar: Schreiben in `<datei>.tmp` (Modus 0600), `fsync`, `rename`, `fsync` des
//!   Verzeichnisses. Ein Absturz hinterlässt entweder den alten oder den neuen Cache.
//! * Frühere Master-Keys (`secrets.previous_key_files`) können den Cache weiter öffnen.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aigw_core::resource::{
    ApiKey, CredentialData, Deployment, McpGrant, McpServer, ModelGroup, Organization,
    PluginBinding, Price, ProviderCredential, SecretString, Team,
};
use aigw_core::snapshot::{KeyHash, Snapshot, SnapshotParts};
use serde::{Deserialize, Serialize};

use crate::{Keyring, StoreError};

const MAGIC: &[u8; 8] = b"AIGWSNAP";
/// Aktuelles Dateiformat; ältere/neuere werden verworfen.
pub const CACHE_FORMAT: u16 = 1;
const NONCE_LEN: usize = 12;

/// Fehler beim Lesen oder Schreiben des Caches.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CacheError {
    /// Dateisystem.
    #[error("Snapshot-Cache {path}: {source}")]
    Io {
        /// Datei.
        path: PathBuf,
        /// Ursache.
        source: std::io::Error,
    },
    /// Kein gültiger Cache (Magic, Format, Länge).
    #[error("Snapshot-Cache ungültig: {0}")]
    Format(String),
    /// Entschlüsselung (falscher Master-Key oder veränderte Datei).
    #[error("Snapshot-Cache nicht entschlüsselbar: {0}")]
    Crypto(StoreError),
    /// Inhalt nicht lesbar.
    #[error("Snapshot-Cache-Inhalt: {0}")]
    Json(#[from] serde_json::Error),
}

/// Gelesener Cache.
#[derive(Debug)]
pub struct CachedSnapshot {
    /// Snapshot (mit allen Indizes).
    pub snapshot: Snapshot,
    /// Schreibzeitpunkt.
    pub saved_at: SystemTime,
    /// Programmversion, die den Cache geschrieben hat.
    pub gateway_version: String,
}

/// Verschlüsselter Snapshot-Cache in einer Datei.
#[derive(Debug, Clone)]
pub struct SnapshotCache {
    path: PathBuf,
    keyring: Arc<Keyring>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CachedKey {
    hash: String,
    key: ApiKey,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CachedCredential {
    info: ProviderCredential,
    /// Klartext – nur innerhalb des verschlüsselten Inhalts.
    values: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize)]
struct CachedSecret {
    info: aigw_core::resource::Secret,
    /// Klartext – nur innerhalb des verschlüsselten Inhalts.
    values: BTreeMap<String, String>,
}

fn plain(data: &CredentialData) -> BTreeMap<String, String> {
    data.0
        .iter()
        .map(|(k, v)| (k.clone(), v.expose().to_owned()))
        .collect()
}

fn sealed_data(values: BTreeMap<String, String>) -> CredentialData {
    CredentialData(
        values
            .into_iter()
            .map(|(k, v)| (k, SecretString::new(v)))
            .collect(),
    )
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Body {
    format: u16,
    gateway_version: String,
    saved_at_ms: u64,
    version: i64,
    organizations: Vec<Organization>,
    teams: Vec<Team>,
    keys: Vec<CachedKey>,
    credentials: Vec<CachedCredential>,
    /// Geheimnisse (F149; fehlt in älteren Cache-Dateien).
    #[serde(default)]
    secrets: Vec<CachedSecret>,
    model_groups: Vec<ModelGroup>,
    deployments: Vec<Deployment>,
    prices: Vec<Price>,
    plugin_bindings: Vec<PluginBinding>,
    mcp_servers: Vec<McpServer>,
    mcp_grants: Vec<McpGrant>,
    /// Tool-Richtlinien (AIGW-87; ältere Caches ohne Feld lesbar).
    #[serde(default)]
    tool_policies: Vec<aigw_core::tools::ToolPolicy>,
    /// WASM-Module (AIGW-63; Inhalt Base64; ältere Caches ohne Feld lesbar).
    #[serde(default)]
    wasm_modules: Vec<CachedWasm>,
}

/// WASM-Modul im Cache (Inhalt Base64).
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CachedWasm {
    id: aigw_core::resource::Id,
    name: String,
    sha256: String,
    #[serde(default)]
    description: Option<String>,
    created_by: String,
    #[serde(with = "time::serde::rfc3339")]
    created_at: time::OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: time::OffsetDateTime,
    content: String,
    /// Signatur (AIGW-101; ältere Caches ohne Feld lesbar).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signature: Option<aigw_core::wasm::ModuleSignature>,
}

impl Body {
    fn from_parts(p: SnapshotParts, saved_at_ms: u64) -> Self {
        Self {
            format: CACHE_FORMAT,
            gateway_version: aigw_core::VERSION.to_owned(),
            saved_at_ms,
            version: p.version,
            organizations: p.organizations,
            teams: p.teams,
            keys: p
                .keys
                .into_iter()
                .map(|(h, key)| CachedKey {
                    hash: h.to_hex(),
                    key,
                })
                .collect(),
            credentials: p
                .credentials
                .into_iter()
                .map(|(info, data)| CachedCredential {
                    info,
                    values: data
                        .0
                        .iter()
                        .map(|(k, v)| (k.clone(), v.expose().to_owned()))
                        .collect(),
                })
                .collect(),
            secrets: p
                .secrets
                .into_iter()
                .map(|(info, data)| CachedSecret {
                    values: plain(&data),
                    info,
                })
                .collect(),
            model_groups: p.model_groups,
            deployments: p.deployments,
            prices: p.prices,
            plugin_bindings: p.plugin_bindings,
            mcp_servers: p.mcp_servers,
            mcp_grants: p.mcp_grants,
            tool_policies: p.tool_policies,
            wasm_modules: p
                .wasm_modules
                .into_iter()
                .map(|m| CachedWasm {
                    id: m.id,
                    name: m.name,
                    sha256: m.sha256,
                    description: m.description,
                    created_by: m.created_by,
                    created_at: m.created_at,
                    updated_at: m.updated_at,
                    content: base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        &m.content,
                    ),
                    signature: m.signature,
                })
                .collect(),
        }
    }

    fn into_parts(self) -> Result<SnapshotParts, CacheError> {
        let keys = self
            .keys
            .into_iter()
            .map(|k| {
                KeyHash::from_hex(&k.hash)
                    .map(|h| (h, k.key))
                    .ok_or_else(|| CacheError::Format("ungültiger Key-Hash".into()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(SnapshotParts {
            version: self.version,
            organizations: self.organizations,
            teams: self.teams,
            keys,
            credentials: self
                .credentials
                .into_iter()
                .map(|c| {
                    let data = CredentialData(
                        c.values
                            .into_iter()
                            .map(|(k, v)| (k, SecretString::new(v)))
                            .collect(),
                    );
                    (c.info, data)
                })
                .collect(),
            secrets: self
                .secrets
                .into_iter()
                .map(|s| (s.info, sealed_data(s.values)))
                .collect(),
            model_groups: self.model_groups,
            deployments: self.deployments,
            prices: self.prices,
            plugin_bindings: self.plugin_bindings,
            mcp_servers: self.mcp_servers,
            mcp_grants: self.mcp_grants,
            tool_policies: self.tool_policies,
            wasm_modules: self
                .wasm_modules
                .into_iter()
                .map(|m| {
                    let content = base64::Engine::decode(
                        &base64::engine::general_purpose::STANDARD,
                        m.content.as_bytes(),
                    )
                    .map_err(|_| CacheError::Format("WASM-Modul: ungültiges Base64".into()))?;
                    Ok(aigw_core::wasm::WasmModule {
                        id: m.id,
                        name: m.name,
                        sha256: m.sha256,
                        description: m.description,
                        created_by: m.created_by,
                        created_at: m.created_at,
                        updated_at: m.updated_at,
                        content: bytes::Bytes::from(content),
                        signature: m.signature,
                    })
                })
                .collect::<Result<Vec<_>, CacheError>>()?,
        })
    }
}

fn header(key_id: &str) -> Result<Vec<u8>, CacheError> {
    let id = key_id.as_bytes();
    let len =
        u8::try_from(id.len()).map_err(|_| CacheError::Format("Key-ID zu lang".to_owned()))?;
    let mut h = Vec::with_capacity(MAGIC.len() + 3 + id.len());
    h.extend_from_slice(MAGIC);
    h.extend_from_slice(&CACHE_FORMAT.to_be_bytes());
    h.push(len);
    h.extend_from_slice(id);
    Ok(h)
}

/// Überschreibt Klartext vor der Freigabe (best effort).
fn wipe(mut buf: Vec<u8>) {
    buf.fill(0);
    std::hint::black_box(&buf);
}

impl SnapshotCache {
    /// Cache in `path`, verschlüsselt mit dem aktuellen Key aus `keyring`.
    pub fn new(path: impl Into<PathBuf>, keyring: Arc<Keyring>) -> Self {
        Self {
            path: path.into(),
            keyring,
        }
    }

    /// Pfad der Cache-Datei.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn io(&self, source: std::io::Error) -> CacheError {
        CacheError::Io {
            path: self.path.clone(),
            source,
        }
    }

    /// Schreibt `snapshot` atomar (blockierend – im Server über `spawn_blocking`).
    pub fn write(&self, snapshot: &Snapshot) -> Result<(), CacheError> {
        let saved_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        let body = Body::from_parts(snapshot.to_parts(), saved_at_ms);
        let plain = serde_json::to_vec(&body)?;
        drop(body);
        let head = header(self.keyring.primary_id())?;
        // Verschlüsselt im Platz: danach steht kein Klartext mehr im Puffer.
        let (_, nonce, cipher) = self
            .keyring
            .seal_bytes(&head, plain)
            .map_err(CacheError::Crypto)?;

        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| self.io(e))?;
        }
        let tmp = self.path.with_file_name(format!(
            "{}.tmp",
            self.path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "snapshot".into())
        ));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        let result = (|| {
            let mut f = opts.open(&tmp)?;
            f.write_all(&head)?;
            f.write_all(&nonce)?;
            f.write_all(&cipher)?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, &self.path)?;
            if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
                // Verzeichniseintrag dauerhaft machen (nicht auf allen Plattformen möglich).
                if let Ok(d) = std::fs::File::open(dir) {
                    let _ = d.sync_all();
                }
            }
            Ok(())
        })();
        result.map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            self.io(e)
        })
    }

    /// Liest und prüft den Cache. `Ok(None)`: keine Cache-Datei vorhanden.
    pub fn read(&self) -> Result<Option<CachedSnapshot>, CacheError> {
        let data = match std::fs::read(&self.path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(self.io(e)),
        };
        let bad = |m: &str| CacheError::Format(m.to_owned());
        let rest = data
            .strip_prefix(MAGIC.as_slice())
            .ok_or_else(|| bad("keine Snapshot-Cache-Datei"))?;
        let (fmt, rest) = rest.split_at_checked(2).ok_or_else(|| bad("zu kurz"))?;
        let format = u16::from_be_bytes([fmt[0], fmt[1]]);
        if format != CACHE_FORMAT {
            return Err(CacheError::Format(format!(
                "Format {format} (erwartet {CACHE_FORMAT})"
            )));
        }
        let (&id_len, rest) = rest.split_first().ok_or_else(|| bad("zu kurz"))?;
        let (id, rest) = rest
            .split_at_checked(usize::from(id_len))
            .ok_or_else(|| bad("zu kurz"))?;
        let key_id = std::str::from_utf8(id).map_err(|_| bad("Key-ID"))?;
        let (nonce, cipher) = rest
            .split_at_checked(NONCE_LEN)
            .ok_or_else(|| bad("zu kurz"))?;
        let head = header(key_id)?;
        let plain = self
            .keyring
            .open_bytes(key_id, &head, nonce, cipher.to_vec())
            .map_err(CacheError::Crypto)?;
        let parsed = serde_json::from_slice::<Body>(&plain);
        wipe(plain);
        let body = parsed?;
        if body.format != CACHE_FORMAT {
            return Err(CacheError::Format(format!("Inhaltsformat {}", body.format)));
        }
        let saved_at = UNIX_EPOCH + Duration::from_millis(body.saved_at_ms);
        let gateway_version = body.gateway_version.clone();
        let parts = body.into_parts()?;
        Ok(Some(CachedSnapshot {
            snapshot: Snapshot::from_parts(parts),
            saved_at,
            gateway_version,
        }))
    }

    /// Wie [`SnapshotCache::read`], verwirft aber einen ungültigen Cache (Datei wird gelöscht,
    /// Warnung im Log) und liefert dann `None`.
    pub fn read_or_discard(&self) -> Option<CachedSnapshot> {
        match self.read() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %self.path.display(),
                    "Snapshot-Cache unbrauchbar – verworfen"
                );
                self.discard();
                None
            }
        }
    }

    /// Löscht die Cache-Datei (Fehler werden ignoriert).
    pub fn discard(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::MasterKey;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "aigw-snapcache-{tag}-{}",
                aigw_core::resource::new_id()
            ))
            .join("snapshot.cache")
    }

    #[test]
    fn missing_file_is_none_and_header_roundtrip() {
        let ring = Arc::new(Keyring::new(MasterKey::generate().unwrap().0));
        let c = SnapshotCache::new(temp_path("none"), Arc::clone(&ring));
        assert!(c.read().unwrap().is_none());
        assert!(c.read_or_discard().is_none());

        c.write(&Snapshot {
            version: 42,
            ..Snapshot::default()
        })
        .unwrap();
        let back = c.read().unwrap().unwrap();
        assert_eq!(back.snapshot.version, 42);
        assert_eq!(back.gateway_version, aigw_core::VERSION);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(c.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(c.path().parent().unwrap());
    }
}
