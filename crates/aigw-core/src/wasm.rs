// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! WASM-Module für Plugins der Art `wasm` (AIGW-63, `docs/konzept/17-wasm-plugins.md`).
//!
//! Ein Modul ist eine WebAssembly-**Komponente** (Component Model, WIT-Welt
//! `aigw:plugin/plugin`), hochgeladen über die Admin-API (nur globaler `admin`) oder aus einer
//! Datei der Startkonfiguration (`wasm.modules`). Es liegt mit SHA-256 in der Datenbank und
//! gelangt über den Snapshot zu allen Instanzen (Hot Reload); Bindungen verweisen per Name
//! (optional zusätzlich mit SHA-256).

use std::fmt;

use bytes::Bytes;
use time::OffsetDateTime;

use crate::resource::Id;

/// Höchstgröße eines Moduls, die das Gateway überhaupt annimmt (`wasm.max_module_bytes` darf
/// nur kleiner sein).
pub const MAX_MODULE_BYTES: u64 = 64 * 1024 * 1024;

/// Ein gespeichertes WASM-Modul (Metadaten + Inhalt).
#[derive(Clone, PartialEq, Eq)]
pub struct WasmModule {
    /// ID (UUIDv7).
    pub id: Id,
    /// Name (wie Ressourcennamen).
    pub name: String,
    /// SHA-256 des Inhalts, hex, klein geschrieben.
    pub sha256: String,
    /// Beschreibung.
    pub description: Option<String>,
    /// Hochgeladen von (Admin-Principal bzw. `startup-config`).
    pub created_by: String,
    /// Angelegt.
    pub created_at: OffsetDateTime,
    /// Zuletzt ersetzt.
    pub updated_at: OffsetDateTime,
    /// Inhalt (Komponente im Binärformat).
    pub content: Bytes,
}

impl WasmModule {
    /// Größe in Byte.
    pub fn size(&self) -> u64 {
        self.content.len() as u64
    }
}

impl fmt::Debug for WasmModule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasmModule")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("sha256", &self.sha256)
            .field("size", &self.content.len())
            .finish_non_exhaustive()
    }
}

/// Prüft eine SHA-256-Angabe (64 Hex-Zeichen) und liefert sie klein geschrieben.
pub fn normalize_sha256(s: &str) -> Result<String, String> {
    let s = s.trim();
    let s = s.strip_prefix("sha256:").unwrap_or(s);
    if s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(s.to_ascii_lowercase())
    } else {
        Err("sha256: 64 Hex-Zeichen erwartet".into())
    }
}

/// Erste Bytes einer Komponente (Magic + Layer 1). Kern-Module (Layer 0) sind keine
/// Komponenten und werden abgelehnt.
pub fn looks_like_component(content: &[u8]) -> bool {
    content.len() >= 8 && content[..4] == *b"\0asm" && content[6..8] == [1, 0]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha_normalized() {
        let h = "AB".repeat(32);
        assert_eq!(normalize_sha256(&h).unwrap(), "ab".repeat(32));
        assert_eq!(
            normalize_sha256(&format!("sha256:{h}")).unwrap(),
            "ab".repeat(32)
        );
        assert!(normalize_sha256("abc").is_err());
        assert!(normalize_sha256(&"g".repeat(64)).is_err());
    }

    #[test]
    fn component_magic() {
        assert!(looks_like_component(b"\0asm\x0d\0\x01\0rest"));
        assert!(!looks_like_component(b"\0asm\x01\0\0\0"));
        assert!(!looks_like_component(b"\0asm"));
    }
}
