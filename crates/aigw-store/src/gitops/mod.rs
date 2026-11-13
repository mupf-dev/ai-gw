// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! GitOps (AIGW-33): Export, Diff und Apply von `aigw/v1alpha1`-Manifesten.
//!
//! - [`export_yaml`]: alle Ressourcen als Mehrfachdokument-YAML in Abhängigkeitsreihenfolge
//!   ([`ResourceKind::ALL`], je Art nach Name), ohne Server-Felder (IDs, Zeitstempel, Hashes,
//!   Key-Zustand). Geheimnisse erscheinen nur als `valueFrom`-Platzhalter
//!   (`env: AIGW_SECRET_<ZUGANG>_<FELD>`), API-Keys nie im Klartext.
//! - [`parse_documents`]: liest Manifeste inkl. der GitOps-Erweiterung `spec.secret` bei
//!   `ApiKey` (vorgegebener Key-Klartext per `valueFrom`).
//! - [`plan`]: Soll/Ist-Vergleich je Ressource (create/update/delete/unchanged/retain/skip)
//!   nach vollständiger Validierung (Verweise, Geheimnisse, Eindeutigkeit).
//! - [`apply`]: wendet den Plan in **einer** Transaktion an (`config_version` genau einmal
//!   erhöht, ein Audit-Eintrag je Änderung), alles oder nichts.
//!
//! Details: `docs/konzept/09-gitops.md`.

mod apply;
mod document;
mod plan;
mod secrets;
mod state;

use std::fmt;

use aigw_core::resource::ResourceKind;
use aigw_core::upstream::UpstreamPolicy;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::StoreError;

pub use apply::{ApplyReport, apply, plan};
pub use document::{Document, parse_documents, secret_env_name};
pub use plan::{Change, ChangeAction, FieldChange, Plan, Summary};
pub use secrets::{ProcessSecrets, SecretPolicy, SecretSource};
pub use state::{ExportFilter, export_yaml, export_yaml_filtered};

/// Akteur im Audit-Log für Änderungen per GitOps (CLI).
pub const GITOPS_ACTOR: &str = "gitops";

/// Präfix der Platzhalter-Umgebungsvariablen im Export (und Erlaubnisliste der Admin-API).
pub const SECRET_ENV_PREFIX: &str = "AIGW_SECRET_";

/// Prüffunktion für `ModelGroup.spec.settings` (siehe [`ApplyOptions::group_settings`]).
pub type SettingsCheck = fn(&aigw_core::resource::JsonObject) -> Result<(), String>;

/// Prüffunktion für Plugin-Bindungen (Spezifikation mit Steuerfeldern; SSRF-Regeln aus
/// [`ApplyOptions::upstreams`]), siehe [`ApplyOptions::plugin_bindings`].
pub type PluginBindingCheck =
    fn(&UpstreamPolicy, &aigw_core::resource::PluginBindingSpec) -> Result<(), String>;

/// Umgang mit `ApiKey`-Manifesten.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ApiKeyMode {
    /// Keys werden exportiert und angewendet: bestehende Keys erhalten Spezifikation und Labels
    /// aus dem Manifest (ihr Geheimnis bleibt, außer `spec.secret` gibt ein anderes vor); neue
    /// Keys werden nur mit `spec.secret.valueFrom` angelegt, sonst übersprungen (Warnung).
    /// `--prune` löscht Keys, die nicht im Manifest stehen.
    Include,
    /// Standard (F4): Keys werden weder exportiert noch angewendet noch gelöscht
    /// (`ApiKey`-Dokumente werden mit Warnung übersprungen; Verweise darauf gelten gegen die
    /// Datenbank).
    #[default]
    Ignore,
}

/// Optionen für [`plan`] und [`apply`].
#[derive(Debug, Clone)]
pub struct ApplyOptions {
    /// Nur planen, nichts schreiben.
    pub dry_run: bool,
    /// Ressourcen löschen, die nicht im Manifest stehen (ohne: nie löschen).
    pub prune: bool,
    /// Umgang mit API-Keys.
    pub api_keys: ApiKeyMode,
    /// Akteur im Audit-Log.
    pub actor: String,
    /// Erlaubte Herkunft von Geheimnissen (`valueFrom`).
    pub secrets: SecretPolicy,
    /// Regeln für Endpunkte von Deployments (wie in der Admin-API, `admin.*`).
    pub upstreams: UpstreamPolicy,
    /// Prüfung von `ModelGroup.spec.settings` (Routing-Parser der Datenebene; die
    /// Persistenzschicht kennt ihn nicht, daher von außen gesetzt, z. B.
    /// `aigw_admin::resources::group_settings`). `None` = keine Prüfung.
    pub group_settings: Option<SettingsCheck>,
    /// Prüfung von `PluginBinding` (Registry der Plugin-Kette, F118/F165; die
    /// Persistenzschicht kennt sie nicht, z. B. `aigw_admin::plugin_bindings::check`).
    /// `None` = keine Prüfung.
    pub plugin_bindings: Option<PluginBindingCheck>,
}

impl Default for ApplyOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            prune: false,
            api_keys: ApiKeyMode::Ignore,
            actor: GITOPS_ACTOR.to_owned(),
            secrets: SecretPolicy::default(),
            upstreams: UpstreamPolicy::strict(),
            group_settings: None,
            plugin_bindings: None,
        }
    }
}

/// Ein Validierungsfehler, möglichst mit Dokument und Ressource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Issue {
    /// Nummer des Dokuments in der Datei (ab 1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document: Option<usize>,
    /// Art der Ressource.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<ResourceKind>,
    /// Name der Ressource.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Meldung.
    pub message: String,
    /// Zeile in der Datei (ab 1): Fundstelle eines YAML-Fehlers bzw. erste Inhaltszeile des
    /// Dokuments ([`locate`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    /// Spalte (ab 1), nur bei YAML-Fehlern mit genauer Fundstelle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
}

impl Issue {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            document: None,
            kind: None,
            name: None,
            message: message.into(),
            line: None,
            column: None,
        }
    }

    pub(crate) fn at(
        document: Option<usize>,
        kind: ResourceKind,
        name: &str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            document,
            kind: Some(kind),
            name: Some(name.to_owned()),
            message: message.into(),
            line: None,
            column: None,
        }
    }
}

/// Erste Inhaltszeile (ab 1) jedes Dokuments einer Mehrfachdokument-Datei, in derselben
/// Zählung wie [`Document::index`] (Index 0 = Dokument 1). Dokumente beginnen nach einem
/// Trenner `---`; Inhalt vor dem ersten Trenner ist Dokument 1. Ein leeres Dokument zeigt auf
/// seinen Trenner.
pub fn document_lines(text: &str) -> Vec<usize> {
    let mut out = Vec::new();
    // (Zeile des Trenners bzw. None für den Vorspann, erste Inhaltszeile)
    let mut current: Option<(Option<usize>, Option<usize>)> = Some((None, None));
    for (i, l) in text.lines().enumerate() {
        let n = i + 1;
        let t = l.trim_end();
        let is_sep = t == "---" || t.starts_with("--- ") || t.starts_with("---\t");
        if is_sep {
            if let Some((sep, first)) = current.take() {
                match (sep, first) {
                    (_, Some(f)) => out.push(f),
                    (Some(s), None) => out.push(s),
                    (None, None) => {}
                }
            }
            // Inhalt hinter dem Trenner in derselben Zeile (`--- {a: 1}`).
            let rest = t.trim_start_matches('-').trim();
            let first = (!rest.is_empty() && !rest.starts_with('#')).then_some(n);
            current = Some((Some(n), first));
            continue;
        }
        let c = l.trim();
        if let Some((_, first @ None)) = current.as_mut()
            && !c.is_empty()
            && !c.starts_with('#')
            && c != "..."
        {
            *first = Some(n);
        }
    }
    if let Some((sep, first)) = current {
        match (sep, first) {
            (_, Some(f)) => out.push(f),
            (Some(s), None) => out.push(s),
            (None, None) => {}
        }
    }
    out
}

/// Ergänzt die Zeile ([`Issue::line`]) von Meldungen mit Dokumentnummer aus dem Text der Datei.
pub fn locate(text: &str, issues: &mut [Issue]) {
    let lines = document_lines(text);
    for i in issues {
        if i.line.is_none()
            && let Some(d) = i.document
            && let Some(l) = d.checked_sub(1).and_then(|d| lines.get(d))
        {
            i.line = Some(*l);
        }
    }
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.document, self.line) {
            (Some(d), Some(l)) => write!(f, "Dokument {d} (Zeile {l}): ")?,
            (Some(d), None) => write!(f, "Dokument {d}: ")?,
            (None, Some(l)) => write!(f, "Zeile {l}: ")?,
            (None, None) => {}
        }
        match (&self.kind, &self.name) {
            (Some(k), Some(n)) => write!(f, "{k} {n:?}: ")?,
            (Some(k), None) => write!(f, "{k}: ")?,
            _ => {}
        }
        f.write_str(&self.message)
    }
}

/// Fehler von GitOps-Operationen.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GitopsError {
    /// Manifest ungültig – es wurde nichts angewendet.
    #[error("Manifest ungültig, nichts angewendet:\n{}", format_issues(.0))]
    Invalid(Vec<Issue>),
    /// Die Konfiguration wurde zwischen Planung und Anwendung geändert.
    #[error(
        "Konfiguration wurde während der Anwendung geändert (geplant auf Version {planned}, \
         vorgefunden {found}) – nichts angewendet, bitte erneut ausführen"
    )]
    Concurrent {
        /// Version, gegen die geplant wurde.
        planned: i64,
        /// Version beim Schreiben (vor der eigenen Erhöhung).
        found: i64,
    },
    /// Fehler der Persistenzschicht (die Transaktion wurde zurückgerollt).
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl GitopsError {
    /// Validierungsfehler, falls es welche sind.
    pub fn issues(&self) -> &[Issue] {
        match self {
            Self::Invalid(i) => i,
            _ => &[],
        }
    }
}

fn format_issues(issues: &[Issue]) -> String {
    issues
        .iter()
        .map(|i| format!("  - {i}"))
        .collect::<Vec<_>>()
        .join("\n")
}
