// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Startkonfiguration (YAML, E7).
//!
//! Die Startkonfiguration enthält nur, was vor dem Öffnen der Datenbank bekannt sein muss
//! (Listener, Datenbank, Zähler-Backend, Master-Key, Logging). Alles Weitere (Modelle, Keys,
//! Plugins …) sind Ressourcen in der Datenbank bzw. GitOps-YAML, siehe [`crate::resource`].
//!
//! Jeder Wert lässt sich per Umgebungsvariable überschreiben:
//! `AIGW_<ABSCHNITT>__<FELD>` (doppelter Unterstrich als Trenner, tiefere Ebenen ebenso),
//! z. B. `AIGW_STORE__URL=postgres://…` oder `AIGW_STORE__MAX_CONNECTIONS=20`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

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
    /// Zähler-Backend für Limits und Budgets.
    pub counters: CountersConfig,
    /// Master-Key für verschlüsselte Zugangsdaten.
    pub secrets: SecretsConfig,
    /// Logging.
    pub log: LogConfig,
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

/// Einstellungen des HTTP-Servers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Adresse, auf der Gateway und Admin-API lauschen.
    pub listen: SocketAddr,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 4000)),
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
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            url: "sqlite://aigw.db?mode=rwc".to_owned(),
            max_connections: 10,
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

/// Einstellungen der Zähler.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CountersConfig {
    /// Gewähltes Backend.
    pub backend: CounterBackendKind,
    /// Redis-URL (nur für `backend: redis`), z. B. `redis://redis:6379/0`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redis_url: Option<String>,
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
  listen: "0.0.0.0:8080"
counters:
  backend: postgres-sync
"#,
        )
        .unwrap();
        assert_eq!(cfg.server.listen.port(), 8080);
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
        assert!(Config::from_yaml("unknown_section: {}\n").is_err());
        assert!(Config::from_yaml("counters:\n  backend: memcached\n").is_err());
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
                ("AIGW_SERVER__LISTEN", "0.0.0.0:9000"),
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
        assert_eq!(cfg.server.listen.port(), 9000);
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
    fn yaml_roundtrip() {
        let mut cfg = Config::default();
        cfg.counters.redis_url = Some("redis://x".into());
        let yaml = cfg.to_yaml().unwrap();
        assert_eq!(Config::from_yaml(&yaml).unwrap(), cfg);
    }
}
