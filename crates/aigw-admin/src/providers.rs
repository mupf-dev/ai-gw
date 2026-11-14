// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Anbieter-Übersicht (AIGW-84): `GET /admin/api/v1/providers`.
//!
//! Fasst die Deployments je Anbieter und Endpunkt zusammen – auch ohne `ProviderCredential`
//! (z. B. Azure AI Foundry per Managed Identity mit Inline-Einstellungen): Anmeldeart(en),
//! Deployments, Modellgruppen und der Laufzeitzustand **dieser Instanz** (Circuit Breaker,
//! Health Checks, nicht nutzbare Deployments mit Grund). Die Anmeldeart ergibt sich aus den
//! wirksamen Einstellungen (Deployment-Einstellungen vor `ProviderCredential.spec.entra`) bzw.
//! den Feldnamen der Zugangsdaten; Werte erscheinen nie. Lesen darf jede Rolle mit Leserecht
//! auf den Katalog.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::sync::Arc;

use aigw_core::resource::{DeploymentSpec, ProviderCredential};
use aigw_core::types::ProviderKind;
use aigw_proxy::SnapshotRoutes;
use aigw_proxy::routing::{BreakerState, DeploymentRuntime};
use aigw_store::Store;
use axum::extract::{Extension, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse as _, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use utoipa::ToSchema;

use crate::auth::{AdminAuthenticator, Permission, Principal, Target, require_principal};
use crate::problem::Problem;

/// Pfad des Endpunkts.
pub const PROVIDERS_PATH: &str = "/admin/api/v1/providers";

/// Zustand des Routers.
#[derive(Clone)]
pub struct ProvidersState {
    /// Datenbank (Deployments, Zugangsdaten).
    pub store: Store,
    /// Anmeldung (dieselbe wie für die übrige Admin-API).
    pub auth: Arc<dyn AdminAuthenticator>,
    /// Routing dieser Instanz (fehlt: kein Laufzeitzustand, Status `unknown`).
    pub routes: Option<Arc<SnapshotRoutes>>,
}

impl fmt::Debug for ProvidersState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProvidersState")
            .field("routes", &self.routes.is_some())
            .finish_non_exhaustive()
    }
}

/// Anmeldeart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum AuthKind {
    /// API-Key (Zugangsdaten mit `apiKey`).
    ApiKey,
    /// Microsoft Entra ID (`settings.auth: entraId` bzw. `ProviderCredential.spec.entra`).
    EntraId,
    /// AWS-Zugangsschlüssel (SigV4).
    AwsAccessKey,
    /// Google-Service-Account.
    ServiceAccount,
    /// Statisches Access-Token.
    AccessToken,
    /// Sonstige Zugangsdaten.
    Credential,
    /// Ohne Anmeldung.
    None,
}

/// Eine Anmeldung, wie sie Deployments eines Endpunkts nutzen.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AuthUse {
    /// Art.
    pub kind: AuthKind,
    /// Bei Entra ID: `managedIdentity`, `workloadIdentity`, `clientSecret`,
    /// `clientCertificate`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entra_credential: Option<String>,
    /// Bei Entra ID: Client-ID aus den Einstellungen bzw. `spec.entra` (nicht geheim).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Name der Zugangsdaten (fehlt bei Inline-Einstellungen bzw. ohne Anmeldung).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

/// Zustand eines Deployments bzw. Endpunkts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum HealthState {
    /// Wird geroutet, Breaker geschlossen, Health Checks in Ordnung.
    Ok,
    /// Breaker halboffen bzw. fehlgeschlagene Health Checks (oder: einzelne Deployments eines
    /// Endpunkts gestört).
    Degraded,
    /// Breaker offen bzw. nicht nutzbar (alle Deployments eines Endpunkts).
    Down,
    /// Nicht nutzbar: Adapter ließ sich nicht bauen (Konfiguration, siehe `error`).
    Unusable,
    /// Abgeschaltet (`enabled: false`).
    Disabled,
    /// Kein Laufzeitzustand (Modellgruppe fehlt, Tabelle noch nicht neu gebaut).
    Unknown,
}

/// Circuit Breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum Breaker {
    /// Geschlossen.
    Closed,
    /// Offen.
    Open,
    /// Halboffen (Probe).
    HalfOpen,
}

/// Deployment in der Übersicht.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDeployment {
    /// Name.
    pub name: String,
    /// Modellgruppe.
    pub model_group: String,
    /// Modellname beim Anbieter.
    pub provider_model: String,
    /// Aktiv.
    pub enabled: bool,
    /// Anmeldung.
    pub auth: AuthUse,
    /// Zustand (dieser Instanz).
    pub state: HealthState,
    /// Circuit Breaker (nur bei gerouteten Deployments).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breaker: Option<Breaker>,
    /// Offene Anfragen dieser Instanz.
    pub in_flight: u32,
    /// Aufeinanderfolgende fehlgeschlagene Health Checks.
    pub health_failures: u32,
    /// Grund bei `unusable` (ohne Geheimnisse).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Anbieter + Endpunkt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderEndpoint {
    /// Anbieter.
    pub provider: ProviderKind,
    /// Endpunkt (fehlt = Standard-Endpunkt des Anbieters).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Genutzte Anmeldungen (sortiert, ohne Doppelte).
    pub auth: Vec<AuthUse>,
    /// Zusammengefasster Zustand.
    pub state: HealthState,
    /// Modellgruppen (sortiert).
    pub model_groups: Vec<String>,
    /// Deployments (nach Name).
    pub deployments: Vec<ProviderDeployment>,
}

/// Antwort von `GET /providers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderOverview {
    /// Laufzeitzustand verfügbar (sonst alle aktiven Deployments `unknown`).
    pub runtime: bool,
    /// Je Anbieter und Endpunkt (nach Anbieter, dann Endpunkt).
    pub items: Vec<ProviderEndpoint>,
}

/// Anmeldeart eines Deployments aus wirksamen Einstellungen und Feldnamen der Zugangsdaten.
pub(crate) fn auth_of(spec: &DeploymentSpec, cred: Option<&ProviderCredential>) -> AuthUse {
    let mut settings = spec.settings.clone();
    if let Some(e) = cred.and_then(|c| c.spec.entra.as_ref()) {
        e.apply_to(&mut settings);
    }
    let s = |k: &str| settings.get(k).and_then(|v| v.as_str()).map(str::to_owned);
    let credential = spec.credential.clone();
    let fields: BTreeSet<&str> = cred
        .map(|c| c.spec.fields.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let entra_capable = matches!(
        spec.provider,
        ProviderKind::AzureOpenAi | ProviderKind::Anthropic
    );
    if entra_capable && s("auth").as_deref() == Some("entraId") {
        let method = s("entraCredential").or_else(|| {
            ["clientSecret", "clientCertificate"]
                .into_iter()
                .find(|f| fields.contains(f))
                .map(str::to_owned)
        });
        return AuthUse {
            kind: AuthKind::EntraId,
            entra_credential: method,
            client_id: s("entraClientId"),
            credential,
        };
    }
    let kind = if cred.is_none() {
        AuthKind::None
    } else if fields.contains("apiKey") {
        AuthKind::ApiKey
    } else if fields.contains("accessKeyId") {
        AuthKind::AwsAccessKey
    } else if fields.contains("serviceAccountJson") || fields.contains("clientEmail") {
        AuthKind::ServiceAccount
    } else if fields.contains("accessToken") {
        AuthKind::AccessToken
    } else {
        AuthKind::Credential
    };
    AuthUse {
        kind,
        entra_credential: None,
        client_id: None,
        credential,
    }
}

fn deployment_state(
    enabled: bool,
    runtime: Option<&HashMap<aigw_core::resource::Id, DeploymentRuntime>>,
    id: aigw_core::resource::Id,
) -> (HealthState, Option<&DeploymentRuntime>) {
    if !enabled {
        return (HealthState::Disabled, None);
    }
    let Some(rt) = runtime.and_then(|r| r.get(&id)) else {
        return (HealthState::Unknown, None);
    };
    let state = if !rt.usable {
        HealthState::Unusable
    } else {
        match rt.breaker {
            Some(BreakerState::Open) => HealthState::Down,
            Some(BreakerState::HalfOpen) => HealthState::Degraded,
            _ if rt.health_failures > 0 => HealthState::Degraded,
            _ => HealthState::Ok,
        }
    };
    (state, Some(rt))
}

/// Zusammengefasster Zustand eines Endpunkts aus den Zuständen seiner Deployments.
pub(crate) fn endpoint_state(states: &[HealthState]) -> HealthState {
    let active: Vec<HealthState> = states
        .iter()
        .copied()
        .filter(|s| *s != HealthState::Disabled)
        .collect();
    let bad = |s: &HealthState| matches!(s, HealthState::Down | HealthState::Unusable);
    if active.is_empty() {
        HealthState::Disabled
    } else if active.iter().all(bad) {
        HealthState::Down
    } else if active.iter().any(|s| bad(s) || *s == HealthState::Degraded) {
        HealthState::Degraded
    } else if active.iter().all(|s| *s == HealthState::Unknown) {
        HealthState::Unknown
    } else {
        HealthState::Ok
    }
}

/// Anbieter-Übersicht je Anbieter und Endpunkt.
#[utoipa::path(
    get, path = "/admin/api/v1/providers", tag = "providers",
    responses(
        (status = 200, description = "Übersicht je Anbieter und Endpunkt", body = ProviderOverview),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub(crate) async fn provider_overview(
    State(st): State<ProvidersState>,
    Extension(p): Extension<Principal>,
) -> Result<Response, Problem> {
    p.require_on(Permission::Read, &Target::Catalog)?;
    let deployments = st.store.list::<DeploymentSpec>().await?;
    let creds: HashMap<String, ProviderCredential> = st
        .store
        .list_credentials()
        .await?
        .into_iter()
        .map(|c| (c.metadata.name.clone(), c))
        .collect();
    let runtime = st.routes.as_ref().map(|r| r.runtime_status());
    let mut by_endpoint: BTreeMap<(&'static str, Option<String>), ProviderEndpoint> =
        BTreeMap::new();
    for d in deployments {
        let spec = &d.spec;
        let cred = spec.credential.as_ref().and_then(|c| creds.get(c));
        let auth = auth_of(spec, cred);
        let (state, rt) = deployment_state(spec.enabled, runtime.as_ref(), d.id);
        let endpoint = spec
            .endpoint
            .as_deref()
            .map(|e| e.trim_end_matches('/').to_owned());
        let entry = by_endpoint
            .entry((spec.provider.as_str(), endpoint.clone()))
            .or_insert_with(|| ProviderEndpoint {
                provider: spec.provider,
                endpoint,
                auth: Vec::new(),
                state: HealthState::Unknown,
                model_groups: Vec::new(),
                deployments: Vec::new(),
            });
        entry.deployments.push(ProviderDeployment {
            name: d.metadata.name.clone(),
            model_group: spec.model_group.clone(),
            provider_model: spec.provider_model.clone(),
            enabled: spec.enabled,
            auth,
            state,
            breaker: rt.and_then(|r| r.breaker).map(|b| match b {
                BreakerState::Closed => Breaker::Closed,
                BreakerState::Open => Breaker::Open,
                BreakerState::HalfOpen => Breaker::HalfOpen,
            }),
            in_flight: rt.map_or(0, |r| r.in_flight),
            health_failures: rt.map_or(0, |r| r.health_failures),
            error: rt.and_then(|r| r.error.clone()),
        });
    }
    let items = by_endpoint
        .into_values()
        .map(|mut e| {
            e.deployments.sort_by(|a, b| a.name.cmp(&b.name));
            let auth: BTreeSet<AuthUse> = e.deployments.iter().map(|d| d.auth.clone()).collect();
            e.auth = auth.into_iter().collect();
            let groups: BTreeSet<String> = e
                .deployments
                .iter()
                .map(|d| d.model_group.clone())
                .collect();
            e.model_groups = groups.into_iter().collect();
            let states: Vec<HealthState> = e.deployments.iter().map(|d| d.state).collect();
            e.state = endpoint_state(&states);
            e
        })
        .collect();
    let mut res = Json(ProviderOverview {
        runtime: runtime.is_some(),
        items,
    })
    .into_response();
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(res)
}

/// Router (mit Anmeldung).
pub fn router(state: ProvidersState) -> Router {
    Router::new()
        .route(PROVIDERS_PATH, get(provider_overview))
        .route_layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state.auth),
            require_principal,
        ))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigw_core::resource::{EntraAuth, EntraCredentialKind, ObjectMeta, ProviderCredentialInfo};
    use serde_json::json;

    fn dep(
        provider: ProviderKind,
        credential: Option<&str>,
        settings: serde_json::Value,
    ) -> DeploymentSpec {
        serde_json::from_value(json!({
            "modelGroup": "g",
            "provider": provider.as_str(),
            "providerModel": "m",
            "credential": credential,
            "settings": settings,
        }))
        .unwrap()
    }

    fn cred(
        provider: ProviderKind,
        fields: &[&str],
        entra: Option<EntraAuth>,
    ) -> ProviderCredential {
        ProviderCredential {
            id: aigw_core::resource::new_id(),
            metadata: ObjectMeta::named("c"),
            spec: ProviderCredentialInfo {
                provider,
                entra,
                fields: fields.iter().map(|f| (*f).to_owned()).collect(),
                key_id: "k".into(),
            },
        }
    }

    #[test]
    fn auth_kinds() {
        // Inline-Einstellungen ohne Zugangsdaten (Live-Fall Foundry per UAMI).
        let a = auth_of(
            &dep(
                ProviderKind::Anthropic,
                None,
                json!({"auth": "entraId", "entraCredential": "managedIdentity", "entraClientId": "abc"}),
            ),
            None,
        );
        assert_eq!(a.kind, AuthKind::EntraId);
        assert_eq!(a.entra_credential.as_deref(), Some("managedIdentity"));
        assert_eq!(a.client_id.as_deref(), Some("abc"));
        assert_eq!(a.credential, None);
        // Entra aus den Zugangsdaten.
        let c = cred(
            ProviderKind::AzureOpenAi,
            &[],
            Some(EntraAuth {
                credential: EntraCredentialKind::ManagedIdentity,
                tenant_id: None,
                client_id: Some("uami".into()),
                authority: None,
                scope: None,
                send_x5c: None,
            }),
        );
        let a = auth_of(
            &dep(ProviderKind::AzureOpenAi, Some("c"), json!({})),
            Some(&c),
        );
        assert_eq!(a.kind, AuthKind::EntraId);
        assert_eq!(a.client_id.as_deref(), Some("uami"));
        assert_eq!(a.credential.as_deref(), Some("c"));
        // Service Principal ohne entraCredential: aus den Feldnamen.
        let c = cred(
            ProviderKind::AzureOpenAi,
            &["clientId", "clientSecret", "tenantId"],
            None,
        );
        let a = auth_of(
            &dep(
                ProviderKind::AzureOpenAi,
                Some("c"),
                json!({"auth": "entraId"}),
            ),
            Some(&c),
        );
        assert_eq!(a.entra_credential.as_deref(), Some("clientSecret"));
        assert_eq!(a.client_id, None, "Client-ID aus data bleibt verborgen");
        let c = cred(ProviderKind::OpenAi, &["apiKey"], None);
        assert_eq!(
            auth_of(&dep(ProviderKind::OpenAi, Some("c"), json!({})), Some(&c)).kind,
            AuthKind::ApiKey
        );
        let c = cred(
            ProviderKind::Bedrock,
            &["accessKeyId", "secretAccessKey"],
            None,
        );
        assert_eq!(
            auth_of(&dep(ProviderKind::Bedrock, Some("c"), json!({})), Some(&c)).kind,
            AuthKind::AwsAccessKey
        );
        assert_eq!(
            auth_of(&dep(ProviderKind::Ollama, None, json!({})), None).kind,
            AuthKind::None
        );
    }

    #[test]
    fn endpoint_states() {
        use HealthState as H;
        assert_eq!(endpoint_state(&[H::Ok, H::Ok]), H::Ok);
        assert_eq!(endpoint_state(&[H::Ok, H::Down]), H::Degraded);
        assert_eq!(endpoint_state(&[H::Unusable, H::Down]), H::Down);
        assert_eq!(endpoint_state(&[H::Disabled]), H::Disabled);
        assert_eq!(endpoint_state(&[H::Unknown, H::Disabled]), H::Unknown);
        assert_eq!(endpoint_state(&[H::Ok, H::Unknown]), H::Ok);
        assert_eq!(endpoint_state(&[H::Degraded, H::Ok]), H::Degraded);
    }
}
