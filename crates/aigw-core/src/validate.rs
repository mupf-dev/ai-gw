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

use crate::resource::{Amount, DeploymentSpec, ModelGroupSpec, PriceSpec};
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

/// Fallbacks der Gruppe `own` (mit neuer `spec`) gegen den Gesamtbestand `groups`
/// (Name → Spezifikation; `own` darf fehlen oder veraltet sein): Ziel existiert (kanonischer
/// Name), nicht die Gruppe selbst, keine Doppelungen, keine Zyklen.
pub fn fallbacks(
    groups: &BTreeMap<&str, &ModelGroupSpec>,
    own: &str,
    spec: &ModelGroupSpec,
) -> Res {
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
    if spec.provider == ProviderKind::AzureOpenAi {
        azure(spec)?;
    }
    if spec.provider == ProviderKind::Bedrock
        && setting_str(spec, "region")?.is_none_or(str::is_empty)
    {
        return Err("settings.region: für AWS Bedrock Pflicht (z. B. \"eu-central-1\")".into());
    }
    Ok(())
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

/// Zugangsdaten müssen zum Anbieter des Deployments gehören.
pub fn deployment_credential(
    spec: &DeploymentSpec,
    credential: &str,
    credential_provider: ProviderKind,
) -> Res {
    if credential_provider == spec.provider {
        Ok(())
    } else {
        Err(format!(
            "credential: {credential:?} gehört zu Anbieter {:?}, das Deployment zu {:?}",
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
        ProviderKind::OpenAi | ProviderKind::Anthropic | ProviderKind::Mistral => &[&["apiKey"]],
        ProviderKind::AzureOpenAi => &[&["apiKey"], &["tenantId", "clientId", "clientSecret"]],
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
    let amounts: [(&str, Option<Amount>); 10] = [
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
