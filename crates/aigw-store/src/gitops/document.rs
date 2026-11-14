// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Lesen und Schreiben der GitOps-Dokumente.
//!
//! Grundlage ist [`Manifest`] aus `aigw-core`. Zusätzlich erlaubt GitOps bei `ApiKey` das Feld
//! `spec.secret` (`{valueFrom: {env|file}}`) für einen vorgegebenen Key-Klartext; es wird vor
//! dem Einlesen der Spezifikation abgetrennt und steht in [`Document::api_key_secret`].

use std::collections::BTreeMap;

use aigw_core::resource::{
    ApiKeySpec, DeploymentSpec, Manifest, McpGrantSpec, McpServerSpec, ModelGroupSpec, ObjectMeta,
    OrganizationSpec, PluginBindingSpec, PriceSpec, ResourceKind, ResourceSpec, SecretRef,
    TeamSpec, UserSpec,
};
use serde::{Deserialize, Serialize};
use time::UtcOffset;

use super::{GitopsError, Issue};
use crate::Resource;
use crate::repo::trunc_millis;

/// Ein Dokument einer GitOps-Datei.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    /// Position in der Datei (ab 1).
    pub index: usize,
    /// Manifest (Server-Felder `createdAt`/`updatedAt` entfernt, Zeitpunkte in UTC).
    pub manifest: Manifest,
    /// Nur `ApiKey`: vorgegebener Key-Klartext (`spec.secret.valueFrom`).
    pub api_key_secret: Option<SecretRef>,
}

impl Document {
    /// Art.
    pub fn kind(&self) -> ResourceKind {
        self.manifest.spec.kind()
    }

    /// Name.
    pub fn name(&self) -> &str {
        &self.manifest.metadata.name
    }
}

/// Rohform eines Dokuments (wie [`Manifest`], `spec` noch untypisiert).
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawDoc {
    api_version: String,
    kind: ResourceKind,
    metadata: ObjectMeta,
    #[serde(default)]
    spec: serde_json::Value,
}

/// `true`, wenn der Text keine Inhalte außer Kommentaren, Leerzeilen und Trennern enthält.
fn is_blank(text: &str) -> bool {
    text.lines().all(|l| {
        let t = l.trim();
        t.is_empty() || t.starts_with('#') || t == "---" || t == "..."
    })
}

/// Liest alle Dokumente einer GitOps-Datei und prüft sie ohne Datenbank: YAML-Syntax,
/// `apiVersion`/`kind`, unbekannte Felder, Namen, Geheimnisregel (nur `valueFrom`), Regeln
/// der Spezifikation und doppelte Ressourcen. Alle Fehler werden gesammelt gemeldet.
pub fn parse_documents(text: &str) -> Result<Vec<Document>, GitopsError> {
    if is_blank(text) {
        return Ok(Vec::new());
    }
    let raws: Vec<Option<RawDoc>> = serde_saphyr::from_multiple(text).map_err(|e| {
        let mut issue = Issue::new(format!("YAML ungültig: {e}"));
        // Fundstelle für die Anzeige mit Zeilenbezug (Admin-UI, AIGW-55).
        if let Some(loc) = e.location() {
            issue.line = usize::try_from(loc.line()).ok().filter(|l| *l > 0);
            issue.column = usize::try_from(loc.column()).ok().filter(|c| *c > 0);
        }
        GitopsError::Invalid(vec![issue])
    })?;

    let mut docs = Vec::with_capacity(raws.len());
    let mut issues = Vec::new();
    for (i, raw) in raws.into_iter().enumerate() {
        let index = i + 1;
        // Leere Dokumente (z. B. `---` am Ende) überspringen.
        let Some(mut raw) = raw else { continue };
        let kind = raw.kind;
        let name = raw.metadata.name.clone();
        let issue = |m: String| Issue::at(Some(index), kind, &name, m);

        let mut api_key_secret = None;
        if kind == ResourceKind::ApiKey
            && let Some(obj) = raw.spec.as_object_mut()
            && let Some(s) = obj.remove("secret")
        {
            match serde_json::from_value::<SecretRef>(s) {
                Ok(r) if r.value.is_none() && r.value_from.is_some() => api_key_secret = Some(r),
                Ok(_) => {
                    issues.push(issue(
                        "spec.secret: nur `valueFrom` (env/file) erlaubt, kein Klartext".into(),
                    ));
                    continue;
                }
                Err(e) => {
                    issues.push(issue(format!("spec.secret ungültig: {e}")));
                    continue;
                }
            }
        }

        let manifest = serde_json::to_value(&raw)
            .and_then(serde_json::from_value::<Manifest>)
            .map_err(|e| issue(e.to_string()));
        let mut manifest = match manifest {
            Ok(m) => m,
            Err(e) => {
                issues.push(e);
                continue;
            }
        };
        if let Err(e) = manifest.validate() {
            issues.push(issue(match e {
                aigw_core::resource::ManifestError::Invalid { message, .. } => message,
                other => other.to_string(),
            }));
            continue;
        }
        normalize(&mut manifest);
        if let Err(m) = validate_spec(&manifest.spec) {
            issues.push(issue(m));
            continue;
        }
        docs.push(Document {
            index,
            manifest,
            api_key_secret,
        });
    }

    // Doppelte Ressourcen (Art + Name).
    let mut seen: BTreeMap<(ResourceKind, String), usize> = BTreeMap::new();
    for d in &docs {
        if let Some(first) = seen.insert((d.kind(), d.name().to_owned()), d.index) {
            issues.push(Issue::at(
                Some(d.index),
                d.kind(),
                d.name(),
                format!("doppelt (bereits in Dokument {first})"),
            ));
        }
    }

    if issues.is_empty() {
        Ok(docs)
    } else {
        super::locate(text, &mut issues);
        Err(GitopsError::Invalid(issues))
    }
}

/// Entfernt Server-Felder und bringt Zeitpunkte in die gespeicherte Form (UTC, Millisekunden),
/// damit der Vergleich mit dem Ist-Zustand stabil ist.
pub(crate) fn normalize(m: &mut Manifest) {
    m.metadata.created_at = None;
    m.metadata.updated_at = None;
    match &mut m.spec {
        ResourceSpec::Price(p) => p.valid_from = p.valid_from.to_offset(UtcOffset::UTC),
        ResourceSpec::ApiKey(k) => {
            k.expires_at = k
                .expires_at
                .map(|t| trunc_millis(t).to_offset(UtcOffset::UTC));
        }
        _ => {}
    }
}

/// Regeln der Spezifikation (dieselben wie beim Schreiben über das Repository).
pub(crate) fn validate_spec(spec: &ResourceSpec) -> Result<(), String> {
    let r = match spec {
        ResourceSpec::Organization(s) => <OrganizationSpec as Resource>::validate(s),
        ResourceSpec::Team(s) => <TeamSpec as Resource>::validate(s),
        ResourceSpec::User(s) => <UserSpec as Resource>::validate(s),
        ResourceSpec::ApiKey(s) => <ApiKeySpec as Resource>::validate(s),
        ResourceSpec::ProviderCredential(_) | ResourceSpec::Secret(_) => Ok(()),
        ResourceSpec::ModelGroup(s) => <ModelGroupSpec as Resource>::validate(s),
        ResourceSpec::Deployment(s) => <DeploymentSpec as Resource>::validate(s),
        ResourceSpec::Price(s) => <PriceSpec as Resource>::validate(s),
        ResourceSpec::PluginBinding(s) => <PluginBindingSpec as Resource>::validate(s),
        ResourceSpec::McpServer(s) => <McpServerSpec as Resource>::validate(s),
        ResourceSpec::McpGrant(s) => <McpGrantSpec as Resource>::validate(s),
        ResourceSpec::ToolPolicy(s) => {
            <aigw_core::tools::ToolPolicySpec as Resource>::validate(s)
        }
    };
    r.map_err(|e| match e {
        crate::StoreError::Invalid(m) => m,
        other => other.to_string(),
    })
}

/// Name der Platzhalter-Umgebungsvariablen für ein Feld von Zugangsdaten im Export:
/// `AIGW_SECRET_<ZUGANG>_<FELD>` in Großbuchstaben, camelCase → `SNAKE_CASE`, andere Zeichen
/// → `_`. Mehrfache `_` werden zusammengefasst – `__` würde sonst als Konfig-Override
/// (`AIGW_<ABSCHNITT>__<FELD>`) gelesen.
pub fn secret_env_name(credential: &str, field: &str) -> String {
    let mut raw = String::from(super::SECRET_ENV_PREFIX);
    raw.push_str(credential);
    raw.push('_');
    let mut prev_lower = false;
    for c in field.chars() {
        if c.is_ascii_uppercase() && prev_lower {
            raw.push('_');
        }
        prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        raw.push(c);
    }
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        let c = if c.is_ascii_alphanumeric() {
            c.to_ascii_uppercase()
        } else {
            '_'
        };
        if c == '_' && out.ends_with('_') {
            continue;
        }
        out.push(c);
    }
    out.trim_end_matches('_').to_owned()
}

/// Schreibt Manifeste mit Kopfkommentar als YAML-Mehrfachdokument.
pub(crate) fn render_yaml(docs: &[Manifest], notes: &[String]) -> Result<String, GitopsError> {
    let mut out = String::from(
        "# ai-gw GitOps-Export (apiVersion aigw/v1alpha1)\n\
         # Anwenden: aigw config diff|apply <datei> [--prune]\n",
    );
    for n in notes {
        out.push_str("# ");
        out.push_str(n);
        out.push('\n');
    }
    if docs.is_empty() {
        return Ok(out);
    }
    let yaml = Manifest::to_yaml(docs).map_err(|e| {
        GitopsError::Store(crate::StoreError::Invalid(format!(
            "YAML-Ausgabe fehlgeschlagen: {e}"
        )))
    })?;
    if !yaml.trim_start().starts_with("---") {
        out.push_str("---\n");
    }
    out.push_str(&yaml);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_names_are_safe() {
        assert_eq!(
            secret_env_name("openai-prod", "apiKey"),
            "AIGW_SECRET_OPENAI_PROD_API_KEY"
        );
        assert_eq!(
            secret_env_name("a._b", "secretAccessKey"),
            "AIGW_SECRET_A_B_SECRET_ACCESS_KEY"
        );
        assert!(!secret_env_name("x__y", "__z__").contains("__"));
    }

    #[test]
    fn parses_api_key_secret_and_rejects_errors() {
        let ok = "apiVersion: aigw/v1alpha1\nkind: ApiKey\nmetadata: {name: ci}\nspec:\n  organization: o\n  team: t\n  secret:\n    valueFrom: {env: CI_KEY}\n---\n";
        let docs = parse_documents(ok).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].api_key_secret, Some(SecretRef::env("CI_KEY")));

        let plain = "apiVersion: aigw/v1alpha1\nkind: ApiKey\nmetadata: {name: ci}\nspec:\n  organization: o\n  secret: {value: sk-aigw-x}\n";
        assert!(parse_documents(plain).is_err());

        let dup = "apiVersion: aigw/v1alpha1\nkind: Organization\nmetadata: {name: o}\n---\napiVersion: aigw/v1alpha1\nkind: Organization\nmetadata: {name: o}\n---\napiVersion: aigw/v1alpha1\nkind: Team\nmetadata: {name: t}\nspec: {organisation: o}\n";
        let err = parse_documents(dup).unwrap_err();
        assert_eq!(err.issues().len(), 2, "{err}");

        assert!(
            parse_documents("# nur Kommentar\n---\n")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn issues_carry_lines() {
        // Dokument 2 (Zeile 6) ist ungültig: unbekanntes Feld.
        let text = "# Kopf\n---\napiVersion: aigw/v1alpha1\nkind: Organization\nmetadata: {name: o}\n---\n\napiVersion: aigw/v1alpha1\nkind: Team\nmetadata: {name: t}\nspec: {organization: o, bogus: 1}\n";
        assert_eq!(super::super::document_lines(text), vec![3, 8]);
        let err = parse_documents(text).unwrap_err();
        let issues = err.issues();
        assert_eq!(issues.len(), 1, "{err}");
        assert_eq!(issues[0].document, Some(2));
        assert_eq!(issues[0].line, Some(8));

        // YAML-Syntaxfehler mit Fundstelle.
        let err = parse_documents("apiVersion: aigw/v1alpha1\nkind: [unterminated\n").unwrap_err();
        assert!(err.issues()[0].line.is_some(), "{err}");
    }

    #[test]
    fn document_lines_cover_edge_cases() {
        use super::super::document_lines;
        assert_eq!(document_lines("a: 1\n---\nb: 2\n"), vec![1, 3]);
        assert_eq!(document_lines("---\n---\nb: 2\n---\n"), vec![1, 3, 4]);
        assert_eq!(document_lines("--- {a: 1}\n"), vec![1]);
        assert!(document_lines("# nur\n").is_empty());
    }
}
