// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Startkonfiguration (YAML, E7).
//!
//! Die Startkonfiguration enthält nur, was vor dem Öffnen der Datenbank bekannt sein muss
//! (Listener, Datenbank, Zähler-Backend, Master-Key, Logging, Request-Log). Alles Weitere (Modelle, Keys,
//! Plugins …) sind Ressourcen in der Datenbank bzw. GitOps-YAML, siehe [`crate::resource`].
//!
//! Jeder Wert lässt sich per Umgebungsvariable überschreiben:
//! `AIGW_<ABSCHNITT>__<FELD>` (doppelter Unterstrich als Trenner, tiefere Ebenen ebenso),
//! z. B. `AIGW_STORE__URL=postgres://…` oder `AIGW_STORE__MAX_CONNECTIONS=20`.

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::observe::PayloadMode;
use crate::resource::Role;

/// Präfix der Umgebungsvariablen für Overrides.
pub const ENV_PREFIX: &str = "AIGW_";
/// Trenner zwischen Abschnitt und Feld in Umgebungsvariablen.
pub const ENV_SEPARATOR: &str = "__";

/// Gesamtkonfiguration des Gateways.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// HTTP-Server.
    pub server: ServerConfig,
    /// Persistenz.
    pub store: StoreConfig,
    /// Konfigurations-Snapshot: inkrementeller Neubau, Platten-Cache, Veraltung (AIGW-75).
    pub snapshot: SnapshotConfig,
    /// Zähler-Backend für Limits und Budgets.
    pub counters: CountersConfig,
    /// Master-Key für verschlüsselte Zugangsdaten.
    pub secrets: SecretsConfig,
    /// Logging.
    pub log: LogConfig,
    /// Request-Log, Rollups, Payload-Logging, Aufbewahrung, Export (AIGW-43).
    pub observe: ObserveConfig,
    /// GitOps (AIGW-33).
    pub gitops: GitopsConfig,
    /// Admin-API: Regeln für Upstream-Endpunkte (AIGW-65), Anmeldung (AIGW-32).
    pub admin: AdminConfig,
    /// Limits je Key/Team/Organisation/Modellgruppe (AIGW-30).
    pub limits: LimitsConfig,
    /// BPE-Rangtabellen von OpenAI zur Laufzeit (Build ohne `embedded-tokenizers`, AIGW-95).
    pub tokenizer: TokenizerConfig,
    /// Kosten und Budgets (AIGW-31).
    pub billing: BillingConfig,
    /// Ereignis-Webhooks (Budget-Warnungen usw., AIGW-61).
    pub events: EventsConfig,
    /// Antwort-Cache, exakt und semantisch (AIGW-39/40).
    pub cache: CacheConfig,
    /// MCP-Gateway (AIGW-41).
    pub mcp: McpConfig,
    /// Ausgehende Verbindungen: Wurzelzertifikate und zusätzliche CAs (AIGW-72).
    pub upstream: UpstreamConfig,
    /// Audit-Log: Aufbewahrung (F116).
    pub audit: AuditConfig,
    /// Tool-Inventar und Tool-Richtlinien (AIGW-87).
    pub tools: ToolsConfig,
    /// WASM-Plugins: Laufzeit-Grenzen und Module aus Dateien (AIGW-63).
    pub wasm: WasmConfig,
    /// Datenebene: Access-Tokens (JWT) von Workload-/Agent-Identitäten (AIGW-115).
    pub auth: crate::workload::AuthConfig,
}

/// WASM-Plugins (AIGW-63, `docs/konzept/17-wasm-plugins.md`): Standardgrenzen je Aufruf,
/// Instanz-Pool und Module aus Dateien. Die Grenzen einer Bindung (`fuel`, `maxMemoryMb`,
/// `timeoutMs`) dürfen die Obergrenzen hier nicht überschreiten.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WasmConfig {
    /// WASM-Plugins ausführen (`false`: Bindungen der Art `wasm` schlagen fehl → `failMode`).
    pub enabled: bool,
    /// Höchstgröße eines Moduls in Byte (Upload und Datei).
    pub max_module_bytes: u64,
    /// Standard-Fuel je Aufruf (CPU-Grenze in Wasm-Instruktionen, ungefähr).
    pub default_fuel: u64,
    /// Obergrenze für `fuel` einer Bindung.
    pub max_fuel: u64,
    /// Standard-Speichergrenze je Instanz (MiB, je linearem Speicher).
    pub default_memory_mb: u32,
    /// Obergrenze für `maxMemoryMb` einer Bindung (zugleich Größe der Pool-Slots).
    pub max_memory_mb: u32,
    /// Pooling-Allocator (vorab reservierte Instanz-Slots; schneller). `false`: Instanzen
    /// werden bei Bedarf angelegt.
    pub pooling: bool,
    /// Gleichzeitige Instanzen im Pool (darüber schlagen Aufrufe fehl → `failMode`).
    pub pool_instances: u32,
    /// Takt der Zeitgrenze (Epoch-Interrupt) in Millisekunden.
    pub epoch_tick_ms: u64,
    /// Kompilierte Module im Speicher behalten (Anzahl, nach SHA-256).
    pub compiled_cache_entries: usize,
    /// Module aus Dateien, beim Start in die Datenbank übernommen (Akteur `startup-config`).
    pub modules: Vec<WasmModuleFile>,
    /// Vertrauenswürdige öffentliche Ed25519-Schlüssel für Modul-Signaturen (AIGW-101).
    /// Leer = Signaturprüfung aus.
    pub trusted_keys: Vec<crate::wasm::TrustedKey>,
    /// Signatur Pflicht? Standard (`null`): **Pflicht, sobald `trusted_keys` gesetzt ist** –
    /// wer Schlüssel hinterlegt, will signierte Module; ein unsigniertes Modul würde die
    /// Prüfung sonst unbemerkt umgehen. `false` erlaubt unsignierte Module (Übergangszeit);
    /// vorhandene Signaturen müssen trotzdem gültig sein.
    pub require_signature: Option<bool>,
}

impl Default for WasmConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_module_bytes: 16 * 1024 * 1024,
            default_fuel: 100_000_000,
            max_fuel: 10_000_000_000,
            default_memory_mb: 32,
            max_memory_mb: 256,
            pooling: true,
            pool_instances: 256,
            epoch_tick_ms: 2,
            compiled_cache_entries: 64,
            modules: Vec::new(),
            trusted_keys: Vec::new(),
            require_signature: None,
        }
    }
}

/// Modul aus einer Datei der Startkonfiguration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WasmModuleFile {
    /// Name des Moduls (wie bei Ressourcen).
    pub name: String,
    /// Pfad zur Komponente (`.wasm`).
    pub path: String,
    /// Erwarteter SHA-256 (hex); Abweichung = Startfehler.
    pub sha256: String,
    /// Beschreibung.
    #[serde(default)]
    pub description: Option<String>,
    /// Ed25519-Signatur (Base64) über den SHA-256 (AIGW-101; `aigw wasm sign`).
    #[serde(default)]
    pub signature: Option<String>,
    /// Schlüssel-ID der Signatur (aus `wasm.trusted_keys`).
    #[serde(default)]
    pub key_id: Option<String>,
}

impl WasmModuleFile {
    /// Signatur der Datei (beide Felder oder keines).
    pub fn signature(&self) -> Result<Option<crate::wasm::ModuleSignature>, String> {
        match (&self.key_id, &self.signature) {
            (None, None) => Ok(None),
            (Some(k), Some(s)) => crate::wasm::ModuleSignature::parse(k, s).map(Some),
            _ => Err("signature und key_id nur gemeinsam angeben".into()),
        }
    }
}

impl WasmConfig {
    /// Prüft die Grenzen.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.max_module_bytes == 0 || self.max_module_bytes > crate::wasm::MAX_MODULE_BYTES {
            return Err(format!(
                "wasm.max_module_bytes: 1–{} Byte",
                crate::wasm::MAX_MODULE_BYTES
            ));
        }
        if self.default_fuel == 0 || self.default_fuel > self.max_fuel {
            return Err("wasm.default_fuel: 1 bis max_fuel".into());
        }
        if self.default_memory_mb == 0
            || self.default_memory_mb > self.max_memory_mb
            || self.max_memory_mb > 4096
        {
            return Err(
                "wasm.default_memory_mb/max_memory_mb: 1 ≤ Standard ≤ Obergrenze ≤ 4096".into(),
            );
        }
        if self.pool_instances == 0 || !(1..=1000).contains(&self.epoch_tick_ms) {
            return Err("wasm.pool_instances ≥ 1, wasm.epoch_tick_ms 1–1000".into());
        }
        for m in &self.modules {
            crate::resource::validate_name(&m.name)
                .map_err(|e| format!("wasm.modules[{}]: {e}", m.name))?;
            crate::wasm::normalize_sha256(&m.sha256)
                .map_err(|e| format!("wasm.modules[{}]: {e}", m.name))?;
            m.signature()
                .map_err(|e| format!("wasm.modules[{}]: {e}", m.name))?;
        }
        crate::wasm::TrustPolicy::from_config(self)?;
        Ok(())
    }
}

/// Tool-Inventar (AIGW-87, `docs/konzept/16-tools.md`). Die Richtlinien selbst sind Ressourcen
/// (`ToolPolicy`) und gelten unabhängig von `inventory`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolsConfig {
    /// Von Anwendungen mitgeschickte Tools inventarisieren (Hashes, Zähler, Clients).
    pub inventory: bool,
    /// Abstand der gepufferten Schreibvorgänge (Millisekunden).
    pub flush_interval_ms: u64,
    /// Höchstzahl im RAM abgeglichener Einträge (darüber wird nur noch gezählt, was schon
    /// bekannt ist; neue Einträge werden verworfen und gezählt).
    pub max_entries: usize,
    /// Geänderte Beschreibung/Schema eines bekannten Tools: `warn` (Ereignis, Audit,
    /// Markierung) oder `block` (zusätzlich Sperre `block-strip` auf Ebene des Keys).
    pub on_change: crate::tools::ToolChangeAction,
    /// Beschreibungen mit dem Prompt-Injection-Erkenner (`prompt-filter`) prüfen.
    pub injection_check: bool,
    /// Header, aus dem der Client gelesen wird (vor `user-agent`; z. B. `x-aigw-client`).
    pub client_headers: Vec<String>,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            inventory: true,
            flush_interval_ms: 5_000,
            max_entries: 100_000,
            on_change: crate::tools::ToolChangeAction::Warn,
            injection_check: true,
            client_headers: vec!["x-aigw-client".into(), "x-title".into()],
        }
    }
}

/// Audit-Log (F116): Aufbewahrung. Die Hash-Kette ist immer aktiv; geprüft wird sie mit
/// `aigw audit verify` bzw. `GET /admin/api/v1/audit-log/verify`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuditConfig {
    /// Einträge älter als so viele Tage löschen (`0` = unbegrenzt aufbewahren, Standard).
    /// Die Kette bleibt prüfbar: der letzte gelöschte Eintrag wird zum Anker.
    pub retention_days: u32,
    /// Abstand der Löschläufe (Sekunden).
    pub sweep_interval_secs: u64,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            retention_days: 0,
            sweep_interval_secs: 3600,
        }
    }
}

impl Config {
    /// Lädt die Konfiguration aus einer YAML-Datei und wendet `AIGW_…__…`-Overrides aus der
    /// Prozessumgebung an.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::ConfigRead {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_yaml_with_env(&text, std::env::vars())
    }

    /// Parst die Konfiguration aus YAML-Text (ohne Umgebungsvariablen).
    pub fn from_yaml(text: &str) -> Result<Self> {
        // Leere Datei bzw. nur Kommentare = alle Standardwerte.
        if text
            .lines()
            .all(|l| l.trim().is_empty() || l.trim_start().starts_with('#'))
        {
            return Ok(Self::default());
        }
        serde_saphyr::from_str(text).map_err(|e| Error::ConfigInvalid(e.to_string()))
    }

    /// Parst YAML und wendet Overrides aus `vars` an (testbar ohne Prozessumgebung).
    pub fn from_yaml_with_env<I>(text: &str, vars: I) -> Result<Self>
    where
        I: IntoIterator<Item = (String, String)>,
    {
        Self::from_yaml(text)?.with_env_overrides(vars)
    }

    /// Wendet `AIGW_<ABSCHNITT>__<FELD>=<wert>`-Overrides an.
    ///
    /// Variablen ohne `__` (z. B. `AIGW_CONFIG`, `AIGW_LOG`) werden ignoriert. Unbekannte
    /// Abschnitte/Felder sind ein Fehler (Tippfehler sollen auffallen). Zahlen und
    /// Wahrheitswerte werden passend zum Zieltyp umgewandelt.
    pub fn with_env_overrides<I>(self, vars: I) -> Result<Self>
    where
        I: IntoIterator<Item = (String, String)>,
    {
        let mut overrides: Vec<(Vec<String>, String)> = vars
            .into_iter()
            .filter_map(|(k, v)| {
                let rest = k.strip_prefix(ENV_PREFIX)?;
                if !rest.contains(ENV_SEPARATOR) {
                    return None;
                }
                let path = rest
                    .split(ENV_SEPARATOR)
                    .map(str::to_ascii_lowercase)
                    .collect();
                Some((path, v))
            })
            .collect();
        if overrides.is_empty() {
            return Ok(self);
        }
        overrides.sort();

        let mut tree = serde_json::to_value(&self).map_err(|e| Error::Internal(e.to_string()))?;
        for (path, value) in overrides {
            let var = format!(
                "{ENV_PREFIX}{}",
                path.iter()
                    .map(|p| p.to_ascii_uppercase())
                    .collect::<Vec<_>>()
                    .join(ENV_SEPARATOR)
            );
            set_path(&mut tree, &path, value)
                .map_err(|msg| Error::ConfigInvalid(format!("{var}: {msg}")))?;
        }
        serde_json::from_value(tree).map_err(|e| Error::ConfigInvalid(format!("Umgebung: {e}")))
    }

    /// Serialisiert die Konfiguration als YAML (z. B. für `aigw config show`).
    pub fn to_yaml(&self) -> Result<String> {
        serde_saphyr::to_string(self).map_err(|e| Error::Internal(e.to_string()))
    }
}

/// Setzt `path` in `tree` auf `raw`, umgewandelt passend zum bisherigen Typ.
fn set_path(
    tree: &mut serde_json::Value,
    path: &[String],
    raw: String,
) -> std::result::Result<(), String> {
    use serde_json::Value;
    let Some((last, parents)) = path.split_last() else {
        return Err("leerer Pfad".into());
    };
    let mut node = tree;
    for p in parents {
        let obj = node
            .as_object_mut()
            .ok_or_else(|| format!("{p:?} ist kein Abschnitt"))?;
        // Abschnitte existieren immer (serde default); fehlende sind unbekannt.
        node = obj
            .get_mut(p.as_str())
            .ok_or_else(|| format!("unbekannter Abschnitt {p:?}"))?;
    }
    let obj = node
        .as_object_mut()
        .ok_or_else(|| "Ziel ist kein Abschnitt".to_owned())?;
    let new = match obj.get(last.as_str()) {
        Some(Value::Number(_)) => raw
            .parse::<i64>()
            .map(Value::from)
            .or_else(|_| raw.parse::<f64>().map(Value::from))
            .map_err(|_| format!("Zahl erwartet, erhalten {raw:?}"))?,
        Some(Value::Bool(_)) => Value::Bool(match raw.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => return Err(format!("true/false erwartet, erhalten {raw:?}")),
        }),
        Some(Value::Array(_)) => Value::Array(
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| Value::String(s.to_owned()))
                .collect(),
        ),
        Some(Value::Object(_)) => return Err("Abschnitt kann nicht als Wert gesetzt werden".into()),
        // Zeichenkette oder fehlendes optionales Feld (null/weggelassen): als Text übernehmen,
        // die Typprüfung übernimmt anschließend serde (`deny_unknown_fields`).
        _ => Value::String(raw),
    };
    obj.insert(last.clone(), new);
    Ok(())
}

/// Einstellungen des HTTP-Servers: getrennte Listener für Daten- und Steuerebene.
///
/// Sind beide Adressen gleich, läuft nur ein Listener mit beiden Routern.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Datenebene: `/v1/*` (OpenAI, Anthropic) und `/healthz`.
    pub data: DataListenerConfig,
    /// Steuerebene: Admin-API, Admin-UI und `/healthz`.
    pub control: ControlListenerConfig,
    /// Verbindungs-Timeouts und HTTP/2-Grenzen beider Listener (S-32, AIGW-80).
    pub http: HttpServerConfig,
}

/// Verbindungs-Timeouts und HTTP/2-Grenzen beider Listener (S-32, AIGW-80; Slowloris).
///
/// Lese-Timeouts gelten nur, solange der Server auf Daten des Clients wartet (Anfragekopf,
/// Anfragekörper, nächste Anfrage). Laufende Antworten – auch lange SSE-Streams – brechen sie
/// nicht ab. `0` schaltet den jeweiligen Wert ab. Details: docs/betrieb/haertung.md.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpServerConfig {
    /// Höchstdauer vom ersten Byte bis zum vollständigen Anfragekopf (HTTP/1, Sekunden).
    pub header_read_timeout_secs: u64,
    /// Höchste Pause zwischen zwei Teilen eines Anfragekörpers (Sekunden).
    pub body_read_timeout_secs: u64,
    /// Mindestrate des Anfragekörpers in Bytes/s, gemessen ab Beginn des Lesens mit
    /// `body_read_timeout_secs` Vorlauf.
    pub body_min_bytes_per_sec: u64,
    /// Keep-Alive-Leerlauf: Verbindung ohne laufende Anfrage wird nach so vielen Sekunden
    /// geschlossen (HTTP/1 und HTTP/2). Größer als das Leerlauf-Timeout eines vorgeschalteten
    /// Load Balancers wählen.
    pub idle_timeout_secs: u64,
    /// HTTP/2: höchstens so viele gleichzeitige Streams je Verbindung.
    pub http2_max_concurrent_streams: u32,
    /// HTTP/2: PING-Abstand zur Erkennung toter Gegenstellen (Sekunden).
    pub http2_keepalive_interval_secs: u64,
    /// HTTP/2: Wartezeit auf die PING-Antwort, danach wird die Verbindung geschlossen.
    pub http2_keepalive_timeout_secs: u64,
}

impl Default for HttpServerConfig {
    fn default() -> Self {
        Self {
            header_read_timeout_secs: 10,
            body_read_timeout_secs: 30,
            body_min_bytes_per_sec: 1024,
            idle_timeout_secs: 120,
            http2_max_concurrent_streams: 256,
            http2_keepalive_interval_secs: 30,
            http2_keepalive_timeout_secs: 20,
        }
    }
}

impl ServerConfig {
    /// `true`, wenn Daten- und Steuerebene auf derselben Adresse laufen.
    pub fn is_combined(&self) -> bool {
        self.data.listen == self.control.listen
    }
}

/// Standardgrenze für Request-Bodies der Datenebene (32 MiB, wie die Anthropic-API).
pub const DEFAULT_MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
/// Standardgrenze für Audio-Uploads (25 MiB, wie bei OpenAI).
pub const DEFAULT_MAX_AUDIO_BYTES: usize = 25 * 1024 * 1024;

/// Listener der Datenebene.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DataListenerConfig {
    /// Adresse (Standard `127.0.0.1:4000`).
    pub listen: SocketAddr,
    /// Maximale Größe eines Request-Bodys in Bytes (Standard 32 MiB); größere → `413`.
    pub max_body_bytes: usize,
    /// Antwort-Header `x-aigw-deployment`, `x-aigw-attempts`, `x-aigw-fallback` setzen
    /// (Standard `true`; abschalten, wenn Clients die Topologie nicht sehen sollen).
    pub routing_headers: bool,
    /// Maximale Größe eines Audio-Uploads (`/v1/audio/transcriptions|translations`) in Bytes
    /// (Standard 25 MiB wie bei OpenAI; wirksam höchstens `max_body_bytes`).
    pub max_audio_bytes: usize,
    /// Höchstzahl gleichzeitiger Verbindungen (Standard 10 000, `0` = unbegrenzt); weitere
    /// Verbindungen bekommen sofort `503` und werden geschlossen (S-32). Gilt bei gemeinsamer
    /// Adresse für den gemeinsamen Listener.
    pub max_connections: usize,
}

impl Default for DataListenerConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 4000)),
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            routing_headers: true,
            max_audio_bytes: DEFAULT_MAX_AUDIO_BYTES,
            max_connections: 10_000,
        }
    }
}

/// Listener der Steuerebene.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ControlListenerConfig {
    /// Adresse (Standard `127.0.0.1:4001`).
    pub listen: SocketAddr,
    /// Höchstzahl gleichzeitiger Verbindungen (Standard 1 024, `0` = unbegrenzt, S-32).
    pub max_connections: usize,
}

impl Default for ControlListenerConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 4001)),
            max_connections: 1024,
        }
    }
}

/// Einstellungen der Persistenz.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StoreConfig {
    /// Datenbank-URL: `sqlite:…` (Standard) oder `postgres://…`.
    pub url: String,
    /// Höchstzahl gleichzeitiger Datenbankverbindungen.
    pub max_connections: u32,
    /// Abstand (Sekunden) der Absicherungs-Abfrage von `config_version`, falls eine
    /// Änderungsbenachrichtigung verloren geht (§14). `0` schaltet die Abfrage ab.
    pub reload_poll_secs: u64,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            url: "sqlite://aigw.db?mode=rwc".to_owned(),
            max_connections: 10,
            reload_poll_secs: 30,
        }
    }
}

/// Einstellungen des Konfigurations-Snapshots (§14; F14–F16, AIGW-75).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SnapshotConfig {
    /// Nur geänderte Objekte neu einarbeiten (F16); `false` = immer vollständiger Neubau.
    pub incremental: bool,
    /// Mehr Audit-Einträge seit dem aktiven Snapshot → vollständiger Neubau.
    pub max_incremental_changes: u32,
    /// Abstand (Sekunden) der Konsistenzprüfung: vollständiger Neubau im Hintergrund und
    /// Vergleich mit dem inkrementell gepflegten Snapshot. `0` = aus.
    pub verify_interval_secs: u64,
    /// Ab diesem Alter (Sekunden ohne erfolgreichen Abgleich mit der Datenbank) meldet
    /// `/readyz` `degraded` (F15).
    pub stale_after_secs: u64,
    /// Verschlüsselter Snapshot-Cache (AES-256-GCM mit dem Master-Key) für den Kaltstart ohne
    /// Datenbank (F14). Ohne Angabe aus.
    pub cache_file: Option<PathBuf>,
    /// Mindestabstand (Sekunden) zwischen zwei Schreibvorgängen des Caches.
    pub cache_write_interval_secs: u64,
    /// Höchstalter (Sekunden) des Caches für den Kaltstart (AIGW-58): ältere Caches werden
    /// verworfen, damit inzwischen gesperrte oder widerrufene Keys nicht wieder gelten.
    /// `0` = unbegrenzt.
    pub cache_max_age_secs: u64,
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            incremental: true,
            max_incremental_changes: 1000,
            verify_interval_secs: 600,
            stale_after_secs: 300,
            cache_file: None,
            cache_write_interval_secs: 10,
            cache_max_age_secs: 86_400,
        }
    }
}

/// Zähler-Backend (E4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CounterBackendKind {
    /// Nur im Prozess (eine Instanz).
    #[default]
    Local,
    /// Lokal zählen, alle 1–2 s mit Postgres abgleichen (unscharf).
    PostgresSync,
    /// Exakt und verteilt über Redis.
    Redis,
}

/// Einstellungen der Zähler (F26: Abgleich, Redis-Pool und -Zeitlimit).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CountersConfig {
    /// Gewähltes Backend.
    pub backend: CounterBackendKind,
    /// Redis-URL (nur für `backend: redis`), z. B. `redis://redis:6379/0`. Nur Einzelinstanz
    /// bzw. Sentinel-Primary; ein Server im Cluster-Modus bricht den Start ab (F27).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redis_url: Option<String>,
    /// Abgleichintervall von `postgres-sync` in Millisekunden (Standard 1000).
    pub sync_interval_ms: u64,
    /// Anzahl multiplexter Redis-Verbindungen (Standard 4).
    pub redis_pool_size: usize,
    /// Zeitlimit je Redis-Operation in Millisekunden, danach lokaler Rückfall (Standard 250).
    pub redis_timeout_ms: u64,
}

impl Default for CountersConfig {
    fn default() -> Self {
        Self {
            backend: CounterBackendKind::default(),
            redis_url: None,
            sync_interval_ms: 1000,
            redis_pool_size: 4,
            redis_timeout_ms: 250,
        }
    }
}

/// Master-Key für die Verschlüsselung der Anbieter-Zugangsdaten (AES-256-GCM).
///
/// Der Schlüssel (32 Byte, hexkodiert = 64 Zeichen) kommt aus `master_key_file` oder – falls
/// keine Datei angegeben ist – aus der Umgebungsvariable `master_key_env`. Er steht nie in der
/// Datenbank und nie in dieser Datei.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecretsConfig {
    /// Datei mit dem aktuellen Master-Key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub master_key_file: Option<PathBuf>,
    /// Name der Umgebungsvariable mit dem Master-Key (wenn keine Datei angegeben ist).
    pub master_key_env: String,
    /// Frühere Master-Keys (nur zum Entschlüsseln während einer Rotation).
    /// Per Umgebung als kommagetrennte Liste.
    pub previous_key_files: Vec<PathBuf>,
}

impl Default for SecretsConfig {
    fn default() -> Self {
        Self {
            master_key_file: None,
            master_key_env: "AIGW_MASTER_KEY".to_owned(),
            previous_key_files: Vec::new(),
        }
    }
}

/// Log-Format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LogFormat {
    /// Menschenlesbar.
    #[default]
    Text,
    /// JSON-Zeilen.
    Json,
}

/// GitOps (AIGW-33, `docs/konzept/09-gitops.md`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GitopsConfig {
    /// Modus „nur YAML“: Schreibrouten der Admin-API antworten mit `403 read-only`;
    /// Änderungen nur per `aigw config apply` bzw. `POST /admin/api/v1/config:apply`.
    pub read_only_admin: bool,
    /// Verzeichnis für `valueFrom.file`-Verweise in Manifesten, die über die Admin-API
    /// (`POST /admin/api/v1/config:apply`) angewendet werden (F52). Ohne: dort keine Dateien.
    /// Pfade werden normalisiert (relativ = innerhalb dieses Verzeichnisses), Symlinks, die
    /// hinausführen, werden abgelehnt. Die CLI ist nicht eingeschränkt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_dir: Option<PathBuf>,
    /// Rolle `gitops` darf per `POST /admin/api/v1/config:apply` auch `User` mit Rollen bzw.
    /// OIDC-Verknüpfung anlegen, ändern und löschen (S-05, AIGW-90). Standard `false`: ein
    /// gitops-Key ist damit nicht admin-gleichwertig; solche Änderungen lehnt der Plan mit
    /// `422` ab. Ein globaler `admin` und die CLI sind nicht betroffen.
    pub manage_users: bool,
}

/// Limits (AIGW-30, `docs/konzept/11-limits-und-budgets.md`).
///
/// Die Limits selbst stehen an den Ressourcen (Key, Team, Organisation, Modellgruppe); hier
/// steht nur, wie der Proxy sie anwendet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    /// Angenommene Ausgabe-Tokens für die Reservierung, wenn die Anfrage kein `max_tokens`
    /// enthält (Token-Limits und Deployment-Kapazität).
    pub default_max_tokens: u64,
    /// Antwort-Header `x-ratelimit-*` bzw. `anthropic-ratelimit-*` auch bei Erfolg setzen
    /// (bei `429` immer).
    pub response_headers: bool,
    /// Verzeichnis der Hugging-Face-`tokenizer.json`-Dateien für `settings.tokenizer:
    /// hf:<datei>` einer Modellgruppe (F46; ohne Angabe ist `hf:` wirkungslos).
    pub tokenizer_dir: Option<String>,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            default_max_tokens: 4096,
            response_headers: true,
            tokenizer_dir: None,
        }
    }
}

/// Rangtabellen der OpenAI-Kodierungen `o200k_base`/`cl100k_base` (AIGW-95,
/// `docs/konzept/11-limits-und-budgets.md` §9).
///
/// Nur wirksam in einem Build **ohne** Cargo-Feature `embedded-tokenizers` (Standard-Builds
/// enthalten die Tabellen). Fehlen die Tabellen, schätzt das Gateway Tokens mit der Heuristik
/// (Warnung beim Start).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TokenizerConfig {
    /// Verzeichnis mit `cl100k_base.tiktoken` und `o200k_base.tiktoken` (Dateinamen und
    /// Inhalt wie von tiktoken geladen; SHA-256 wird gegen eingebaute Prüfsummen geprüft).
    pub ranks_dir: Option<String>,
    /// Fehlende Dateien einmalig von der offiziellen Quelle
    /// (`https://openaipublic.blob.core.windows.net/encodings/`) laden und in `ranks_dir`
    /// ablegen. Ausgehend über den gemeinsamen Client mit den SSRF-Regeln aus
    /// `admin.allow_*_upstreams`. Standard `false` (kein Download).
    pub download_ranks: bool,
}

impl TokenizerConfig {
    /// Prüft die Kombination der Felder.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.download_ranks && self.ranks_dir.as_deref().is_none_or(str::is_empty) {
            return Err("tokenizer.download_ranks braucht tokenizer.ranks_dir".into());
        }
        Ok(())
    }
}

/// MCP-Gateway (AIGW-41, `docs/konzept/15-mcp-gateway.md`).
///
/// Die Upstream-MCP-Server und ihre Freigaben sind Ressourcen (`McpServer`, `McpGrant`); hier
/// steht nur, wie die Datenebene sie betreibt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpConfig {
    /// Endpunkte `/mcp` und `/mcp/{server}` auf der Datenebene anbieten.
    pub enabled: bool,
    /// Programme (absolute Pfade), die als stdio-MCP-Server gestartet werden dürfen. Leer =
    /// stdio aus (Standard). **Sicherheit:** ein stdio-Server läuft als Kindprozess mit den
    /// Rechten des Gateways – nur vertrauenswürdige Programme eintragen.
    pub allow_stdio_commands: Vec<String>,
    /// Client-Sessions (`Mcp-Session-Id`) enden nach so vielen Sekunden ohne Anfrage.
    pub session_idle_timeout_secs: u64,
    /// Höchstzahl gleichzeitiger Client-Sessions je Instanz.
    pub max_sessions: usize,
    /// Höchstzahl gleichzeitiger Client-Sessions je API-Key und Instanz (S-28).
    pub max_sessions_per_key: usize,
    /// Höchstzahl gleichzeitiger Client-Sessions je Team und Instanz (S-28).
    pub max_sessions_per_team: usize,
    /// Puffer für Benachrichtigungen je Session bzw. je `tools/call`-Strom (Nachrichten,
    /// S-28). Voll → älteste (Session) bzw. neue (`tools/call`) Meldungen werden verworfen
    /// und geloggt.
    pub notification_buffer: usize,
    /// Zeitlimit je Upstream-Anfrage (Sekunden), falls der Server keins setzt (`timeoutMs`).
    pub call_timeout_secs: u64,
    /// Zeitlimit für den Verbindungsaufbau zum Upstream (Sekunden).
    pub connect_timeout_secs: u64,
    /// Höchstgröße einer einzelnen Nachricht vom Upstream (Bytes).
    pub max_message_bytes: usize,
    /// Tool-, Prompt- und Ressourcenlisten der Upstreams so lange zwischenspeichern
    /// (Sekunden; `list_changed` leert sofort).
    pub list_cache_secs: u64,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_stdio_commands: Vec::new(),
            session_idle_timeout_secs: 3600,
            max_sessions: 10_000,
            max_sessions_per_key: 100,
            max_sessions_per_team: 1_000,
            notification_buffer: 64,
            call_timeout_secs: 120,
            connect_timeout_secs: 10,
            max_message_bytes: 16 * 1024 * 1024,
            list_cache_secs: 300,
        }
    }
}

/// Kostenabrechnung und Budgets (AIGW-31).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BillingConfig {
    /// Zeitzone der Budget-Zeiträume (Kalendertag/-woche/-monat), IANA-Name wie
    /// `Europe/Berlin` oder `UTC` (Standard).
    pub timezone: String,
    /// Abstand des Budget-Abgleichs mit der Datenbank (`budget_usage`) in Millisekunden.
    pub budget_sync_ms: u64,
    /// Preisvorschläge (AIGW-85): Azure Retail Prices API und mitgelieferter Preiskatalog.
    pub price_suggestions: PriceSuggestionsConfig,
    /// Abrechnungswährung als ISO-4217-Code (AIGW-117, Standard `USD` wie Preisvorschläge
    /// und Katalog). Nur Beschriftung in Kostenanalyse und Chargeback-Export – das Gateway
    /// rechnet nicht um; die Preise müssen in dieser Währung gepflegt sein.
    pub currency: String,
    /// Schlüssel in `spec.metadata` von Organisation, Team und API-Key, der die Kostenstelle
    /// trägt (AIGW-117, Chargeback-Export; Standard `costCenter`). Gesucht wird am Key, dann
    /// am Team, dann an der Organisation.
    pub cost_center_key: String,
}

impl Default for BillingConfig {
    fn default() -> Self {
        Self {
            timezone: "UTC".to_owned(),
            budget_sync_ms: 1000,
            price_suggestions: PriceSuggestionsConfig::default(),
            currency: "USD".to_owned(),
            cost_center_key: "costCenter".to_owned(),
        }
    }
}

impl BillingConfig {
    /// Prüft Währung und Kostenstellen-Schlüssel (AIGW-117).
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.currency.len() != 3 || !self.currency.bytes().all(|b| b.is_ascii_uppercase()) {
            return Err(format!(
                "billing.currency: ISO-4217-Code aus drei Großbuchstaben erwartet, nicht {:?}",
                self.currency
            ));
        }
        if self.cost_center_key.trim().is_empty() || self.cost_center_key.len() > 128 {
            return Err("billing.cost_center_key: 1–128 Zeichen".into());
        }
        Ok(())
    }
}

/// Preisvorschläge (AIGW-85, `billing.price_suggestions`).
///
/// Vorschläge sind nur Lesehilfen: übernommen wird erst per Admin-API
/// (`POST /admin/api/v1/price-suggestions:accept`), manuelle Preise haben Vorrang.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PriceSuggestionsConfig {
    /// Vorschläge anbieten (Standard `true`; ohne Netzwerkzugriff, solange niemand die Seite
    /// öffnet bzw. kein Abgleich eingestellt ist).
    pub enabled: bool,
    /// Azure Retail Prices API für Azure-OpenAI- und Foundry-Deployments.
    pub azure: AzureRetailConfig,
    /// Mitgelieferter Preiskatalog der direkten Anbieter, optional per URL aktualisiert.
    pub catalog: PriceCatalogConfig,
    /// Periodischer Abgleich in Sekunden (`0` = aus, Standard): Warnung im Log und in
    /// `GET /admin/api/v1/price-suggestions` bei abweichenden Preisen.
    pub check_interval_secs: u64,
}

impl Default for PriceSuggestionsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            azure: AzureRetailConfig::default(),
            catalog: PriceCatalogConfig::default(),
            check_interval_secs: 0,
        }
    }
}

/// Azure Retail Prices API (`billing.price_suggestions.azure`); öffentlich, ohne Anmeldung.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AzureRetailConfig {
    /// Abfrage aktiv (Standard `true`).
    pub enabled: bool,
    /// Adresse der API. Nur dieser Host wird abgefragt (auch für Folgeseiten); er ist von den
    /// SSRF-Regeln ausgenommen, weil er aus der Startkonfiguration des Betreibers stammt.
    pub api_url: String,
    /// `serviceName` der Modell-Meter (Standard `Foundry Models`).
    pub service_name: String,
    /// Region, wenn das Deployment keine nennt (`settings.pricing.region`) und sie sich nicht
    /// aus dem Endpunkt ablesen lässt, z. B. `swedencentral`.
    pub default_region: Option<String>,
    /// Bereitstellungsart ohne `settings.pricing.deploymentType`: `global` (GlobalStandard,
    /// Standard), `data-zone` (DataZone Standard) oder `regional` (Standard).
    pub default_deployment_type: String,
    /// Gültigkeit des Zwischenspeichers je Region in Sekunden (Standard 24 h). Bei Fehlern
    /// bleiben ältere Daten in Gebrauch.
    pub cache_ttl_secs: u64,
    /// Zeitlimit je Seite in Sekunden.
    pub timeout_secs: u64,
}

impl Default for AzureRetailConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            api_url: "https://prices.azure.com/api/retail/prices".to_owned(),
            service_name: "Foundry Models".to_owned(),
            default_region: None,
            default_deployment_type: "global".to_owned(),
            cache_ttl_secs: 86_400,
            timeout_secs: 30,
        }
    }
}

/// Preiskatalog (`billing.price_suggestions.catalog`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PriceCatalogConfig {
    /// Aktualisierung per URL (Standard aus): ersetzt den mitgelieferten Katalog, solange die
    /// Datei geladen werden kann.
    pub url: Option<String>,
    /// Format der URL: `aigw` (Format des mitgelieferten Katalogs, Standard) oder `litellm`
    /// (`model_prices_and_context_window.json`).
    pub format: String,
    /// Abstand der Aktualisierung in Sekunden (Standard 24 h).
    pub refresh_secs: u64,
}

impl Default for PriceCatalogConfig {
    fn default() -> Self {
        Self {
            url: None,
            format: "aigw".to_owned(),
            refresh_secs: 86_400,
        }
    }
}

/// Geheimnis eines Webhooks (HMAC-Schlüssel), **nie** als Klartext in der Konfiguration.
///
/// Genau eines von `credential` (Feld `field` einer `ProviderCredential`, Standard
/// `webhookSecret`; verschlüsselt in der Datenbank, Rotation ohne Neustart), `env`
/// (Umgebungsvariable) oder `file` (Datei, ohne abschließenden Zeilenumbruch).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebhookSecret {
    /// Name eines `Secret` (F149, bevorzugt).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// Name einer `ProviderCredential` (älterer Weg).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// Feld (Standard bei `secret`: `value`, bei `credential`:
    /// [`WebhookSecret::DEFAULT_FIELD`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// Umgebungsvariable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    /// Datei.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<PathBuf>,
}

impl WebhookSecret {
    /// Standardfeld einer `ProviderCredential`.
    pub const DEFAULT_FIELD: &'static str = "webhookSecret";

    /// Prüft, dass genau eine Quelle angegeben ist.
    pub fn validate(&self) -> std::result::Result<(), String> {
        let n = usize::from(self.secret.is_some())
            + usize::from(self.credential.is_some())
            + usize::from(self.env.is_some())
            + usize::from(self.file.is_some());
        if n != 1 {
            return Err("secret: genau eines von secret, credential, env oder file angeben".into());
        }
        if self.field.is_some() && self.credential.is_none() && self.secret.is_none() {
            return Err("secret.field gilt nur zusammen mit secret.secret/credential".into());
        }
        Ok(())
    }

    /// Feldname bei `secret` bzw. `credential`.
    pub fn field(&self) -> &str {
        match (&self.field, &self.secret) {
            (Some(f), _) => f,
            (None, Some(_)) => crate::resource::SecretSpec::DEFAULT_FIELD,
            (None, None) => Self::DEFAULT_FIELD,
        }
    }
}

/// Ereignis-Webhooks (AIGW-61, `docs/konzept/11-limits-und-budgets.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EventsConfig {
    /// Empfänger.
    pub webhooks: Vec<EventWebhookConfig>,
    /// Plätze im Puffer; ist er voll, werden neue Ereignisse verworfen (Zähler `dropped`).
    pub queue_capacity: usize,
    /// Gleichzeitige Zustellungen.
    pub concurrency: usize,
    /// Erste Wartezeit vor einer Wiederholung (verdoppelt sich, mit Zufallsanteil).
    pub retry_initial_ms: u64,
    /// Längste Wartezeit zwischen zwei Versuchen.
    pub retry_max_ms: u64,
    /// Ereignisse im Cluster nur einmal senden (Tabelle `event_dedup`).
    pub dedup: bool,
}

impl Default for EventsConfig {
    fn default() -> Self {
        Self {
            webhooks: Vec::new(),
            queue_capacity: 1024,
            concurrency: 8,
            retry_initial_ms: 1000,
            retry_max_ms: 60_000,
            dedup: true,
        }
    }
}

/// Ein Empfänger von Ereignissen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventWebhookConfig {
    /// Name (Log, Metriken).
    pub name: String,
    /// Ziel-URL (SSRF-Regeln wie Deployments, `admin.allow_*_upstreams`).
    pub url: String,
    /// Ereignistypen (z. B. `budget.warning`, `budget.exhausted`, Präfix `budget.*`);
    /// leer = alle.
    #[serde(default)]
    pub events: Vec<String>,
    /// HMAC-Schlüssel (ohne: unsigniert, Warnung beim Start).
    #[serde(default)]
    pub secret: Option<WebhookSecret>,
    /// Zeitlimit je Versuch.
    #[serde(default = "default_event_timeout_ms")]
    pub timeout_ms: u64,
    /// Versuche insgesamt (danach Dead-Letter-Zähler + Log).
    #[serde(default = "default_event_attempts")]
    pub max_attempts: u32,
}

fn default_event_timeout_ms() -> u64 {
    5000
}

fn default_event_attempts() -> u32 {
    6
}

/// Antwort-Cache (AIGW-39/40, `docs/konzept/14-cache.md`).
///
/// Hier steht nur die Infrastruktur (Speicher, Backends, Grenzen). **Wo** gecacht wird
/// (Ebene, Modus, TTL, Scope, Schwellen), legen Bindungen des Plugins `cache` fest; ohne
/// Bindung ist der Cache für eine Anfrage aus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    /// Hauptschalter; `false` ignoriert alle `cache`-Bindungen.
    pub enabled: bool,
    /// Obergrenze des RAM-LRU für exakte Treffer (Bytes, serialisierte Antworten).
    pub memory_max_bytes: u64,
    /// Größte cachebare Antwort (Bytes, serialisiert); größere werden nicht gespeichert.
    pub max_entry_bytes: u64,
    /// Obergrenze jeder TTL (Sekunden), auch für `x-aigw-cache-ttl` und Bindungen.
    pub max_ttl_secs: u64,
    /// Optionale zweite Ebene für exakte Treffer in Redis/Valkey (gemeinsam für alle
    /// Instanzen), z. B. `redis://redis:6379/0`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redis_url: Option<String>,
    /// Schlüsselpräfix in Redis.
    pub redis_key_prefix: String,
    /// Zeitlimit je Zugriff auf externe Backends (Redis, pgvector) in Millisekunden;
    /// Überschreitung = Cache-Miss.
    pub backend_timeout_ms: u64,
    /// Zeichen je simuliertem Chunk bei der Wiedergabe gecachter Antworten als Stream.
    pub replay_chunk_chars: usize,
    /// Semantischer Cache.
    pub semantic: SemanticCacheConfig,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            memory_max_bytes: 256 * 1024 * 1024,
            max_entry_bytes: 1024 * 1024,
            max_ttl_secs: 7 * 24 * 3600,
            redis_url: None,
            redis_key_prefix: "aigw:cache:".to_owned(),
            backend_timeout_ms: 250,
            replay_chunk_chars: 32,
            semantic: SemanticCacheConfig::default(),
        }
    }
}

/// Vektor-Backend des semantischen Caches (Architektur §10).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VectorBackendKind {
    /// Eigener HNSW-Index im RAM je Instanz (Warmstart aus der Datenbank).
    #[default]
    Memory,
    /// Postgres mit Erweiterung `vector` (gemeinsam für alle Instanzen).
    Pgvector,
    /// Redis Stack / Valkey Search (`FT.SEARCH` KNN, gemeinsam für alle Instanzen).
    Redis,
}

/// Semantischer Cache (AIGW-40).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SemanticCacheConfig {
    /// Vektor-Backend.
    pub backend: VectorBackendKind,
    /// Standard-Modellgruppe für Embeddings (Bindungen können eine andere angeben). Ohne
    /// Angabe ist der semantische Modus nur mit `semantic.embeddingModel` der Bindung aktiv.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_model: Option<String>,
    /// Dimension der Embeddings (Pflicht für `pgvector`/`redis`: Spaltentyp bzw. Index;
    /// `memory` übernimmt sie aus dem ersten Vektor). Vektoren anderer Länge werden
    /// ignoriert.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<u32>,
    /// Zeitlimit eines Embedding-Aufrufs (Millisekunden); Überschreitung = Cache-Miss.
    pub embed_timeout_ms: u64,
    /// Höchstzahl Vektoren (`memory`; älteste werden verdrängt).
    pub max_entries: u64,
    /// Obergrenze für Vektoren und Antworten im RAM (`memory`, Bytes).
    pub max_bytes: u64,
    /// HNSW-Parameter (`memory`, beim Anlegen auch `pgvector`/`redis`).
    pub hnsw: HnswConfig,
    /// `memory`: Einträge (verschlüsselt mit dem Master-Key) in der Datenbank ablegen und
    /// beim Start laden (Warmstart).
    pub persist: bool,
    /// `pgvector`: Datenbank-URL (Standard: `store.url`, falls Postgres).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pgvector_url: Option<String>,
    /// `redis`: URL von Redis Stack bzw. Valkey mit Search-Modul (Standard: `cache.redis_url`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redis_url: Option<String>,
}

impl Default for SemanticCacheConfig {
    fn default() -> Self {
        Self {
            backend: VectorBackendKind::Memory,
            embedding_model: None,
            dimensions: None,
            embed_timeout_ms: 2000,
            max_entries: 100_000,
            max_bytes: 512 * 1024 * 1024,
            hnsw: HnswConfig::default(),
            persist: true,
            pgvector_url: None,
            redis_url: None,
        }
    }
}

/// Parameter des HNSW-Index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HnswConfig {
    /// Nachbarn je Knoten und Ebene (Ebene 0: 2·M).
    pub m: usize,
    /// Kandidatenliste beim Einfügen.
    pub ef_construction: usize,
    /// Kandidatenliste bei der Suche.
    pub ef_search: usize,
}

impl Default for HnswConfig {
    fn default() -> Self {
        Self {
            m: 16,
            ef_construction: 200,
            ef_search: 64,
        }
    }
}

/// Logging.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    /// Filter im Format von `RUST_LOG` (z. B. `info,aigw_proxy=debug`).
    pub level: String,
    /// Ausgabeformat.
    pub format: LogFormat,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".to_owned(),
            format: LogFormat::Text,
        }
    }
}

/// Vertrauensanker für ausgehende TLS-Verbindungen (AIGW-72, F48).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RootsKind {
    /// Mozilla-Wurzelzertifikate (`webpki-roots`, im Binary eingebettet; unabhängig vom
    /// Container-Image). Standard.
    #[default]
    Webpki,
    /// Zertifikatsspeicher des Betriebssystems.
    System,
    /// Beide.
    Both,
}

/// Ausgehende Verbindungen aller Clients (Anbieter, Webhooks, MCP, OIDC, Guardrail-Dienste,
/// OTLP; AIGW-72, `docs/konzept/06-streaming-und-anbieter.md` §4).
///
/// Ein Deployment kann mit `settings.caFile` zusätzlich eine eigene CA-Datei angeben, der
/// OIDC-Client mit `admin.auth.oidc.ca_file`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpstreamConfig {
    /// Wurzelzertifikate: `webpki` (Standard), `system` oder `both`.
    pub roots: RootsKind,
    /// Zusätzliche CA-Zertifikate (PEM) für alle ausgehenden Verbindungen, z. B. eine interne CA.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ca_file: Option<PathBuf>,
    /// Verzeichnis, aus dem Deployments eigene CA-Dateien laden dürfen (`settings.caFile`,
    /// F177). Ohne ist `settings.caFile` nicht erlaubt. Pfade werden normalisiert (relativ =
    /// innerhalb dieses Verzeichnisses), Symlinks, die hinausführen, werden abgelehnt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ca_dir: Option<PathBuf>,
    /// Entra-ID-Anmeldung an Anbieter (AIGW-82/84/90).
    pub entra: UpstreamEntraConfig,
}

/// Entra-ID-Anmeldung an Anbieter (`upstream.entra`, AIGW-90).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpstreamEntraConfig {
    /// Zugangsdaten bzw. Deployments mit `workloadIdentity`/`managedIdentity` dürfen die
    /// Identität des Gateway-Prozesses nutzen (Managed Identity der VM/Container App,
    /// föderiertes Token des Pods). Standard `false`: solche Deployments sind nicht nutzbar
    /// (Fehler beim Routenaufbau, Warnung beim Start) – wer Deployments oder Zugangsdaten
    /// anlegen darf, kann sonst Tokens der Gateway-Identität anfordern lassen.
    pub ambient_identity: bool,
}

/// Einstellungen der Admin-API (AIGW-65, `docs/konzept/08-admin-api.md`).
///
/// Endpunkte von Deployments werden gegen SSRF-Grundregeln geprüft: nur `https`, keine
/// privaten/lokalen Adressen. Ausnahmen (z. B. Ollama/vLLM im LAN) werden hier freigeschaltet.
/// Einträge: Hostname (`ollama`, `localhost`), Muster `*.lan`, IP-Adresse oder CIDR-Bereich
/// (`192.168.0.0/16`, `fd00::/8`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdminConfig {
    /// Hosts, die per `http://` (ohne TLS) angesprochen werden dürfen.
    pub allow_insecure_upstreams: Vec<String>,
    /// Private, lokale oder interne Ziele, die trotzdem erlaubt sind.
    pub allow_private_upstreams: Vec<String>,
    /// Deployments mit `provider: mock` (eingebauter Echo-Anbieter) erlauben (Standard `true`,
    /// F85). `false`: Admin-API und GitOps lehnen sie ab, das Routing lässt vorhandene aus.
    pub allow_mock: bool,
    /// Anmeldung an Admin-API und Admin-UI (AIGW-32).
    pub auth: AdminAuthConfig,
    /// Einstellungen der Admin-UI (AIGW-77).
    pub ui: AdminUiConfig,
}

/// Einstellungen der Admin-UI (AIGW-77, `docs/konzept/04-ux-designsystem.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdminUiConfig {
    /// Mock-Modus (Beispieldaten im Browser) anbieten (Standard `true`, F135). `false` blendet
    /// den Schalter aus und ignoriert `?mock=1` – für Produktivinstallationen.
    pub mock: bool,
    /// Link-Vorlage zum Tracing-System für den Log-Explorer, `{traceId}` wird ersetzt
    /// (z. B. `https://grafana.example.org/explore?traceId={traceId}`); ohne = kein Link.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_url: Option<String>,
}

impl Default for AdminUiConfig {
    fn default() -> Self {
        Self {
            mock: true,
            trace_url: None,
        }
    }
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            allow_insecure_upstreams: Vec::new(),
            allow_private_upstreams: Vec::new(),
            allow_mock: true,
            auth: AdminAuthConfig::default(),
            ui: AdminUiConfig::default(),
        }
    }
}

/// Request-Log und Auswertung (AIGW-43, `docs/konzept/07-beobachtung.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ObserveConfig {
    /// Rohlog je Anfrage in `request_log` schreiben (unabhängig von `rollups`).
    pub request_log: bool,
    /// Minuten-/Stunden-Rollups für Dashboards und Analytics schreiben (F33, Standard an).
    /// Aus: Dashboards bleiben leer, Rohlog, Export und Metriken laufen weiter.
    pub rollups: bool,
    /// Kapazität des Puffers zwischen Proxy und Writer (Einträge); voll → verwerfen + zählen.
    pub queue_capacity: usize,
    /// Höchstzahl Einträge je Batch (Schreiben spätestens bei Erreichen).
    pub batch_size: usize,
    /// Schreiben spätestens nach dieser Zeit (Millisekunden).
    pub flush_interval_ms: u64,
    /// Höchstdauer für das Leeren des Puffers beim Herunterfahren (Millisekunden).
    pub shutdown_timeout_ms: u64,
    /// Payload-Logging: `off` | `metadata` | `masked` | `full` – Standard für alle Anfragen;
    /// je Organisation/Team/Key überschreibbar (`payloadMode`, F30).
    pub payload: PayloadMode,
    /// Payload-Bodies werden auf diese Größe gekürzt (Bytes, je Request und Response).
    pub payload_max_bytes: usize,
    /// Aufbewahrungsfristen.
    pub retention: RetentionConfig,
    /// Export als JSON-Lines-Datei.
    pub jsonl: JsonlExportConfig,
    /// Prometheus-Metriken (`/metrics`, AIGW-44).
    pub metrics: MetricsConfig,
    /// OpenTelemetry-Export (OTLP/HTTP): Traces und Request-Log als Logs (AIGW-45).
    pub otlp: OtlpConfig,
}

impl Default for ObserveConfig {
    fn default() -> Self {
        Self {
            request_log: true,
            rollups: true,
            queue_capacity: 16_384,
            batch_size: 1000,
            flush_interval_ms: 500,
            shutdown_timeout_ms: 5000,
            payload: PayloadMode::Off,
            payload_max_bytes: 64 * 1024,
            retention: RetentionConfig::default(),
            jsonl: JsonlExportConfig::default(),
            metrics: MetricsConfig::default(),
            otlp: OtlpConfig::default(),
        }
    }
}

/// Aufbewahrungsfristen in Tagen (`0` = unbegrenzt).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionConfig {
    /// Rohlogs (`request_log`).
    pub request_log_days: u32,
    /// Payloads (`request_payload`), getrennt befristet; höchstens so lange wie die Rohlogs.
    pub payload_days: u32,
    /// Minuten-Rollups (`usage_rollup`).
    pub rollup_days: u32,
    /// Abstand der Löschläufe (Sekunden).
    pub sweep_interval_secs: u64,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            request_log_days: 30,
            payload_days: 7,
            rollup_days: 400,
            sweep_interval_secs: 3600,
        }
    }
}

/// Export des Request-Logs als JSON-Lines-Datei (ein JSON-Objekt je Zeile, angehängt).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JsonlExportConfig {
    /// Export aktiv.
    pub enabled: bool,
    /// Zieldatei.
    pub path: PathBuf,
    /// Payload (sofern geloggt, also bereits maskiert) mit exportieren.
    pub include_payload: bool,
    /// Rotation (F31).
    pub rotate: JsonlRotateConfig,
}

impl Default for JsonlExportConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: PathBuf::from("aigw-requests.jsonl"),
            include_payload: false,
            rotate: JsonlRotateConfig::default(),
        }
    }
}

/// Rotation der JSON-Lines-Datei (F31). Rotiert wird beim Tageswechsel (UTC, `daily`) und/oder
/// sobald die Datei `max_size_mb` überschreiten würde; die rotierte Datei heißt
/// `<pfad>.<UTC-Zeitstempel>` (mit `gzip` zusätzlich `.gz`). Unabhängig davon öffnet `SIGHUP`
/// die Datei neu (für externes logrotate).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JsonlRotateConfig {
    /// Beim Tageswechsel (UTC) rotieren.
    pub daily: bool,
    /// Höchstgröße in MiB (`0` = ohne Größengrenze).
    pub max_size_mb: u64,
    /// So viele rotierte Dateien behalten, ältere löschen (`0` = alle behalten).
    pub keep: usize,
    /// Rotierte Dateien mit gzip komprimieren.
    pub gzip: bool,
}

impl JsonlRotateConfig {
    /// `true`, wenn überhaupt rotiert wird.
    pub fn enabled(&self) -> bool {
        self.daily || self.max_size_mb > 0
    }
}

// ---------------------------------------------------------------------------------------------
// Admin-API: Anmeldung (AIGW-32, docs/konzept/08-admin-api.md §5)
// ---------------------------------------------------------------------------------------------

/// Anmeldung an der Admin-API: OIDC-Login mit Session, Admin-API-Keys, Bootstrap-Token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdminAuthConfig {
    /// Bootstrap-Token für die Erstinstallation: `auto` (Standard; aus, sobald ein OIDC-Admin
    /// sich angemeldet hat oder ein Admin-API-Key mit Rolle `admin` existiert), `on`, `off`.
    pub bootstrap: BootstrapMode,
    /// Sessions nach dem OIDC-Login.
    pub session: SessionConfig,
    /// OIDC-Login (Authorization Code + PKCE); ohne `issuer` abgeschaltet.
    pub oidc: OidcConfig,
    /// Anmelden und Abmelden nur mit Audit-Eintrag (F113, Standard `true`): Session und
    /// Eintrag entstehen in derselben Transaktion; scheitert der Eintrag, scheitert die
    /// Anmeldung (fail-closed). `false`: Eintrag nach bestem Bemühen (Warnung im Log).
    pub audit_required: bool,
    /// Höchste Laufzeit von Admin-API-Keys (F87), z. B. `90d`, `12h` (Einheiten `s`, `m`,
    /// `h`, `d`). Ohne Angabe unbegrenzt. Mit Angabe ist `expiresAt` Pflicht-Obergrenze:
    /// fehlt sie beim Anlegen/Ändern, setzt der Server jetzt + Höchstlaufzeit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_admin_key_lifetime: Option<String>,
}

impl Default for AdminAuthConfig {
    fn default() -> Self {
        Self {
            bootstrap: BootstrapMode::default(),
            session: SessionConfig::default(),
            oidc: OidcConfig::default(),
            audit_required: true,
            max_admin_key_lifetime: None,
        }
    }
}

impl AdminAuthConfig {
    /// `max_admin_key_lifetime` in Sekunden (`None` = unbegrenzt).
    pub fn max_admin_key_lifetime_secs(&self) -> std::result::Result<Option<u64>, String> {
        self.max_admin_key_lifetime
            .as_deref()
            .map(|s| {
                parse_duration_secs(s)
                    .map_err(|e| format!("admin.auth.max_admin_key_lifetime: {e}"))
            })
            .transpose()
    }
}

/// Dauer als `<Zahl><Einheit>` (`s`, `m`, `h`, `d`; ohne Einheit Sekunden), > 0.
pub fn parse_duration_secs(text: &str) -> std::result::Result<u64, String> {
    let t = text.trim();
    let (num, mult) = match t.char_indices().last() {
        Some((i, 's')) => (&t[..i], 1),
        Some((i, 'm')) => (&t[..i], 60),
        Some((i, 'h')) => (&t[..i], 3600),
        Some((i, 'd')) => (&t[..i], 86_400),
        _ => (t, 1),
    };
    num.trim()
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .and_then(|n| n.checked_mul(mult))
        .ok_or_else(|| format!("Dauer {text:?} ungültig (z. B. \"90d\", \"12h\")"))
}

/// Profil des Identitätsanbieters (F90): bestimmt die unveränderliche Nutzerkennung.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OidcProfile {
    /// Beliebiger IdP: Kennung = `identity_claim` (Standard `sub`).
    #[default]
    Generic,
    /// Microsoft Entra ID: Kennung = `oid` (über alle Anwendungen des Tenants stabil), der
    /// Tenant (`tid`) muss `tenant_id` entsprechen.
    Entra,
}

/// Verhalten des Bootstrap-Tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BootstrapMode {
    /// Aktiv, bis ein OIDC-Admin sich angemeldet hat oder ein Admin-API-Key mit Rolle `admin`
    /// angelegt wurde (danach dauerhaft aus; wieder einschalten nur mit `on`).
    #[default]
    Auto,
    /// Immer aktiv.
    On,
    /// Nie aktiv.
    Off,
}

/// Sessions der Admin-UI (serverseitig gespeichert, Cookie mit zufälliger Session-ID).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionConfig {
    /// Höchste Lebensdauer einer Session ab Anmeldung (Sekunden, Standard 8 h).
    pub max_age_secs: u64,
    /// Abmeldung nach so langer Inaktivität (Sekunden, Standard 30 min).
    pub idle_timeout_secs: u64,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            max_age_secs: 8 * 3600,
            idle_timeout_secs: 30 * 60,
        }
    }
}

/// OIDC-Relying-Party (Authorization Code + PKCE, beliebiger IdP).
///
/// Das Client-Secret steht nie in dieser Datei: es kommt aus `client_secret_file` oder der
/// Umgebungsvariable `client_secret_env`; fehlt beides, meldet sich ai-gw als öffentlicher
/// Client (nur PKCE) an.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OidcConfig {
    /// Issuer-URL (Discovery über `<issuer>/.well-known/openid-configuration`); leer = aus.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    /// Client-ID bei diesem IdP.
    pub client_id: String,
    /// Datei mit dem Client-Secret (vertraulicher Client).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret_file: Option<PathBuf>,
    /// Umgebungsvariable mit dem Client-Secret (wenn keine Datei angegeben ist).
    pub client_secret_env: String,
    /// Öffentliche Rücksprung-URL (`https://…/admin/auth/callback`), beim IdP registriert.
    /// Ihr Ursprung gilt zugleich als erlaubter `Origin` schreibender Anfragen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect_url: Option<String>,
    /// Angeforderte Scopes (`openid` wird immer ergänzt).
    pub scopes: Vec<String>,
    /// Claim mit den Gruppen des Nutzers; Punkte trennen verschachtelte Objekte
    /// (z. B. `realm_access.roles`).
    pub groups_claim: String,
    /// Zuordnung IdP-Gruppe → Rolle (optional eingeschränkt auf Organisation/Team).
    pub group_roles: Vec<GroupRoleMapping>,
    /// Unbekannte Nutzer beim ersten Login anlegen, sofern ihnen über Gruppen eine Rolle
    /// zugeordnet ist.
    pub provision_users: bool,
    /// Beim ersten Login einen vorab angelegten Nutzer ohne OIDC-Kennung über die (vom IdP
    /// bestätigte) E-Mail-Adresse verknüpfen. Standard aus (F90): E-Mail-Adressen sind beim
    /// IdP oft änderbar; sicherer sind vorab eingetragene Kennungen oder Einladungslinks.
    pub link_by_email: bool,
    /// Profil des IdP: `generic` (Standard) oder `entra` (Microsoft Entra ID, F90).
    pub profile: OidcProfile,
    /// Claim mit der unveränderlichen Nutzerkennung (zusammen mit dem Issuer, F90). Ohne
    /// Angabe: `sub` (`generic`) bzw. `oid` (`entra`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity_claim: Option<String>,
    /// Profil `entra`: erwarteter Tenant (`tid`, GUID) – Pflicht; Tokens anderer Tenants
    /// werden abgelehnt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    /// Gültigkeit eines Einladungslinks (Sekunden, Standard 7 Tage, F90).
    pub invite_ttl_secs: u64,
    /// Abmeldung auch beim IdP (RP-initiated Logout), falls er `end_session_endpoint` anbietet.
    pub end_session: bool,
    /// Ziel nach der Abmeldung beim IdP (Standard: Ursprung von `redirect_url` + `/admin/`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_logout_redirect_url: Option<String>,
    /// Zusätzliche CA-Zertifikate (PEM) für den IdP, z. B. eine interne CA.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ca_file: Option<PathBuf>,
    /// Gültigkeit von Discovery-Dokument und JWKS im Speicher (Sekunden).
    pub cache_secs: u64,
    /// Zeitlimit je Anfrage an den IdP (Sekunden).
    pub http_timeout_secs: u64,
    /// Erlaubte Uhrenabweichung bei `exp`/`iat` (Sekunden).
    pub clock_skew_secs: u64,
}

impl Default for OidcConfig {
    fn default() -> Self {
        Self {
            issuer: None,
            client_id: String::new(),
            client_secret_file: None,
            client_secret_env: "AIGW_OIDC_CLIENT_SECRET".to_owned(),
            redirect_url: None,
            scopes: vec!["openid".into(), "profile".into(), "email".into()],
            groups_claim: "groups".to_owned(),
            group_roles: Vec::new(),
            provision_users: true,
            link_by_email: false,
            profile: OidcProfile::Generic,
            identity_claim: None,
            tenant_id: None,
            invite_ttl_secs: 7 * 86_400,
            end_session: true,
            post_logout_redirect_url: None,
            ca_file: None,
            cache_secs: 3600,
            http_timeout_secs: 10,
            clock_skew_secs: 60,
        }
    }
}

impl OidcConfig {
    /// `true`, wenn OIDC-Login konfiguriert ist.
    pub fn enabled(&self) -> bool {
        self.issuer.as_deref().is_some_and(|s| !s.trim().is_empty())
    }

    /// Claim mit der Nutzerkennung (`identity_claim`, sonst Standard des Profils).
    pub fn effective_identity_claim(&self) -> &str {
        match (&self.identity_claim, self.profile) {
            (Some(c), _) if !c.trim().is_empty() => c.trim(),
            (_, OidcProfile::Entra) => "oid",
            (_, OidcProfile::Generic) => "sub",
        }
    }
}

/// Zuordnung einer IdP-Gruppe zu einer Rolle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GroupRoleMapping {
    /// Gruppenname (bzw. -ID) wie im Claim.
    pub group: String,
    /// Rolle.
    pub role: Role,
    /// Organisation (Name), falls eingeschränkt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    /// Team (Name), falls eingeschränkt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
}

/// Prometheus-Metriken des Gateways (AIGW-44, `docs/konzept/07-beobachtung.md` §11).
///
/// Labels sind auf Modellgruppe, Deployment, Anbieter, Eingangsformat und Statusklasse
/// begrenzt (keine Key-, Team- oder Nutzer-IDs). Werte jenseits der Obergrenzen landen im
/// Sammelwert `other`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// Gateway-Metriken erfassen (die `aigw_log_*`-Zähler gibt es immer).
    pub enabled: bool,
    /// Höchstzahl unterschiedlicher Werte für das Label `model_group`.
    pub max_model_groups: usize,
    /// Höchstzahl unterschiedlicher Werte für das Label `deployment`.
    pub max_deployments: usize,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_model_groups: 200,
            max_deployments: 500,
        }
    }
}

/// OpenTelemetry-Export über OTLP/HTTP mit Protobuf (AIGW-45).
///
/// `endpoint` ist die Basis-URL des Collectors (wie `OTEL_EXPORTER_OTLP_ENDPOINT`); die Pfade
/// `/v1/traces` und `/v1/logs` werden angehängt. Leer = kein Export.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OtlpConfig {
    /// Basis-URL, z. B. `http://otel-collector:4318`.
    pub endpoint: String,
    /// Zusätzliche HTTP-Header (z. B. `authorization`); Werte erscheinen nicht in `Debug`.
    pub headers: BTreeMap<String, String>,
    /// Zeitlimit je Export-Aufruf (Millisekunden).
    pub timeout_ms: u64,
    /// `service.name` der Ressource.
    pub service_name: String,
    /// Traces.
    pub traces: OtlpTracesConfig,
    /// Request-Log als OTLP-Logs.
    pub logs: OtlpLogsConfig,
    /// Wiederholung bei `429`/`502`/`503`/`504` und Verbindungsfehlern (F102).
    pub retry: OtlpRetryConfig,
}

impl Default for OtlpConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            headers: BTreeMap::new(),
            timeout_ms: 10_000,
            service_name: "aigw".to_owned(),
            traces: OtlpTracesConfig::default(),
            logs: OtlpLogsConfig::default(),
            retry: OtlpRetryConfig::default(),
        }
    }
}

/// Wiederholung fehlgeschlagener OTLP-Exporte mit exponentiellem Backoff (F102).
///
/// Wiederholt werden nur vorübergehende Fehler (`429`, `502`, `503`, `504`, Verbindungsfehler,
/// Zeitlimit). Ein `Retry-After` des Collectors wird beachtet; verlangt er länger als
/// `max_backoff_ms`, wird der Batch verworfen (gezählt) statt den Export zu blockieren.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OtlpRetryConfig {
    /// Höchstzahl der Versuche je Batch inklusive des ersten (1 = keine Wiederholung).
    pub max_attempts: u32,
    /// Wartezeit vor der ersten Wiederholung; verdoppelt sich je Versuch (mit Jitter).
    pub initial_backoff_ms: u64,
    /// Längste Wartezeit zwischen zwei Versuchen (auch Obergrenze für `Retry-After`).
    pub max_backoff_ms: u64,
}

impl Default for OtlpRetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            initial_backoff_ms: 500,
            max_backoff_ms: 5_000,
        }
    }
}

impl fmt::Debug for OtlpConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers: Vec<&str> = self.headers.keys().map(String::as_str).collect();
        f.debug_struct("OtlpConfig")
            .field("endpoint", &self.endpoint)
            .field("headers", &headers)
            .field("timeout_ms", &self.timeout_ms)
            .field("service_name", &self.service_name)
            .field("traces", &self.traces)
            .field("logs", &self.logs)
            .field("retry", &self.retry)
            .finish()
    }
}

impl OtlpConfig {
    /// `true`, wenn ein Collector konfiguriert ist.
    pub fn has_endpoint(&self) -> bool {
        !self.endpoint.trim().is_empty()
    }

    /// Traces werden erzeugt und exportiert.
    pub fn traces_active(&self) -> bool {
        self.has_endpoint() && self.traces.enabled
    }

    /// Das Request-Log wird als OTLP-Logs exportiert.
    pub fn logs_active(&self) -> bool {
        self.has_endpoint() && self.logs.enabled
    }
}

/// Traces (Spans je Anfrage mit GenAI-Semantic-Conventions).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OtlpTracesConfig {
    /// Traces erzeugen und exportieren (Standard: aus).
    pub enabled: bool,
    /// Anteil der Anfragen ohne übernommene Sampling-Entscheidung, die aufgezeichnet werden
    /// (0.0–1.0, deterministisch nach Trace-ID).
    pub sampling_ratio: SamplingRatio,
    /// Sampling-Entscheidung eines eingehenden `traceparent` (Flag `sampled`) übernehmen.
    pub parent_based: bool,
    /// Eingehenden W3C-`traceparent` übernehmen (sonst beginnt jede Anfrage einen neuen Trace).
    pub accept_traceparent: bool,
    /// `traceparent` an den Upstream weitergeben.
    pub propagate: bool,
    /// Puffer fertiger Spans (voll → verwerfen + zählen).
    pub queue_capacity: usize,
    /// Höchstzahl Spans je Export.
    pub batch_size: usize,
    /// Export spätestens nach dieser Zeit (Millisekunden).
    pub flush_interval_ms: u64,
}

impl Default for OtlpTracesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sampling_ratio: SamplingRatio::ALWAYS,
            parent_based: true,
            accept_traceparent: true,
            propagate: true,
            queue_capacity: 8192,
            batch_size: 512,
            flush_interval_ms: 1000,
        }
    }
}

/// Request-Log als OTLP-Logs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OtlpLogsConfig {
    /// Export aktiv (Standard: aus).
    pub enabled: bool,
    /// Payload (sofern geloggt, also bereits maskiert) als Body mitsenden.
    pub include_payload: bool,
}

/// Sampling-Anteil 0.0–1.0.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(transparent)]
pub struct SamplingRatio(f64);

impl SamplingRatio {
    /// Alles aufzeichnen.
    pub const ALWAYS: Self = Self(1.0);
    /// Nichts aufzeichnen.
    pub const NEVER: Self = Self(0.0);

    /// Anteil (nur 0.0–1.0).
    pub fn new(ratio: f64) -> Option<Self> {
        (0.0..=1.0).contains(&ratio).then_some(Self(ratio))
    }

    /// Wert.
    pub fn get(self) -> f64 {
        self.0
    }
}

impl Default for SamplingRatio {
    fn default() -> Self {
        Self::ALWAYS
    }
}

impl PartialEq for SamplingRatio {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_bits() == other.0.to_bits()
    }
}

impl Eq for SamplingRatio {}

impl<'de> Deserialize<'de> for SamplingRatio {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let v = f64::deserialize(d)?;
        Self::new(v).ok_or_else(|| {
            serde::de::Error::custom(format!("sampling_ratio muss zwischen 0 und 1 liegen: {v}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn parses_partial_config_with_defaults() {
        let cfg = Config::from_yaml(
            r#"
server:
  data:
    listen: "0.0.0.0:8080"
counters:
  backend: postgres-sync
"#,
        )
        .unwrap();
        assert_eq!(cfg.server.data.listen.port(), 8080);
        assert_eq!(cfg.server.control.listen.port(), 4001);
        assert!(!cfg.server.is_combined());
        assert_eq!(cfg.server.data.max_body_bytes, DEFAULT_MAX_BODY_BYTES);
        assert_eq!(cfg.counters.backend, CounterBackendKind::PostgresSync);
        assert_eq!(cfg.store, StoreConfig::default());
        assert_eq!(cfg.secrets.master_key_env, "AIGW_MASTER_KEY");
    }

    #[test]
    fn empty_file_is_default() {
        assert_eq!(Config::from_yaml("").unwrap(), Config::default());
        assert_eq!(
            Config::from_yaml("# nur Kommentar\n").unwrap(),
            Config::default()
        );
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(Config::from_yaml("server:\n  listen_typo: 1\n").is_err());
        // `server.listen` (vor AIGW-13) wird bewusst nicht mehr akzeptiert.
        assert!(Config::from_yaml("server:\n  listen: \"127.0.0.1:4000\"\n").is_err());
        assert!(Config::from_yaml("server:\n  control:\n    max_body_bytes: 1\n").is_err());
        assert!(Config::from_yaml("unknown_section: {}\n").is_err());
        assert!(Config::from_yaml("counters:\n  backend: memcached\n").is_err());
        // F26/F33/F31: neue Felder mit Standardwerten, per YAML und ENV
        let d = Config::default();
        assert_eq!(d.counters.sync_interval_ms, 1000);
        assert_eq!(d.counters.redis_pool_size, 4);
        assert_eq!(d.counters.redis_timeout_ms, 250);
        assert!(d.observe.rollups);
        assert!(!d.observe.jsonl.rotate.enabled());
        let cfg = Config::from_yaml_with_env(
            "counters:\n  backend: postgres-sync\n  sync_interval_ms: 2000\n\
             observe:\n  rollups: false\n  jsonl:\n    rotate: {daily: true, keep: 7, gzip: true}\n",
            [
                ("AIGW_COUNTERS__REDIS_POOL_SIZE".to_owned(), "8".to_owned()),
                ("AIGW_COUNTERS__REDIS_TIMEOUT_MS".to_owned(), "100".to_owned()),
                (
                    "AIGW_OBSERVE__JSONL__ROTATE__MAX_SIZE_MB".to_owned(),
                    "64".to_owned(),
                ),
            ],
        )
        .unwrap();
        assert_eq!(cfg.counters.sync_interval_ms, 2000);
        assert_eq!(cfg.counters.redis_pool_size, 8);
        assert_eq!(cfg.counters.redis_timeout_ms, 100);
        assert!(!cfg.observe.rollups);
        let r = &cfg.observe.jsonl.rotate;
        assert!(r.daily && r.gzip && r.keep == 7 && r.max_size_mb == 64 && r.enabled());
    }

    #[test]
    fn tokenizer_section() {
        let cfg = Config::from_yaml_with_env(
            "tokenizer:\n  download_ranks: true\n",
            env(&[("AIGW_TOKENIZER__RANKS_DIR", "/var/lib/aigw/tiktoken")]),
        )
        .unwrap();
        assert_eq!(
            cfg.tokenizer.ranks_dir.as_deref(),
            Some("/var/lib/aigw/tiktoken")
        );
        assert!(cfg.tokenizer.download_ranks);
        cfg.tokenizer.validate().unwrap();
        // AIGW-117: Abrechnungswährung und Kostenstellen-Schlüssel.
        cfg.billing.validate().unwrap();
        assert_eq!(cfg.billing.currency, "USD");
        let mut b = cfg.billing.clone();
        b.currency = "eur".into();
        assert!(b.validate().is_err());
        let only_download = TokenizerConfig {
            ranks_dir: None,
            download_ranks: true,
        };
        assert!(only_download.validate().is_err());
    }

    #[test]
    fn example_configs_parse() {
        for text in [
            include_str!("../../../config/aigw.example.yaml"),
            include_str!("../../../config/aigw.container.yaml"),
        ] {
            Config::from_yaml(text).unwrap();
        }
    }

    #[test]
    fn env_overrides() {
        let cfg = Config::from_yaml_with_env(
            "store:\n  url: \"sqlite::memory:\"\n",
            env(&[
                ("AIGW_STORE__URL", "postgres://db/aigw"),
                ("AIGW_STORE__MAX_CONNECTIONS", "42"),
                ("AIGW_SERVER__DATA__LISTEN", "0.0.0.0:9000"),
                ("AIGW_SERVER__DATA__MAX_BODY_BYTES", "1024"),
                ("AIGW_SERVER__CONTROL__LISTEN", "0.0.0.0:9001"),
                ("AIGW_SERVER__CONTROL__MAX_CONNECTIONS", "64"),
                ("AIGW_SERVER__HTTP__HEADER_READ_TIMEOUT_SECS", "5"),
                ("AIGW_COUNTERS__BACKEND", "redis"),
                ("AIGW_COUNTERS__REDIS_URL", "redis://r:6379"),
                ("AIGW_SECRETS__MASTER_KEY_FILE", "/run/secrets/aigw-key"),
                ("AIGW_SECRETS__PREVIOUS_KEY_FILES", "/a, /b"),
                ("AIGW_LOG__FORMAT", "json"),
                // ohne `__`: gehört nicht zur Datei-Konfiguration
                ("AIGW_LOG", "debug"),
                ("AIGW_CONFIG", "/etc/aigw/aigw.yaml"),
                ("OTHER__X", "1"),
            ]),
        )
        .unwrap();
        assert_eq!(cfg.store.url, "postgres://db/aigw");
        assert_eq!(cfg.store.max_connections, 42);
        assert_eq!(cfg.server.data.listen.port(), 9000);
        assert_eq!(cfg.server.data.max_body_bytes, 1024);
        assert_eq!(cfg.server.control.listen.port(), 9001);
        assert_eq!(cfg.server.control.max_connections, 64);
        assert_eq!(cfg.server.http.header_read_timeout_secs, 5);
        assert_eq!(cfg.server.http.idle_timeout_secs, 120);
        assert_eq!(cfg.counters.backend, CounterBackendKind::Redis);
        assert_eq!(cfg.counters.redis_url.as_deref(), Some("redis://r:6379"));
        assert_eq!(
            cfg.secrets.master_key_file.as_deref(),
            Some(Path::new("/run/secrets/aigw-key"))
        );
        assert_eq!(cfg.secrets.previous_key_files.len(), 2);
        assert_eq!(cfg.log.format, LogFormat::Json);
        assert_eq!(cfg.log.level, "info");
    }

    #[test]
    fn env_overrides_reject_typos_and_bad_values() {
        let bad = |k: &str, v: &str| Config::from_yaml_with_env("", env(&[(k, v)])).unwrap_err();
        assert!(
            bad("AIGW_STOR__URL", "x")
                .to_string()
                .contains("AIGW_STOR__URL")
        );
        assert!(bad("AIGW_STORE__URLL", "x").to_string().contains("urll"));
        assert!(
            bad("AIGW_STORE__MAX_CONNECTIONS", "viele")
                .to_string()
                .contains("Zahl")
        );
        assert!(
            bad("AIGW_COUNTERS__BACKEND", "memcached")
                .to_string()
                .contains("Umgebung")
        );
    }

    #[test]
    fn observe_section() {
        let cfg = Config::from_yaml_with_env(
            "observe:\n  payload: masked\n  retention:\n    payload_days: 3\n",
            env(&[
                ("AIGW_OBSERVE__BATCH_SIZE", "50"),
                ("AIGW_OBSERVE__JSONL__ENABLED", "true"),
                ("AIGW_OBSERVE__RETENTION__REQUEST_LOG_DAYS", "0"),
            ]),
        )
        .unwrap();
        assert_eq!(cfg.observe.payload, PayloadMode::Masked);
        assert_eq!(cfg.observe.retention.payload_days, 3);
        assert_eq!(cfg.observe.retention.request_log_days, 0);
        assert_eq!(cfg.observe.batch_size, 50);
        assert_eq!(cfg.observe.flush_interval_ms, 500);
        assert!(cfg.observe.jsonl.enabled);
        assert!(Config::from_yaml("observe:\n  payload: everything\n").is_err());
    }

    #[test]
    fn admin_section() {
        let cfg = Config::from_yaml_with_env(
            "admin:\n  allow_private_upstreams: [\"192.168.0.0/16\"]\n",
            env(&[("AIGW_ADMIN__ALLOW_INSECURE_UPSTREAMS", "localhost, ollama")]),
        )
        .unwrap();
        assert_eq!(cfg.admin.allow_private_upstreams, ["192.168.0.0/16"]);
        assert_eq!(cfg.admin.allow_insecure_upstreams, ["localhost", "ollama"]);
        assert!(Config::from_yaml("admin:\n  allow_all: true\n").is_err());
    }

    #[test]
    fn admin_auth_section() {
        let cfg = Config::from_yaml_with_env(
            r#"
admin:
  auth:
    bootstrap: "off"
    oidc:
      issuer: "https://idp.example/realms/acme"
      client_id: aigw
      redirect_url: "https://gw.example/admin/auth/callback"
      group_roles:
        - group: aigw-admins
          role: admin
        - group: platform-leads
          role: team-admin
          organization: acme
          team: platform
"#,
            env(&[
                ("AIGW_ADMIN__AUTH__SESSION__IDLE_TIMEOUT_SECS", "60"),
                ("AIGW_ADMIN__AUTH__OIDC__GROUPS_CLAIM", "realm_access.roles"),
                (
                    "AIGW_ADMIN__AUTH__OIDC__CLIENT_SECRET_FILE",
                    "/run/secrets/oidc",
                ),
            ]),
        )
        .unwrap();
        let a = &cfg.admin.auth;
        assert_eq!(a.bootstrap, BootstrapMode::Off);
        assert!(a.oidc.enabled());
        assert_eq!(a.oidc.group_roles.len(), 2);
        assert_eq!(a.oidc.group_roles[1].role, Role::TeamAdmin);
        assert_eq!(a.oidc.group_roles[1].team.as_deref(), Some("platform"));
        assert_eq!(a.session.idle_timeout_secs, 60);
        assert_eq!(a.session.max_age_secs, 8 * 3600);
        assert_eq!(a.oidc.groups_claim, "realm_access.roles");
        assert!(a.oidc.client_secret_file.is_some());
        assert!(!Config::default().admin.auth.oidc.enabled());
        assert!(Config::from_yaml("admin:\n  auth:\n    bootstrap: maybe\n").is_err());
    }

    #[test]
    fn admin_auth_identity_lifetime_and_dirs() {
        let d = Config::default();
        assert!(d.admin.auth.audit_required, "F113: Standard fail-closed");
        assert!(!d.admin.auth.oidc.link_by_email, "F90: Standard aus");
        assert_eq!(d.admin.auth.oidc.effective_identity_claim(), "sub");
        assert_eq!(d.admin.auth.max_admin_key_lifetime_secs(), Ok(None));
        assert!(d.gitops.secret_dir.is_none() && d.upstream.ca_dir.is_none());
        assert!(!d.gitops.manage_users, "S-05: Standard aus");
        assert!(!d.upstream.entra.ambient_identity, "AIGW-90: Standard aus");
        let cfg = Config::from_yaml_with_env(
            "admin:\n  auth:\n    oidc:\n      profile: entra\n      tenant_id: t-1\n",
            env(&[
                ("AIGW_ADMIN__AUTH__MAX_ADMIN_KEY_LIFETIME", "90d"),
                ("AIGW_ADMIN__AUTH__AUDIT_REQUIRED", "false"),
                ("AIGW_GITOPS__SECRET_DIR", "/run/aigw/secrets"),
                ("AIGW_UPSTREAM__CA_DIR", "/etc/aigw/ca"),
                ("AIGW_UPSTREAM__ENTRA__AMBIENT_IDENTITY", "true"),
                ("AIGW_GITOPS__MANAGE_USERS", "true"),
            ]),
        )
        .unwrap();
        let a = &cfg.admin.auth;
        assert_eq!(a.oidc.profile, OidcProfile::Entra);
        assert_eq!(a.oidc.effective_identity_claim(), "oid");
        assert_eq!(a.max_admin_key_lifetime_secs(), Ok(Some(90 * 86_400)));
        assert!(!a.audit_required);
        assert_eq!(
            cfg.gitops.secret_dir.as_deref(),
            Some(Path::new("/run/aigw/secrets"))
        );
        assert_eq!(
            cfg.upstream.ca_dir.as_deref(),
            Some(Path::new("/etc/aigw/ca"))
        );
        assert!(cfg.upstream.entra.ambient_identity && cfg.gitops.manage_users);
        for (t, v) in [("30", 30), ("5m", 300), ("12h", 43_200), ("1d", 86_400)] {
            assert_eq!(parse_duration_secs(t), Ok(v), "{t}");
        }
        for bad in ["", "0d", "x", "-1h", "1w"] {
            assert!(parse_duration_secs(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn observe_metrics_and_otlp() {
        let cfg = Config::from_yaml_with_env(
            "observe:\n  otlp:\n    endpoint: http://collector:4318\n    headers: {authorization: geheim}\n    traces: {enabled: true, sampling_ratio: 0.25}\n",
            env(&[
                ("AIGW_OBSERVE__METRICS__MAX_DEPLOYMENTS", "7"),
                ("AIGW_OBSERVE__OTLP__TRACES__SAMPLING_RATIO", "0.5"),
                ("AIGW_OBSERVE__OTLP__LOGS__ENABLED", "true"),
            ]),
        )
        .unwrap();
        let o = &cfg.observe;
        assert!(o.metrics.enabled);
        assert_eq!(o.metrics.max_deployments, 7);
        assert!(o.otlp.traces_active());
        assert!(o.otlp.logs_active());
        assert_eq!(o.otlp.traces.sampling_ratio.get(), 0.5);
        assert!(o.otlp.traces.propagate);
        assert_eq!(o.otlp.headers["authorization"], "geheim");
        assert!(!format!("{o:?}").contains("geheim"));
        assert!(
            Config::from_yaml("observe:\n  otlp:\n    traces: {sampling_ratio: 1.5}\n").is_err()
        );
        let off = ObserveConfig::default();
        assert!(!off.otlp.traces_active() && !off.otlp.logs_active());
    }

    #[test]
    fn yaml_roundtrip() {
        let mut cfg = Config::default();
        cfg.counters.redis_url = Some("redis://x".into());
        let yaml = cfg.to_yaml().unwrap();
        assert_eq!(Config::from_yaml(&yaml).unwrap(), cfg);
    }

    #[test]
    fn cache_section_yaml_and_env() {
        let cfg = Config::from_yaml_with_env(
            "cache:\n  memory_max_bytes: 1024\n  semantic:\n    backend: pgvector\n    dimensions: 768\n    hnsw: {m: 8}\n",
            env(&[
                ("AIGW_CACHE__SEMANTIC__HNSW__EF_SEARCH", "128"),
                ("AIGW_CACHE__REDIS_URL", "redis://r:6379/0"),
            ]),
        )
        .unwrap();
        let c = &cfg.cache;
        assert!(c.enabled);
        assert_eq!(c.memory_max_bytes, 1024);
        assert_eq!(c.redis_url.as_deref(), Some("redis://r:6379/0"));
        assert_eq!(c.semantic.backend, VectorBackendKind::Pgvector);
        assert_eq!(c.semantic.dimensions, Some(768));
        assert_eq!(c.semantic.hnsw.m, 8);
        assert_eq!(c.semantic.hnsw.ef_construction, 200);
        assert_eq!(c.semantic.hnsw.ef_search, 128);
        assert!(Config::from_yaml("cache:\n  semantic:\n    backend: faiss\n").is_err());
    }
}
