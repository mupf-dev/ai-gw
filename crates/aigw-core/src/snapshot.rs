// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Unveränderlicher Konfigurations-Snapshot für den Hot Path (§14).
//!
//! Wird aus der Datenbank aufgebaut (`aigw_store::load_snapshot`) und enthält vorberechnete
//! Indizes, damit Auth und Routing ohne DB-Zugriff in O(1) laufen. Den jeweils aktuellen
//! Snapshot hält [`SnapshotHandle`] (`ArcSwap`, lock-frei) samt Zustand für die Readiness
//! (Quelle, Abgleich mit der Datenbank; F15); neu gebaut wird er vom Reloader in
//! `aigw_store::reload` – inkrementell über [`Snapshot::apply_patch`], als Rückfall vollständig
//! über [`Snapshot::from_parts`] (F16, AIGW-75).

mod handle;
mod parts;

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use time::OffsetDateTime;

use crate::resource::{
    ApiKey, CredentialData, Deployment, Id, McpGrant, McpServer, ModelGroup, Organization,
    PluginBinding, Price, ProviderCredential, Team,
};
use crate::types::ProviderKind;

pub use handle::{DEFAULT_STALE_AFTER, SnapshotHandle, SnapshotSource, SnapshotStatus};
pub use parts::{Change, SnapshotParts, SnapshotPatch};

/// SHA-256 eines API-Keys (Schlüssel des Key-Index).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeyHash(pub [u8; 32]);

impl KeyHash {
    /// Hex-Darstellung (so steht der Hash in der Datenbank).
    pub fn to_hex(&self) -> String {
        use fmt::Write as _;
        let mut s = String::with_capacity(64);
        for b in self.0 {
            let _ = write!(s, "{b:02x}");
        }
        s
    }

    /// Gegenstück zu [`KeyHash::to_hex`].
    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 64 || !s.is_ascii() {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, o) in out.iter_mut().enumerate() {
            *o = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
        }
        Some(Self(out))
    }
}

impl fmt::Debug for KeyHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Nur ein Ausschnitt – der volle Hash ist zwar kein Geheimnis, aber unnötig in Logs.
        write!(f, "KeyHash({}…)", &self.to_hex()[..12])
    }
}

/// Alles, was der Hot Path zu einem authentifizierten Key braucht.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyCtx {
    /// Der Key (Spezifikation + Status).
    pub key: ApiKey,
    /// Team, falls zugeordnet.
    pub team: Option<Arc<Team>>,
    /// Organisation des Keys (F3: Pflicht; mit Team die Organisation des Teams).
    pub organization: Option<Arc<Organization>>,
    /// Ablauf (Kopie aus `key.spec`, muss pro Anfrage geprüft werden, da Snapshots altern).
    pub expires_at: Option<OffsetDateTime>,
}

impl KeyCtx {
    /// Ist der Key zum Zeitpunkt `now` gültig?
    pub fn is_valid_at(&self, now: OffsetDateTime) -> bool {
        self.expires_at.is_none_or(|e| now < e)
    }

    /// Wirksamer Payload-Modus (F30, S-09): Ausgangspunkt `default` (`observe.payload`), dann
    /// von oben nach unten Organisation → Team → Key – das spezifischste gesetzte
    /// `payloadMode` gilt, aber nur bis zur ersten Ebene mit `payloadModeLocked` (darunter wird
    /// `payloadMode` ignoriert). Das Ergebnis wird auf das Minimum aller `payloadModeMax` von
    /// Organisation und Team begrenzt. Die Prüfung beim Schreiben verhindert solche Werte
    /// bereits; hier greift sie auch für Altdaten, `force` und Importe.
    pub fn payload_mode(
        &self,
        default: crate::observe::PayloadMode,
    ) -> crate::observe::PayloadMode {
        let mut mode = default;
        let mut cap = crate::observe::PayloadMode::Full;
        let mut locked = false;
        let levels = [
            self.organization.as_ref().map(|o| {
                (
                    o.spec.payload_mode,
                    o.spec.payload_mode_max,
                    o.spec.payload_mode_locked,
                )
            }),
            self.team.as_ref().map(|t| {
                (
                    t.spec.payload_mode,
                    t.spec.payload_mode_max,
                    t.spec.payload_mode_locked,
                )
            }),
            Some((self.key.spec.payload_mode, None, false)),
        ];
        for (m, max, lock) in levels.into_iter().flatten() {
            if !locked && let Some(m) = m {
                mode = m;
            }
            if let Some(max) = max {
                cap = cap.min(max);
            }
            locked |= lock;
        }
        mode.min(cap)
    }

    /// `exposeReasoning` (Key → Team → Organisation; F185), `None` = nicht gesetzt.
    pub fn expose_reasoning(&self) -> Option<bool> {
        self.key
            .spec
            .expose_reasoning
            .or_else(|| self.team.as_ref().and_then(|t| t.spec.expose_reasoning))
            .or_else(|| {
                self.organization
                    .as_ref()
                    .and_then(|o| o.spec.expose_reasoning)
            })
    }

    /// Darf der Key die Modellgruppe `group` (kanonischer Name) nutzen?
    ///
    /// Leere Freigabelisten bedeuten „alle“; Key- und Team-Liste müssen beide passen.
    pub fn allows_model(&self, group: &ModelGroupCtx) -> bool {
        let matches = |list: &[String]| list.is_empty() || list.iter().any(|m| group.answers_to(m));
        matches(&self.key.spec.allowed_models)
            && self
                .team
                .as_ref()
                .is_none_or(|t| matches(&t.spec.allowed_models))
    }
}

/// Deployment mit aufgelösten Zugangsdaten.
#[derive(Debug, Clone, PartialEq)]
pub struct DeploymentCtx {
    /// Deployment.
    pub deployment: Deployment,
    /// Entschlüsselte Zugangsdaten (falls zugeordnet).
    pub credential: Option<Arc<CredentialData>>,
}

/// MCP-Server mit aufgelösten Zugangsdaten (AIGW-41).
#[derive(Debug, Clone, PartialEq)]
pub struct McpServerCtx {
    /// Server.
    pub server: McpServer,
    /// Entschlüsselte Zugangsdaten (falls zugeordnet).
    pub credential: Option<Arc<CredentialData>>,
}

/// Modellgruppe mit ihren aktiven Deployments (nach Priorität, dann Name sortiert).
#[derive(Debug, Clone, PartialEq)]
pub struct ModelGroupCtx {
    /// Modellgruppe.
    pub group: ModelGroup,
    /// Aktive Deployments.
    pub deployments: Vec<DeploymentCtx>,
}

impl ModelGroupCtx {
    /// Hört die Gruppe auf `name` (Name oder Alias)?
    pub fn answers_to(&self, name: &str) -> bool {
        self.group.metadata.name == name || self.group.spec.aliases.iter().any(|a| a == name)
    }
}

/// Vollständiger Konfigurationsstand einer Version.
///
/// `PartialEq` vergleicht den Inhalt (nicht die Zeiger) – Grundlage der Konsistenzprüfung
/// „inkrementell = vollständig“ (F16).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    /// `config_version`, aus der der Snapshot gebaut wurde.
    pub version: i64,
    /// Key-Hash → Kontext (nur aktive Keys).
    pub keys: HashMap<KeyHash, Arc<KeyCtx>>,
    /// Key-ID → Hash der aktiven Keys (für den inkrementellen Neubau, F16). Gepflegt von
    /// [`Snapshot::from_parts`] und [`Snapshot::apply_patch`]; von Hand gebaute Snapshots
    /// (Tests) brauchen ihn nicht.
    pub key_ids: HashMap<Id, KeyHash>,
    /// Workload-/Agent-Identität → Hash des aktiven Keys (AIGW-115). Identitäten, die mehreren
    /// aktiven Keys zugeordnet sind, fehlen (mehrdeutig → keine Anmeldung). Gepflegt von
    /// [`Snapshot::from_parts`] und [`Snapshot::apply_patch`].
    pub workloads: HashMap<crate::workload::WorkloadKey, KeyHash>,
    /// Name **und** Alias → Modellgruppe.
    pub model_groups: HashMap<String, Arc<ModelGroupCtx>>,
    /// Organisationen nach ID.
    pub organizations: HashMap<Id, Arc<Organization>>,
    /// Teams nach ID.
    pub teams: HashMap<Id, Arc<Team>>,
    /// Zugangsdaten (Metadaten) nach Name.
    pub credentials: HashMap<String, ProviderCredential>,
    /// Entschlüsselte Zugangsdaten nach Name (nur im RAM; nie ausgeben oder loggen) – für
    /// Plugins mit Credential-Referenz: Webhook-Geheimnisse (AIGW-61), `content-safety` (AIGW-38).
    pub credential_data: HashMap<String, Arc<CredentialData>>,
    /// Geheimnisse (`Secret`, F149; Metadaten) nach Name.
    pub secrets: HashMap<String, crate::resource::Secret>,
    /// Entschlüsselte Geheimnisse (`Secret`, F149) nach Name (nur im RAM; nie ausgeben oder
    /// loggen) – für Webhook-Geheimnisse, `content-safety` und MCP-Server.
    pub secret_data: HashMap<String, Arc<CredentialData>>,
    /// Preise je (Anbieter, Anbieter-Modell), aufsteigend nach `validFrom`.
    /// Einträge ohne Anbieter stehen unter `None`.
    pub prices: HashMap<(Option<ProviderKind>, String), Vec<Arc<Price>>>,
    /// Plugin-Bindungen, sortiert nach Ebene und `order`.
    pub plugin_bindings: Vec<Arc<PluginBinding>>,
    /// Aktive MCP-Server nach Name.
    pub mcp_servers: HashMap<String, Arc<McpServerCtx>>,
    /// MCP-Freigaben.
    pub mcp_grants: Vec<Arc<McpGrant>>,
    /// Aktive Tool-Richtlinien (AIGW-87), nach Name sortiert.
    pub tool_policies: Vec<Arc<crate::tools::ToolPolicy>>,
    /// Vorberechneter Index der Tool-Richtlinien (Hot Path). Gepflegt von
    /// [`Snapshot::from_parts`] und [`Snapshot::apply_patch`].
    pub tool_index: Arc<crate::tools::ToolPolicyIndex>,
    /// WASM-Module (AIGW-63) nach Name – Inhalt geteilt (`Bytes`), kompiliert wird in
    /// `aigw-wasm` (Cache nach SHA-256).
    pub wasm_modules: HashMap<String, Arc<crate::wasm::WasmModule>>,
}

impl Snapshot {
    /// Schlägt einen Key über seinen Hash nach.
    pub fn key(&self, hash: &KeyHash) -> Option<&Arc<KeyCtx>> {
        self.keys.get(hash)
    }

    /// Schlägt den Key einer Workload-/Agent-Identität nach (AIGW-115).
    pub fn workload_key(&self, id: &crate::workload::WorkloadKey) -> Option<&Arc<KeyCtx>> {
        self.keys.get(self.workloads.get(id)?)
    }

    /// Baut den Index der Workload-Identitäten aus den aktiven Keys neu. Mehrfach zugeordnete
    /// Identitäten werden ausgelassen (deterministisch, unabhängig von der Reihenfolge);
    /// Admin-API und GitOps verhindern solche Doppelungen beim Schreiben.
    pub fn rebuild_workload_index(&mut self) {
        let mut index: HashMap<crate::workload::WorkloadKey, KeyHash> = HashMap::new();
        let mut ambiguous = std::collections::HashSet::new();
        for (hash, ctx) in &self.keys {
            for w in &ctx.key.spec.workload_identities {
                let k = w.key();
                if ambiguous.contains(&k) {
                    continue;
                }
                if index
                    .insert(k.clone(), *hash)
                    .is_some_and(|prev| prev != *hash)
                {
                    index.remove(&k);
                    ambiguous.insert(k);
                }
            }
        }
        self.workloads = index;
    }

    /// Löst einen Modellnamen (Name oder Alias) auf.
    pub fn model_group(&self, name: &str) -> Option<&Arc<ModelGroupCtx>> {
        self.model_groups.get(name)
    }

    /// Zum Zeitpunkt `at` gültiger Preis eines Anbieter-Modells (spezifisch vor generisch).
    pub fn price_at(
        &self,
        provider: ProviderKind,
        model: &str,
        at: OffsetDateTime,
    ) -> Option<&Arc<Price>> {
        let pick = |p: Option<ProviderKind>| {
            self.prices
                .get(&(p, model.to_owned()))?
                .iter()
                .rev()
                .find(|pr| pr.spec.valid_from <= at)
        };
        pick(Some(provider)).or_else(|| pick(None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_hash_hex_roundtrip() {
        let h = KeyHash([0xab; 32]);
        let hex = h.to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(KeyHash::from_hex(&hex), Some(h));
        assert_eq!(KeyHash::from_hex("zz"), None);
        assert!(format!("{h:?}").starts_with("KeyHash(abababababab"));
    }
}
