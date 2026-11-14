// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Inhaltliche Regeln für Modellgruppen, Deployments, Zugangsdaten und Preise (AIGW-65).
//!
//! Gemeinsam genutzt von der Admin-API (`aigw_admin`) und GitOps (`aigw_store::gitops`), damit
//! beide Wege dieselben Regeln anwenden. Die Funktionen sind rein (ohne Datenbank); Verweise
//! übergibt der Aufrufer. Fehler sind deutschsprachige Meldungen mit Feldpfad
//! (Admin-API: `422`, GitOps: `Issue`).
//!
//! [`bounds`]: Grenzen Key ≤ Team ≤ Organisation (AIGW-67).

pub mod bounds;

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde_json::Value;
use time::OffsetDateTime;

use crate::resource::{
    Amount, ApiKeySpec, DeploymentSpec, EntraAuth, EntraCredentialKind, JsonObject, ModelGroupSpec,
    PriceSpec, Role, UserSpec,
};
use crate::types::ProviderKind;
use crate::upstream::UpstreamPolicy;

/// Höchstlänge eines Alias (Modellnamen der Clients können länger sein als Ressourcennamen).
pub const MAX_ALIAS_LEN: usize = 256;
/// Höchstgewicht eines Deployments.
pub const MAX_WEIGHT: u32 = 1_000_000;
/// Höchstlänge von `providerModel` bzw. `Price.model`.
pub const MAX_MODEL_LEN: usize = 256;
/// Höchstzahl Felder je Zugangsdaten.
pub const MAX_CREDENTIAL_FIELDS: usize = 32;
/// Höchstlänge eines Zugangsdaten-Werts (Service-Account-JSON passt bequem hinein).
pub const MAX_CREDENTIAL_VALUE_LEN: usize = 64 * 1024;
/// Standard-Endpunkt eines Ollama-Deployments ohne `endpoint` (wie im Adapter).
pub const OLLAMA_DEFAULT_ENDPOINT: &str = "http://localhost:11434/v1";

type Res = Result<(), String>;

// ---------------------------------------------------------------------------------------------
// Nutzer (F90)
// ---------------------------------------------------------------------------------------------

/// Nutzer: OIDC-Identität nur vollständig (`oidcIssuer` **und** `oidcSubject`, die Kennung
/// laut `identity_claim`), damit vorab angelegte Nutzer eindeutig zugeordnet werden; Rolle
/// `gitops` nur global (F55).
pub fn user(spec: &UserSpec) -> Res {
    let filled = |v: &Option<String>| v.as_deref().is_some_and(|s| !s.trim().is_empty());
    match (filled(&spec.oidc_issuer), filled(&spec.oidc_subject)) {
        (true, true) | (false, false) => {}
        _ => {
            return Err(
                "oidcIssuer und oidcSubject nur gemeinsam angeben (OIDC-Identität = Issuer + \
                 Kennung)"
                    .into(),
            );
        }
    }
    if spec
        .oidc_issuer
        .as_deref()
        .is_some_and(|s| s.trim().is_empty())
        || spec
            .oidc_subject
            .as_deref()
            .is_some_and(|s| s.trim().is_empty())
    {
        return Err("oidcIssuer/oidcSubject dürfen nicht leer sein".into());
    }
    if let Some(b) = spec
        .roles
        .iter()
        .find(|b| b.role == Role::Gitops && (b.organization.is_some() || b.team.is_some()))
    {
        return Err(format!(
            "roles: gitops gilt nur global (ohne organization/team), erhalten {b:?}"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// API-Keys (F3)
// ---------------------------------------------------------------------------------------------

/// Organisation eines API-Keys: Pflicht; mit Team muss sie die Organisation des Teams sein.
/// `team_org` ist die Organisation des angegebenen Teams (`None`: ohne Team oder Team
/// unbekannt – unbekannte Verweise meldet der Aufrufer).
pub fn api_key_organization(spec: &ApiKeySpec, team_org: Option<&str>) -> Res {
    if spec.organization.trim().is_empty() {
        return Err(
            "spec.organization: Pflichtfeld (jeder Key gehört zu einer Organisation)".into(),
        );
    }
    match (&spec.team, team_org) {
        (Some(team), Some(org)) if org != spec.organization => Err(format!(
            "spec.organization: Team {team:?} gehört zu Organisation {org:?}, nicht zu {:?}",
            spec.organization
        )),
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------------------------
// Modellgruppen
// ---------------------------------------------------------------------------------------------

/// Aliasse: Zeichen, Länge, keine Doppelungen, nicht der eigene Name. Die globale
/// Eindeutigkeit über alle Gruppen prüft die Datenbank bzw. GitOps.
pub fn aliases(name: &str, spec: &ModelGroupSpec) -> Res {
    let mut seen = HashSet::new();
    for a in &spec.aliases {
        if a.is_empty()
            || a.len() > MAX_ALIAS_LEN
            || a.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(format!(
                "aliases: ungültiger Alias {a:?} (1–{MAX_ALIAS_LEN} Zeichen, ohne Leer-/Steuerzeichen)"
            ));
        }
        if a == name {
            return Err(format!("aliases: {a:?} ist bereits der Name der Gruppe"));
        }
        if !seen.insert(a.as_str()) {
            return Err(format!("aliases: {a:?} doppelt"));
        }
    }
    Ok(())
}

/// Höchstzahl der Fallbacks einer Modellgruppe. Die Datenebene probiert je Anfrage höchstens
/// die Ursprungsgruppe plus so viele Fallbacks (8 Gruppen; nur die Kette der Ursprungsgruppe,
/// F69/F81).
pub const MAX_FALLBACKS: usize = 7;

/// Fallbacks der Gruppe `own` (mit neuer `spec`) gegen den Gesamtbestand `groups`
/// (Name → Spezifikation; `own` darf fehlen oder veraltet sein): höchstens
/// [`MAX_FALLBACKS`], Ziel existiert (kanonischer Name), nicht die Gruppe selbst, keine
/// Doppelungen, keine Zyklen.
pub fn fallbacks(
    groups: &BTreeMap<&str, &ModelGroupSpec>,
    own: &str,
    spec: &ModelGroupSpec,
) -> Res {
    if spec.fallbacks.len() > MAX_FALLBACKS {
        return Err(format!(
            "fallbacks: höchstens {MAX_FALLBACKS} Einträge (je Anfrage werden höchstens \
             {} Modellgruppen versucht)",
            MAX_FALLBACKS + 1
        ));
    }
    let mut seen = HashSet::new();
    for f in &spec.fallbacks {
        let target = f.model_group.as_str();
        if target == own {
            return Err(format!(
                "fallbacks: Gruppe {target:?} kann nicht auf sich selbst zurückfallen"
            ));
        }
        if !groups.contains_key(target) {
            let hint = groups
                .iter()
                .find(|(_, g)| g.aliases.iter().any(|a| a == target))
                .map(|(n, _)| format!(" (Alias von {n:?} – kanonischen Namen verwenden)"))
                .unwrap_or_default();
            return Err(format!(
                "fallbacks: unbekannte Modellgruppe {target:?}{hint}"
            ));
        }
        if !seen.insert(target) {
            return Err(format!("fallbacks: {target:?} doppelt"));
        }
    }
    // Graph mit der neuen Spezifikation; Suche ab der eigenen Gruppe zurück zu ihr.
    let edges = |g: &str| -> Vec<&str> {
        let s = if g == own {
            Some(spec)
        } else {
            groups.get(g).copied()
        };
        s.map(|s| s.fallbacks.iter().map(|f| f.model_group.as_str()).collect())
            .unwrap_or_default()
    };
    let mut stack: Vec<Vec<&str>> = vec![vec![own]];
    let mut visited = HashSet::new();
    while let Some(path) = stack.pop() {
        let Some(&last) = path.last() else { continue };
        for next in edges(last) {
            if next == own {
                let mut cycle = path.clone();
                cycle.push(own);
                return Err(format!("fallbacks: Zyklus {}", cycle.join(" → ")));
            }
            if visited.insert(next) {
                let mut p = path.clone();
                p.push(next);
                stack.push(p);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Deployments
// ---------------------------------------------------------------------------------------------

/// Schlüssel in `settings.timeouts` (Millisekunden, je > 0; F44).
pub const TIMEOUT_KEYS: &[&str] = &["connectMs", "firstByteMs", "idleMs", "totalMs"];

/// Werte von `settings.thinkingBudgetPolicy` (F59).
pub const THINKING_BUDGET_POLICIES: &[&str] = &["raise-max-tokens", "cap-budget"];

/// Einstellungen, die je Deployment gelten und auch an der Modellgruppe stehen dürfen (dann
/// erben die Deployments sie): `timeouts` (F44), `thinkingBudgetPolicy` (F59),
/// `propagateTraceContext` (F98). Geprüft beim Schreiben von Deployments und – für die
/// Modellgruppe – über `GroupPolicy::from_settings` der Datenebene.
pub fn deployment_settings(settings: &JsonObject) -> Res {
    match settings.get("timeouts") {
        None | Some(Value::Null) => {}
        Some(Value::Object(o)) => {
            for (k, v) in o {
                if !TIMEOUT_KEYS.contains(&k.as_str()) {
                    return Err(format!(
                        "settings.timeouts.{k} unbekannt (erlaubt: {})",
                        TIMEOUT_KEYS.join(", ")
                    ));
                }
                if v.as_u64().is_none_or(|ms| ms == 0) {
                    return Err(format!(
                        "settings.timeouts.{k} muss eine Zahl > 0 (Millisekunden) sein"
                    ));
                }
            }
        }
        Some(_) => return Err("settings.timeouts muss ein Objekt sein".into()),
    }
    match settings.get("thinkingBudgetPolicy") {
        None | Some(Value::Null) => {}
        Some(Value::String(p)) if THINKING_BUDGET_POLICIES.contains(&p.as_str()) => {}
        Some(other) => {
            return Err(format!(
                "settings.thinkingBudgetPolicy {other} unbekannt (raise-max-tokens, cap-budget)"
            ));
        }
    }
    match settings.get("propagateTraceContext") {
        None | Some(Value::Null | Value::Bool(_)) => {}
        Some(_) => return Err("settings.propagateTraceContext muss true/false sein".into()),
    }
    pricing_settings(settings.get("pricing"))
}

/// Erlaubte Schlüssel in `settings.pricing` (Preisvorschläge, AIGW-85).
pub const PRICING_KEYS: &[&str] = &["source", "region", "deploymentType", "model", "meter"];
/// Werte von `settings.pricing.source`.
pub const PRICING_SOURCES: &[&str] = &["auto", "azure", "catalog", "none"];
/// Werte von `settings.pricing.deploymentType`.
pub const PRICING_DEPLOYMENT_TYPES: &[&str] = &["global", "data-zone", "regional"];

/// `settings.pricing`: Zuordnung eines Deployments zu Preisvorschlägen (AIGW-85).
fn pricing_settings(v: Option<&Value>) -> Res {
    let o = match v {
        None | Some(Value::Null) => return Ok(()),
        Some(Value::Object(o)) => o,
        Some(_) => return Err("settings.pricing muss ein Objekt sein".into()),
    };
    for (k, v) in o {
        if !PRICING_KEYS.contains(&k.as_str()) {
            return Err(format!(
                "settings.pricing.{k} unbekannt (erlaubt: {})",
                PRICING_KEYS.join(", ")
            ));
        }
        let Some(s) = v.as_str() else {
            if v.is_null() {
                continue;
            }
            return Err(format!("settings.pricing.{k} muss eine Zeichenkette sein"));
        };
        let ok = match k.as_str() {
            "source" => PRICING_SOURCES.contains(&s),
            "deploymentType" => PRICING_DEPLOYMENT_TYPES.contains(&s),
            "region" => {
                !s.is_empty()
                    && s.len() <= 64
                    && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            }
            _ => !s.trim().is_empty() && s.len() <= 256 && !s.chars().any(char::is_control),
        };
        if !ok {
            let hint = match k.as_str() {
                "source" => PRICING_SOURCES.join(", "),
                "deploymentType" => PRICING_DEPLOYMENT_TYPES.join(", "),
                "region" => "Azure-Region klein ohne Leerzeichen, z. B. swedencentral".into(),
                _ => "1–256 Zeichen".into(),
            };
            return Err(format!("settings.pricing.{k} ungültig ({hint})"));
        }
    }
    Ok(())
}

/// Anbieter ohne Standard-Endpunkt (Basis-URL ist Pflicht).
pub fn endpoint_required(p: ProviderKind) -> bool {
    matches!(
        p,
        ProviderKind::AzureOpenAi | ProviderKind::Vllm | ProviderKind::OpenAiCompatible
    )
}

/// Endpunkt, den der Adapter ohne `endpoint` nutzt, sofern er lokal/privat ist und deshalb
/// ebenfalls den SSRF-Regeln unterliegt.
fn implicit_local_endpoint(p: ProviderKind) -> Option<&'static str> {
    (p == ProviderKind::Ollama).then_some(OLLAMA_DEFAULT_ENDPOINT)
}

/// Zeichenkette aus `settings` (Fehler, wenn vorhanden, aber kein String).
fn setting_str<'a>(spec: &'a DeploymentSpec, key: &str) -> Result<Option<&'a str>, String> {
    match spec.settings.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(format!("settings.{key} muss eine Zeichenkette sein")),
    }
}

/// Regeln eines Deployments ohne Verweise: `providerModel`, Gewicht, Kapazität, Endpunkt
/// (Pflicht je Anbieter, SSRF-Regeln), Azure-Pflichtfelder.
pub fn deployment(upstreams: &UpstreamPolicy, spec: &DeploymentSpec) -> Res {
    let pm = &spec.provider_model;
    if pm.trim().is_empty() || pm.len() > MAX_MODEL_LEN || pm.chars().any(char::is_control) {
        return Err(format!(
            "providerModel: 1–{MAX_MODEL_LEN} Zeichen ohne Steuerzeichen"
        ));
    }
    if !(1..=MAX_WEIGHT).contains(&spec.weight) {
        return Err(format!(
            "weight: 1–{MAX_WEIGHT} (zum Abschalten `enabled: false`)"
        ));
    }
    if spec.capacity.rpm == Some(0) || spec.capacity.tpm == Some(0) {
        return Err("capacity: rpm/tpm müssen > 0 sein (ohne Grenze weglassen)".into());
    }
    match (&spec.endpoint, implicit_local_endpoint(spec.provider)) {
        (Some(ep), _) => upstreams
            .check_endpoint(ep)
            .map_err(|m| format!("endpoint: {m}"))?,
        (None, Some(default)) => upstreams.check_endpoint(default).map_err(|m| {
            format!("endpoint: Standard-Endpunkt {default} des Anbieters ist nicht erlaubt: {m}")
        })?,
        (None, None) if endpoint_required(spec.provider) => {
            return Err(format!(
                "endpoint: für Anbieter {:?} Pflicht (kein Standard-Endpunkt)",
                spec.provider.as_str()
            ));
        }
        (None, None) => {}
    }
    if spec.provider == ProviderKind::Mock && !upstreams.allows_mock() {
        return Err(
            "provider: mock ist gesperrt (admin.allow_mock: false in der Startkonfiguration)"
                .into(),
        );
    }
    deployment_settings(&spec.settings)?;
    if spec.provider == ProviderKind::AzureOpenAi {
        azure(spec)?;
        azure_auth(upstreams, spec)?;
    }
    if spec.provider == ProviderKind::Anthropic {
        azure_auth(upstreams, spec)?;
        if setting_str(spec, "auth")? == Some("entraId") && spec.endpoint.is_none() {
            // Ein Entra-ID-Token gehört nie an den Standard-Endpunkt api.anthropic.com.
            return Err(
                "endpoint: bei settings.auth entraId Pflicht (Azure AI Foundry, z. B. \
                 https://<ressource>.services.ai.azure.com/anthropic)"
                    .into(),
            );
        }
    }
    if spec.provider == ProviderKind::Bedrock
        && setting_str(spec, "region")?.is_none_or(str::is_empty)
    {
        return Err("settings.region: für AWS Bedrock Pflicht (z. B. \"eu-central-1\")".into());
    }
    if let Some(ca) = setting_str(spec, "caFile")? {
        ca_file(upstreams, ca)?;
    }
    Ok(())
}

/// `settings.caFile` (F48, F177): PEM-Datei innerhalb von `upstream.ca_dir` (relativ =
/// darin, absolut = muss darin liegen, ohne `..`). Ohne `ca_dir` verboten. Ob die Datei lesbar
/// ist und Symlinks im Verzeichnis bleiben, prüfen Admin-API und Routenaufbau auf dem Gateway
/// (GitOps per CLI kann auf einem anderen Rechner laufen).
fn ca_file(upstreams: &UpstreamPolicy, path: &str) -> Res {
    let Some(dir) = upstreams.ca_dir() else {
        return Err(
            "settings.caFile: nur mit upstream.ca_dir in der Startkonfiguration erlaubt (F177)"
                .into(),
        );
    };
    crate::paths::lexical_within(dir, path)
        .map(drop)
        .map_err(|e| format!("settings.caFile: {e}"))
}

/// Azure OpenAI: Deployment-Name (`providerModel`) als Pfadsegment, `settings.apiVersion`.
fn azure(spec: &DeploymentSpec) -> Res {
    let version = setting_str(spec, "apiVersion")?;
    if let Some(v) = version
        && (v.is_empty()
            || !v
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.'))
    {
        return Err(format!(
            "settings.apiVersion {v:?} ungültig (z. B. \"2024-10-21\")"
        ));
    }
    match setting_str(spec, "apiStyle")?.unwrap_or("deployment") {
        "deployment" => {
            if !spec
                .provider_model
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
            {
                return Err(format!(
                    "providerModel {:?} ist kein gültiger Azure-Deployment-Name \
                     (Buchstaben, Ziffern, - _ .)",
                    spec.provider_model
                ));
            }
            if version.is_none() {
                return Err(
                    "settings.apiVersion: für Azure OpenAI (apiStyle deployment) Pflicht".into(),
                );
            }
            Ok(())
        }
        "v1" => Ok(()),
        other => Err(format!(
            "settings.apiStyle {other:?} unbekannt (deployment, v1)"
        )),
    }
}

/// Arten der Entra-ID-Anmeldung (`settings.entraCredential`, AIGW-82).
pub const ENTRA_CREDENTIALS: &[&str] = &[
    "clientSecret",
    "clientCertificate",
    "workloadIdentity",
    "managedIdentity",
];
/// Erlaubte Scopes für Workload/Managed Identity (Identität des Gateways selbst).
pub const ENTRA_AMBIENT_SCOPES: &[&str] = &[
    "https://cognitiveservices.azure.com/.default",
    "https://cognitiveservices.azure.us/.default",
    "https://cognitiveservices.azure.cn/.default",
];

/// Tenant-/Client-ID (GUID oder Domänenname).
fn entra_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_')
}

/// Azure OpenAI und Anthropic (Claude in Azure AI Foundry): `settings.auth`
/// (`apiKey`/`entraId`) und die Entra-ID-Einstellungen
/// (`entraCredential`, `entraAuthority` – nur `https://host`, SSRF-Regeln –, `entraScope`,
/// `entraTenantId`, `entraClientId`).
fn azure_auth(upstreams: &UpstreamPolicy, spec: &DeploymentSpec) -> Res {
    match setting_str(spec, "auth")?.unwrap_or("apiKey") {
        "apiKey" => return Ok(()),
        "entraId" => {}
        other => {
            return Err(format!(
                "settings.auth {other:?} unbekannt (apiKey, entraId)"
            ));
        }
    }
    let method = setting_str(spec, "entraCredential")?;
    if let Some(m) = method
        && !ENTRA_CREDENTIALS.contains(&m)
    {
        return Err(format!(
            "settings.entraCredential {m:?} unbekannt ({})",
            ENTRA_CREDENTIALS.join(", ")
        ));
    }
    entra_values(
        upstreams,
        method,
        &EntraValues {
            authority: setting_str(spec, "entraAuthority")?,
            scope: setting_str(spec, "entraScope")?,
            tenant_id: setting_str(spec, "entraTenantId")?,
            client_id: setting_str(spec, "entraClientId")?,
        },
        |k| format!("settings.entra{}", upper_first(k)),
    )
}

fn upper_first(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_ascii_uppercase().to_string() + c.as_str())
        .unwrap_or_default()
}

/// Nicht geheime Entra-ID-Werte (aus `settings.entra*` bzw. `spec.entra`).
struct EntraValues<'a> {
    authority: Option<&'a str>,
    scope: Option<&'a str>,
    tenant_id: Option<&'a str>,
    client_id: Option<&'a str>,
}

/// Gemeinsame Regeln für Authority (nur `https://host`, SSRF-Regeln, nicht bei Workload
/// Identity), Scope (bei Workload/Managed Identity nur Cognitive Services) und IDs.
/// `path(k)` bildet den Feldpfad für Meldungen (`k` = `authority`, `scope`, `tenantId`,
/// `clientId`).
fn entra_values(
    upstreams: &UpstreamPolicy,
    method: Option<&str>,
    v: &EntraValues<'_>,
    path: impl Fn(&str) -> String,
) -> Res {
    let ambient = matches!(method, Some("workloadIdentity" | "managedIdentity"));
    if let Some(a) = v.authority {
        let p = path("authority");
        if method == Some("workloadIdentity") {
            return Err(format!(
                "{p}: bei workloadIdentity nicht erlaubt (kommt aus AZURE_AUTHORITY_HOST)"
            ));
        }
        let rest = a.strip_prefix("https://").ok_or_else(|| {
            format!("{p}: https://host erwartet (z. B. https://login.microsoftonline.us)")
        })?;
        if rest.trim_end_matches('/').contains(['/', '?', '#']) {
            return Err(format!("{p}: nur Schema und Host (ohne Pfad/Abfrage)"));
        }
        upstreams
            .check_endpoint(a)
            .map_err(|m| format!("{p}: {m}"))?;
    }
    if let Some(scope) = v.scope {
        let p = path("scope");
        if scope.is_empty() || scope.len() > 512 || scope.chars().any(char::is_control) {
            return Err(format!("{p}: 1–512 Zeichen ohne Steuerzeichen"));
        }
        if ambient && !ENTRA_AMBIENT_SCOPES.contains(&scope) {
            return Err(format!(
                "{p}: bei Workload/Managed Identity nur {}",
                ENTRA_AMBIENT_SCOPES.join(", ")
            ));
        }
    }
    for (key, val) in [("tenantId", v.tenant_id), ("clientId", v.client_id)] {
        if let Some(val) = val
            && !entra_id(val)
        {
            return Err(format!("{} ungültig (GUID oder Domänenname)", path(key)));
        }
    }
    Ok(())
}

/// Anbieter, deren Zugangsdaten eine Entra-ID-Anmeldung (`spec.entra`) beschreiben dürfen
/// (AIGW-84): Azure OpenAI und Claude in Azure AI Foundry.
pub const ENTRA_PROVIDERS: &[ProviderKind] = &[ProviderKind::AzureOpenAi, ProviderKind::Anthropic];
/// Erlaubte Felder in `spec.data` neben `spec.entra`.
pub const ENTRA_DATA_FIELDS: &[&str] =
    &["tenantId", "clientId", "clientSecret", "clientCertificate"];

/// Zugangsdaten (AIGW-65/84): ohne `entra` wie [`credential_fields`]. Mit `entra` (nur
/// [`ENTRA_PROVIDERS`]): `data` enthält höchstens [`ENTRA_DATA_FIELDS`] – das Geheimnis der
/// gewählten Art (`clientSecret` bzw. `clientCertificate`, Pflicht), bei Managed/Workload
/// Identity kein Geheimnis; Tenant- und Client-ID entweder in `entra` oder in `data`
/// (Service Principal: beide Pflicht); Authority/Scope wie bei `settings.entra*`.
pub fn credential<'a>(
    upstreams: &UpstreamPolicy,
    provider: ProviderKind,
    entra: Option<&EntraAuth>,
    fields: impl IntoIterator<Item = &'a str>,
) -> Res {
    let Some(e) = entra else {
        return credential_fields(provider, fields);
    };
    if !ENTRA_PROVIDERS.contains(&provider) {
        return Err(format!(
            "spec.entra: nur für Anbieter azure-openai und anthropic (nicht {:?})",
            provider.as_str()
        ));
    }
    let fields: BTreeSet<&str> = fields.into_iter().collect();
    if let Some(bad) = fields.iter().find(|f| !ENTRA_DATA_FIELDS.contains(f)) {
        return Err(format!(
            "spec.data.{bad}: mit spec.entra nur {}",
            ENTRA_DATA_FIELDS.join(", ")
        ));
    }
    let secret = match e.credential {
        EntraCredentialKind::ClientSecret => Some("clientSecret"),
        EntraCredentialKind::ClientCertificate => Some("clientCertificate"),
        EntraCredentialKind::ManagedIdentity | EntraCredentialKind::WorkloadIdentity => None,
    };
    for f in ["clientSecret", "clientCertificate"] {
        if fields.contains(f) && secret != Some(f) {
            return Err(format!(
                "spec.data.{f}: passt nicht zu spec.entra.credential {:?}",
                e.credential.as_str()
            ));
        }
    }
    if let Some(f) = secret
        && !fields.contains(f)
    {
        return Err(format!(
            "spec.data.{f}: bei spec.entra.credential {:?} Pflicht",
            e.credential.as_str()
        ));
    }
    for (key, v) in [("tenantId", &e.tenant_id), ("clientId", &e.client_id)] {
        let in_data = fields.contains(key);
        if v.is_some() && in_data {
            return Err(format!(
                "{key}: entweder in spec.entra oder in spec.data angeben, nicht in beiden"
            ));
        }
        if secret.is_some() && v.is_none() && !in_data {
            return Err(format!(
                "spec.entra.{key}: bei spec.entra.credential {:?} Pflicht",
                e.credential.as_str()
            ));
        }
    }
    entra_values(
        upstreams,
        Some(e.credential.as_str()),
        &EntraValues {
            authority: e.authority.as_deref(),
            scope: e.scope.as_deref(),
            tenant_id: e.tenant_id.as_deref(),
            client_id: e.client_id.as_deref(),
        },
        |k| format!("spec.entra.{k}"),
    )
}

/// Deployment mit Zugangsdaten (AIGW-84): Anbieter passend ([`deployment_credential`]); bei
/// Zugangsdaten mit `entra` darf `settings.auth` nur fehlen oder `entraId` sein, und die
/// zusammengeführten Einstellungen ([`EntraAuth::apply_to`]) müssen gültig sein (z. B.
/// `endpoint` Pflicht für Claude in Foundry).
pub fn deployment_credential_auth(
    upstreams: &UpstreamPolicy,
    spec: &DeploymentSpec,
    credential: &str,
    credential_provider: ProviderKind,
    entra: Option<&EntraAuth>,
) -> Res {
    deployment_credential(spec, credential, credential_provider)?;
    let Some(e) = entra else {
        return Ok(());
    };
    if let Some(a) = setting_str(spec, "auth")?
        && a != "entraId"
    {
        return Err(format!(
            "settings.auth {a:?} passt nicht zu den Zugangsdaten {credential:?} mit Entra ID \
             (weglassen oder entraId)"
        ));
    }
    let mut merged = spec.clone();
    if e.apply_to(&mut merged.settings) {
        deployment(upstreams, &merged)
            .map_err(|m| format!("mit Zugangsdaten {credential:?}: {m}"))?;
    }
    Ok(())
}

/// Kompatibilitätsliste Zugangsdaten-Anbieter → weitere Deployment-Anbieter (F65): ein
/// OpenAI-Key passt auch zu OpenAI-kompatiblen Endpunkten (`openai-compatible`, `vllm`,
/// `ollama`; dieselbe Bearer-Anmeldung mit `apiKey`). Gleicher Anbieter passt immer.
pub const CREDENTIAL_COMPAT: &[(ProviderKind, &[ProviderKind])] = &[(
    ProviderKind::OpenAi,
    &[
        ProviderKind::OpenAiCompatible,
        ProviderKind::Vllm,
        ProviderKind::Ollama,
    ],
)];

/// Dürfen Zugangsdaten des Anbieters `credential` von einem Deployment des Anbieters
/// `deployment` genutzt werden (F65)?
pub fn credential_compatible(credential: ProviderKind, deployment: ProviderKind) -> bool {
    credential == deployment
        || CREDENTIAL_COMPAT
            .iter()
            .any(|(c, deps)| *c == credential && deps.contains(&deployment))
}

/// Zugangsdaten müssen zum Anbieter des Deployments passen (gleicher Anbieter oder laut
/// [`CREDENTIAL_COMPAT`]).
pub fn deployment_credential(
    spec: &DeploymentSpec,
    credential: &str,
    credential_provider: ProviderKind,
) -> Res {
    if credential_compatible(credential_provider, spec.provider) {
        Ok(())
    } else {
        Err(format!(
            "credential: {credential:?} gehört zu Anbieter {:?} und passt nicht zum Anbieter \
             {:?} des Deployments (kompatibel: gleicher Anbieter; openai auch für \
             openai-compatible, vllm, ollama)",
            credential_provider.as_str(),
            spec.provider.as_str()
        ))
    }
}

// ---------------------------------------------------------------------------------------------
// Zugangsdaten
// ---------------------------------------------------------------------------------------------

/// Pflichtfelder je Anbieter: eine der Alternativen muss vollständig vorhanden sein
/// (leer = keine Vorgabe).
pub fn required_credential_fields(p: ProviderKind) -> &'static [&'static [&'static str]] {
    match p {
        ProviderKind::OpenAi | ProviderKind::Mistral => &[&["apiKey"]],
        // Anthropic: API-Key oder Entra ID (Claude in Azure AI Foundry).
        ProviderKind::AzureOpenAi | ProviderKind::Anthropic => &[
            &["apiKey"],
            &["tenantId", "clientId", "clientSecret"],
            &["tenantId", "clientId", "clientCertificate"],
        ],
        ProviderKind::Bedrock => &[&["accessKeyId", "secretAccessKey"], &["apiKey"]],
        ProviderKind::Vertex => &[
            &["serviceAccountJson"],
            &["clientEmail", "privateKey"],
            &["accessToken"],
            &["apiKey"],
        ],
        _ => &[],
    }
}

fn valid_field_name(f: &str) -> bool {
    let mut b = f.bytes();
    f.len() <= 64
        && b.next().is_some_and(|c| c.is_ascii_alphabetic())
        && b.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

/// Feldmenge der Zugangsdaten: Anzahl, Feldnamen, Pflichtfelder des Anbieters.
pub fn credential_fields<'a>(
    provider: ProviderKind,
    fields: impl IntoIterator<Item = &'a str>,
) -> Res {
    let fields: BTreeSet<&str> = fields.into_iter().collect();
    if fields.is_empty() {
        return Err("spec.data: mindestens ein Feld angeben".into());
    }
    if fields.len() > MAX_CREDENTIAL_FIELDS {
        return Err(format!(
            "spec.data: höchstens {MAX_CREDENTIAL_FIELDS} Felder"
        ));
    }
    if let Some(bad) = fields.iter().find(|f| !valid_field_name(f)) {
        return Err(format!(
            "spec.data: ungültiger Feldname {bad:?} (Buchstabe, dann Buchstaben/Ziffern/_, max. 64)"
        ));
    }
    let alts = required_credential_fields(provider);
    if alts.is_empty()
        || alts
            .iter()
            .any(|alt| alt.iter().all(|f| fields.contains(f)))
    {
        return Ok(());
    }
    let wanted: Vec<String> = alts.iter().map(|a| a.join(" + ")).collect();
    Err(format!(
        "spec.data: Anbieter {:?} braucht {}",
        provider.as_str(),
        wanted.join(" oder ")
    ))
}

/// Länge eines Zugangsdaten-Werts (ohne den Wert zu nennen).
pub fn credential_value(field: &str, value: &str) -> Res {
    if value.is_empty() || value.len() > MAX_CREDENTIAL_VALUE_LEN {
        return Err(format!(
            "spec.data.{field}.value: 1–{MAX_CREDENTIAL_VALUE_LEN} Byte"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Preise
// ---------------------------------------------------------------------------------------------

/// Modellname und nicht negative Beträge.
pub fn price(p: &PriceSpec) -> Res {
    if p.model.trim().is_empty()
        || p.model.len() > MAX_MODEL_LEN
        || p.model.chars().any(char::is_control)
    {
        return Err(format!(
            "model: 1–{MAX_MODEL_LEN} Zeichen ohne Steuerzeichen"
        ));
    }
    let t = &p.per_million_tokens;
    let amounts: [(&str, Option<Amount>); 12] = [
        ("perMillionTokens.input", t.input),
        ("perMillionTokens.output", t.output),
        ("perMillionTokens.cachedInput", t.cached_input),
        ("perMillionTokens.cacheWrite", t.cache_write),
        ("perMillionTokens.reasoning", t.reasoning),
        ("perMillionTokens.audioInput", t.audio_input),
        ("perMillionTokens.audioOutput", t.audio_output),
        ("perMillionTokens.imageInput", t.image_input),
        ("perRequest", p.per_request),
        ("perImage", p.per_image),
        ("perAudioMinute", p.per_audio_minute),
        ("perMillionCharacters", p.per_million_characters),
    ];
    for (field, a) in amounts {
        if a.is_some_and(|a| a.nanos() < 0) {
            return Err(format!("{field}: darf nicht negativ sein"));
        }
    }
    Ok(())
}

fn fmt_time(t: OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| t.to_string())
}

/// Ist die Preisversion zum Zeitpunkt `now` bereits gültig (und damit unveränderlich)?
pub fn price_effective(p: &PriceSpec, now: OffsetDateTime) -> bool {
    p.valid_from <= now
}

/// Änderung einer Preisversion: gültige Versionen sind unveränderlich (nur Labels), künftige
/// dürfen nicht in die Vergangenheit verlegt werden.
pub fn price_change(current: &PriceSpec, new: &PriceSpec, now: OffsetDateTime) -> Res {
    if current == new {
        return Ok(());
    }
    if price_effective(current, now) {
        return Err(format!(
            "Preisversion gilt seit {} und ist unveränderlich – neue Version mit späterem \
             validFrom anlegen (nur Labels lassen sich ändern)",
            fmt_time(current.valid_from)
        ));
    }
    if price_effective(new, now) {
        return Err(
            "validFrom einer künftigen Version kann nicht in die Vergangenheit verlegt werden – \
             stattdessen neue Version anlegen"
                .into(),
        );
    }
    Ok(())
}

/// Löschen einer Preisversion: nur, solange sie noch nicht gilt.
pub fn price_delete(current: &PriceSpec, now: OffsetDateTime) -> Res {
    if price_effective(current, now) {
        return Err(format!(
            "Preisversion gilt seit {} und kann nicht gelöscht werden – neue Version anlegen",
            fmt_time(current.valid_from)
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{Capacity, Fallback, TokenPrices};
    use serde_json::json;

    #[test]
    fn ca_file_setting() {
        let caf = |v: Value| {
            dep(
                ProviderKind::OpenAi,
                None,
                serde_json::json!({ "caFile": v }),
            )
        };
        // ohne upstream.ca_dir verboten (F177)
        let strict = UpstreamPolicy::strict();
        let err = deployment(&strict, &caf(json!("/etc/aigw/ca/x.pem"))).unwrap_err();
        assert!(err.contains("ca_dir"), "{err}");
        let p = UpstreamPolicy::strict().with_ca_dir(Some("/etc/aigw/ca".into()));
        assert!(deployment(&p, &caf(json!("/etc/aigw/ca/x.pem"))).is_ok());
        assert!(deployment(&p, &caf(json!("x.pem"))).is_ok());
        for bad in [
            json!("/etc/aigw/ca/../../root/x"),
            json!("/etc/passwd"),
            json!("../x.pem"),
            json!(""),
            json!(1),
        ] {
            assert!(deployment(&p, &caf(bad.clone())).is_err(), "{bad}");
        }
    }

    fn dep(provider: ProviderKind, endpoint: Option<&str>, settings: Value) -> DeploymentSpec {
        DeploymentSpec {
            model_group: "g".into(),
            provider,
            endpoint: endpoint.map(str::to_owned),
            provider_model: "gpt-4o".into(),
            credential: None,
            weight: 1,
            priority: 0,
            capacity: Capacity::default(),
            enabled: true,
            settings: settings.as_object().cloned().unwrap_or_default(),
        }
    }

    #[test]
    fn deployment_rules() {
        let p = UpstreamPolicy::strict();
        assert!(deployment(&p, &dep(ProviderKind::OpenAi, None, json!({}))).is_ok());
        assert!(deployment(&p, &dep(ProviderKind::Vllm, None, json!({}))).is_err());
        // Ollama-Standard (http://localhost) braucht eine Freischaltung
        assert!(deployment(&p, &dep(ProviderKind::Ollama, None, json!({}))).is_err());
        let lan = dep(
            ProviderKind::Ollama,
            Some("http://192.168.1.5:11434"),
            json!({}),
        );
        assert!(deployment(&p, &lan).is_err());
        let mut d = dep(ProviderKind::OpenAi, None, json!({}));
        d.weight = 0;
        assert!(deployment(&p, &d).is_err());
        let mut d = dep(ProviderKind::OpenAi, None, json!({}));
        d.capacity.rpm = Some(0);
        assert!(deployment(&p, &d).is_err());
        let mut d = dep(ProviderKind::OpenAi, None, json!({}));
        d.provider_model = " ".into();
        assert!(deployment(&p, &d).is_err());
    }

    #[test]
    fn mock_and_data_plane_settings() {
        // F85: mock standardmäßig erlaubt, mit admin.allow_mock: false gesperrt.
        let p = UpstreamPolicy::strict();
        assert!(deployment(&p, &dep(ProviderKind::Mock, None, json!({}))).is_ok());
        let no_mock = UpstreamPolicy::from_config(&crate::config::AdminConfig {
            allow_mock: false,
            ..crate::config::AdminConfig::default()
        })
        .unwrap();
        assert!(!no_mock.allows_mock());
        assert!(deployment(&no_mock, &dep(ProviderKind::Mock, None, json!({}))).is_err());
        assert!(deployment(&no_mock, &dep(ProviderKind::OpenAi, None, json!({}))).is_ok());
        // F44/F59/F98: Einstellungen je Deployment.
        let ok = json!({"timeouts": {"connectMs": 1000, "firstByteMs": 5000, "idleMs": 5000,
                                     "totalMs": 60000},
                        "thinkingBudgetPolicy": "cap-budget", "propagateTraceContext": false});
        assert!(deployment(&p, &dep(ProviderKind::OpenAi, None, ok)).is_ok());
        for bad in [
            json!({"timeouts": {"totalMs": 0}}),
            json!({"timeouts": {"readMs": 1}}),
            json!({"timeouts": []}),
            json!({"thinkingBudgetPolicy": "raise"}),
            json!({"propagateTraceContext": "yes"}),
        ] {
            assert!(
                deployment(&p, &dep(ProviderKind::OpenAi, None, bad.clone())).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn fallback_chain_length_is_limited() {
        let target = ModelGroupSpec::default();
        let names: Vec<String> = (0..=MAX_FALLBACKS).map(|i| format!("g{i}")).collect();
        let groups: BTreeMap<&str, &ModelGroupSpec> =
            names.iter().map(|n| (n.as_str(), &target)).collect();
        let chain = |n: usize| ModelGroupSpec {
            fallbacks: names[..n]
                .iter()
                .map(|g| Fallback {
                    model_group: g.clone(),
                    on: Vec::new(),
                })
                .collect(),
            ..ModelGroupSpec::default()
        };
        assert!(fallbacks(&groups, "own", &chain(MAX_FALLBACKS)).is_ok());
        assert!(fallbacks(&groups, "own", &chain(MAX_FALLBACKS + 1)).is_err());
    }

    #[test]
    fn azure_rules() {
        let ep = Some("https://x.openai.azure.com");
        let ok = dep(
            ProviderKind::AzureOpenAi,
            ep,
            json!({"apiVersion": "2024-10-21"}),
        );
        assert!(azure(&ok).is_ok());
        assert!(azure(&dep(ProviderKind::AzureOpenAi, ep, json!({}))).is_err());
        assert!(
            azure(&dep(
                ProviderKind::AzureOpenAi,
                ep,
                json!({"apiStyle": "v1"})
            ))
            .is_ok()
        );
        assert!(
            azure(&dep(
                ProviderKind::AzureOpenAi,
                ep,
                json!({"apiVersion": "a/b"})
            ))
            .is_err()
        );
        assert!(
            azure(&dep(
                ProviderKind::AzureOpenAi,
                ep,
                json!({"apiVersion": 1})
            ))
            .is_err()
        );
        let mut bad = ok.clone();
        bad.provider_model = "a/b".into();
        assert!(azure(&bad).is_err());
    }

    #[test]
    fn azure_entra_rules() {
        let ep = Some("https://x.openai.azure.com");
        let p = UpstreamPolicy::strict();
        let check = |settings: Value| azure_auth(&p, &dep(ProviderKind::AzureOpenAi, ep, settings));
        assert!(check(json!({})).is_ok());
        assert!(check(json!({"auth": "entraId"})).is_ok());
        assert!(check(json!({"auth": "magie"})).is_err());
        assert!(check(json!({"auth": "entraId", "entraCredential": "x"})).is_err());
        assert!(
            check(
                json!({"auth": "entraId", "entraCredential": "managedIdentity",
                         "entraClientId": "11111111-2222-3333-4444-555555555555"})
            )
            .is_ok()
        );
        assert!(
            check(
                json!({"auth": "entraId", "entraAuthority": "https://login.microsoftonline.us/"})
            )
            .is_ok()
        );
        for bad in [
            json!({"auth": "entraId", "entraAuthority": "http://login.microsoftonline.us"}),
            json!({"auth": "entraId", "entraAuthority": "https://login.example/x"}),
            json!({"auth": "entraId", "entraAuthority": "https://10.0.0.1"}),
            json!({"auth": "entraId", "entraCredential": "workloadIdentity",
                   "entraAuthority": "https://login.microsoftonline.us"}),
            json!({"auth": "entraId", "entraCredential": "managedIdentity",
                   "entraScope": "https://management.azure.com/.default"}),
            json!({"auth": "entraId", "entraTenantId": "../x"}),
        ] {
            assert!(check(bad.clone()).is_err(), "{bad}");
        }
        assert!(
            credential_fields(
                ProviderKind::AzureOpenAi,
                ["tenantId", "clientId", "clientCertificate"]
            )
            .is_ok()
        );
    }

    fn entra(kind: EntraCredentialKind) -> EntraAuth {
        EntraAuth {
            credential: kind,
            tenant_id: None,
            client_id: None,
            authority: None,
            scope: None,
        }
    }

    #[test]
    fn credential_entra_rules() {
        use EntraCredentialKind as K;
        let p = UpstreamPolicy::strict();
        let az = ProviderKind::AzureOpenAi;
        let mi = entra(K::ManagedIdentity);
        let check = |prov: ProviderKind, e: &EntraAuth, fields: &[&str]| {
            credential(&p, prov, Some(e), fields.iter().copied())
        };
        // Managed Identity: keine Felder nötig (anders als ohne entra).
        assert!(check(az, &mi, &[]).is_ok());
        assert!(check(ProviderKind::Anthropic, &mi, &[]).is_ok());
        assert!(credential(&p, az, None, []).is_err());
        // Nur Azure OpenAI und Anthropic.
        assert!(check(ProviderKind::OpenAi, &mi, &[]).is_err());
        // Bei Managed Identity kein gespeichertes Geheimnis, keine fremden Felder.
        assert!(check(az, &mi, &["clientSecret"]).is_err());
        assert!(check(az, &mi, &["apiKey"]).is_err());
        // Service Principal: Geheimnis Pflicht, IDs aus entra oder data (nicht beides).
        let sp = EntraAuth {
            tenant_id: Some("contoso.onmicrosoft.com".into()),
            client_id: Some("11111111-2222-3333-4444-555555555555".into()),
            ..entra(K::ClientSecret)
        };
        assert!(check(az, &sp, &["clientSecret"]).is_ok());
        assert!(check(az, &sp, &[]).is_err());
        assert!(check(az, &sp, &["clientCertificate"]).is_err());
        assert!(check(az, &sp, &["clientSecret", "tenantId"]).is_err());
        let bare = entra(K::ClientCertificate);
        assert!(check(az, &bare, &["clientCertificate"]).is_err());
        assert!(check(az, &bare, &["clientCertificate", "tenantId", "clientId"]).is_ok());
        // Authority/Scope/IDs wie bei settings.entra*.
        for bad in [
            EntraAuth {
                authority: Some("http://login.microsoftonline.us".into()),
                ..mi.clone()
            },
            EntraAuth {
                scope: Some("https://management.azure.com/.default".into()),
                ..mi.clone()
            },
            EntraAuth {
                client_id: Some("../x".into()),
                ..mi.clone()
            },
            EntraAuth {
                authority: Some("https://login.microsoftonline.us".into()),
                ..entra(K::WorkloadIdentity)
            },
        ] {
            let e = check(az, &bad, &[]);
            assert!(
                e.as_ref().is_err_and(|m| m.starts_with("spec.entra.")),
                "{e:?}"
            );
        }
    }

    #[test]
    fn deployment_with_entra_credential() {
        let p = UpstreamPolicy::strict();
        let mi = EntraAuth {
            client_id: Some("11111111-2222-3333-4444-555555555555".into()),
            ..entra(EntraCredentialKind::ManagedIdentity)
        };
        let ep = Some("https://res.services.ai.azure.com/anthropic");
        let check = |endpoint: Option<&str>, settings: Value, e: Option<&EntraAuth>| {
            deployment_credential_auth(
                &p,
                &dep(ProviderKind::Anthropic, endpoint, settings),
                "c",
                ProviderKind::Anthropic,
                e,
            )
        };
        assert!(check(ep, json!({}), Some(&mi)).is_ok());
        assert!(check(ep, json!({"auth": "entraId"}), Some(&mi)).is_ok());
        assert!(check(ep, json!({"auth": "apiKey"}), Some(&mi)).is_err());
        // Ohne endpoint: Entra-Token nie an api.anthropic.com.
        let e = check(None, json!({}), Some(&mi));
        assert!(e.as_ref().is_err_and(|m| m.contains("endpoint")), "{e:?}");
        // Ohne entra wie bisher.
        assert!(check(None, json!({}), None).is_ok());
        // Zusammenführen: Deployment-Einstellungen haben Vorrang.
        let mut s = json!({"entraClientId": "eigene"})
            .as_object()
            .unwrap()
            .clone();
        assert!(mi.apply_to(&mut s));
        assert_eq!(s["auth"], "entraId");
        assert_eq!(s["entraCredential"], "managedIdentity");
        assert_eq!(s["entraClientId"], "eigene");
        let mut s = json!({"auth": "apiKey"}).as_object().unwrap().clone();
        assert!(!mi.apply_to(&mut s));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn pricing_settings_rules() {
        let check = |v: serde_json::Value| {
            let serde_json::Value::Object(o) = serde_json::json!({ "pricing": v }) else {
                unreachable!()
            };
            deployment_settings(&o)
        };
        assert!(
            check(serde_json::json!({
                "region": "swedencentral", "deploymentType": "data-zone",
                "model": "gpt-5.4-mini", "source": "azure", "meter": "5.4 mini"
            }))
            .is_ok()
        );
        assert!(check(serde_json::json!({"region": "Sweden Central"})).is_err());
        assert!(check(serde_json::json!({"deploymentType": "GlobalStandard"})).is_err());
        assert!(check(serde_json::json!({"source": "web"})).is_err());
        assert!(check(serde_json::json!({"regoin": "x"})).is_err());
        assert!(check(serde_json::json!("swedencentral")).is_err());
        assert!(check(serde_json::json!({"model": 5})).is_err());
    }

    #[test]
    fn anthropic_foundry_rules() {
        let p = UpstreamPolicy::strict();
        let ep = Some("https://res.services.ai.azure.com/anthropic");
        let check = |endpoint: Option<&str>, settings: Value| {
            deployment(&p, &dep(ProviderKind::Anthropic, endpoint, settings))
        };
        assert!(check(None, json!({})).is_ok());
        assert!(check(ep, json!({"auth": "apiKey"})).is_ok());
        assert!(
            check(
                ep,
                json!({"auth": "entraId", "entraCredential": "managedIdentity",
                       "entraClientId": "11111111-2222-3333-4444-555555555555"})
            )
            .is_ok()
        );
        // Kein Entra-Token an api.anthropic.com.
        let e = check(
            None,
            json!({"auth": "entraId", "entraCredential": "managedIdentity"}),
        );
        assert!(e.as_ref().is_err_and(|m| m.contains("endpoint")), "{e:?}");
        assert!(check(ep, json!({"auth": "magie"})).is_err());
        assert!(
            check(
                ep,
                json!({"auth": "entraId", "entraCredential": "managedIdentity",
                       "entraScope": "https://management.azure.com/.default"})
            )
            .is_err()
        );
        assert!(
            credential_fields(
                ProviderKind::Anthropic,
                ["tenantId", "clientId", "clientSecret"]
            )
            .is_ok()
        );
        assert!(credential_fields(ProviderKind::Anthropic, ["apiKey"]).is_ok());
    }

    #[test]
    fn user_identity_rules() {
        use crate::resource::RoleBinding;
        let u = |iss: Option<&str>, sub: Option<&str>| UserSpec {
            oidc_issuer: iss.map(Into::into),
            oidc_subject: sub.map(Into::into),
            ..UserSpec::default()
        };
        assert!(user(&u(None, None)).is_ok());
        assert!(user(&u(Some("https://idp"), Some("oid-1"))).is_ok());
        assert!(user(&u(Some("https://idp"), None)).is_err());
        assert!(user(&u(None, Some("x"))).is_err());
        assert!(user(&u(Some("https://idp"), Some(" "))).is_err());
        let mut g = u(None, None);
        g.roles = vec![RoleBinding {
            role: Role::Gitops,
            organization: Some("o".into()),
            team: None,
        }];
        assert!(user(&g).is_err());
        g.roles[0].organization = None;
        assert!(user(&g).is_ok());
    }

    #[test]
    fn credential_compatibility() {
        use ProviderKind as P;
        assert!(credential_compatible(P::OpenAi, P::OpenAi));
        for d in [P::OpenAiCompatible, P::Vllm, P::Ollama] {
            assert!(credential_compatible(P::OpenAi, d), "{d:?}");
            assert!(!credential_compatible(d, P::OpenAi), "{d:?}");
        }
        assert!(!credential_compatible(P::OpenAi, P::AzureOpenAi));
        assert!(!credential_compatible(P::Anthropic, P::OpenAi));
        let d = dep(P::Vllm, Some("https://vllm.example/v1"), json!({}));
        assert!(deployment_credential(&d, "oa", P::OpenAi).is_ok());
        let err = deployment_credential(&d, "an", P::Anthropic).unwrap_err();
        assert!(err.contains("passt nicht"), "{err}");
    }

    #[test]
    fn credential_rules() {
        assert!(credential_fields(ProviderKind::OpenAi, ["apiKey"]).is_ok());
        assert!(credential_fields(ProviderKind::OpenAi, ["token"]).is_err());
        assert!(credential_fields(ProviderKind::OpenAi, []).is_err());
        assert!(
            credential_fields(
                ProviderKind::AzureOpenAi,
                ["tenantId", "clientId", "clientSecret"]
            )
            .is_ok()
        );
        assert!(credential_fields(ProviderKind::AzureOpenAi, ["tenantId"]).is_err());
        assert!(credential_fields(ProviderKind::Bedrock, ["accessKeyId"]).is_err());
        assert!(credential_fields(ProviderKind::Bedrock, ["apiKey"]).is_ok());
        assert!(credential_fields(ProviderKind::Vertex, ["clientEmail", "privateKey"]).is_ok());
        assert!(credential_fields(ProviderKind::Vertex, ["privateKey"]).is_err());
        assert!(
            deployment(
                &UpstreamPolicy::strict(),
                &dep(ProviderKind::Bedrock, None, json!({}))
            )
            .is_err()
        );
        assert!(
            deployment(
                &UpstreamPolicy::strict(),
                &dep(
                    ProviderKind::Bedrock,
                    None,
                    json!({"region": "eu-central-1"})
                )
            )
            .is_ok()
        );
        assert!(credential_fields(ProviderKind::Ollama, ["anything"]).is_ok());
        assert!(credential_fields(ProviderKind::Ollama, ["1bad"]).is_err());
        assert!(credential_value("apiKey", "").is_err());
    }

    #[test]
    fn fallback_cycles() {
        let fb = |t: &str| ModelGroupSpec {
            fallbacks: vec![Fallback {
                model_group: t.into(),
                on: vec![],
            }],
            ..ModelGroupSpec::default()
        };
        let a = fb("b");
        let b = ModelGroupSpec::default();
        let groups: BTreeMap<&str, &ModelGroupSpec> = [("a", &a), ("b", &b)].into();
        assert!(fallbacks(&groups, "a", &a).is_ok());
        assert!(
            fallbacks(&groups, "b", &fb("a"))
                .unwrap_err()
                .contains("Zyklus")
        );
        assert!(fallbacks(&groups, "b", &fb("b")).is_err());
        assert!(fallbacks(&groups, "b", &fb("x")).is_err());
    }

    #[test]
    fn price_rules() {
        let at = |ts| PriceSpec {
            provider: None,
            model: "m".into(),
            valid_from: OffsetDateTime::from_unix_timestamp(ts)
                .unwrap_or(OffsetDateTime::UNIX_EPOCH),
            per_million_tokens: TokenPrices::default(),
            per_request: None,
            per_image: None,
            per_audio_minute: None,
            per_million_characters: None,
        };
        let now = OffsetDateTime::from_unix_timestamp(1_000).unwrap_or(OffsetDateTime::UNIX_EPOCH);
        assert!(price_change(&at(10), &at(10), now).is_ok());
        assert!(price_change(&at(10), &at(2_000), now).is_err());
        assert!(price_change(&at(2_000), &at(3_000), now).is_ok());
        assert!(price_change(&at(2_000), &at(500), now).is_err());
        assert!(price_delete(&at(10), now).is_err());
        assert!(price_delete(&at(2_000), now).is_ok());
        let mut neg = at(0);
        neg.per_request = Some(Amount::from_nanos(-1));
        assert!(price(&neg).is_err());
    }
}
