// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Playground der Admin-UI (AIGW-54).
//!
//! - `GET /admin/api/v1/playground/models?key=` – Modellgruppen, die der Key nutzen darf, mit
//!   `exposeReasoning` (Gruppe und Key/Team/Organisation) für die Anzeige.
//! - `POST /admin/api/v1/playground/chat?key=` – Körper wie `POST /v1/chat/completions`
//!   (OpenAI-Format, auch `stream: true` → SSE); Antwort wie auf der Datenebene mit
//!   `x-aigw-request-id`, `x-aigw-deployment`, `x-aigw-model`, `x-aigw-cache` …
//!
//! **Kein Umgehen der Datenebene:** Die Admin-Anmeldung allein erlaubt keinen Modellzugriff.
//! Jede Anfrage läuft im Namen eines bestehenden, aktiven virtuellen Keys (`key`) und durch
//! dieselbe Pipeline wie `/v1/chat/completions` ([`aigw_proxy::playground`]): Modellfreigaben,
//! Limits, Budgets, Guardrails/Plugins, Cache, Routing, Kosten und Request-Log gelten für
//! diesen Key. Den Klartext des Keys braucht die UI nicht.
//!
//! **Berechtigung:** Schreibrecht auf den Key (globaler `admin`, `org-admin` der Organisation,
//! `team-admin` des Teams) – wer den Key verwalten (also auch rotieren) darf, darf ihn auch im
//! Playground nutzen. Ohne Leserecht auf den Key antwortet die API `404` (wie für unbekannte
//! oder inaktive Keys). Im Modus „nur YAML“ bleibt der Playground nutzbar (keine
//! Konfigurationsänderung).
//!
//! **Audit:** Jede Chat-Anfrage schreibt vorab einen Eintrag (Art `playground`, Aktion `chat`,
//! Objekt = Key, Geltungsbereich Organisation/Team des Keys) mit Modell, Stream-Flag und
//! Request-ID – schlägt das Schreiben fehl, wird die Anfrage nicht ausgeführt.

#![allow(unused_qualifications)]

use std::fmt;
use std::sync::Arc;

use aigw_core::snapshot::SnapshotHandle;
use aigw_proxy::auth::AuthContext;
use aigw_proxy::routing::GroupPolicy;
use aigw_proxy::{ProxyState, RequestId};
use aigw_store::{AuditEvent, Store};
use axum::extract::rejection::{BytesRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, Extension, Query, State};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use utoipa::{IntoParams, ToSchema};

use crate::auth::{AdminAuthenticator, Permission, Principal, Target, require_principal};
use crate::problem::Problem;

/// Pfad der Modellliste.
pub const MODELS_PATH: &str = "/admin/api/v1/playground/models";
/// Pfad des Chats.
pub const CHAT_PATH: &str = "/admin/api/v1/playground/chat";
/// Art im Audit-Log.
pub const AUDIT_KIND: &str = "playground";

/// Zustand der Playground-Routen.
#[derive(Clone)]
pub struct PlaygroundState {
    /// Datenbank (Audit-Log).
    pub store: Store,
    /// Anmeldung (dieselbe wie für die übrige Admin-API).
    pub auth: Arc<dyn AdminAuthenticator>,
    /// Aktueller Konfigurations-Snapshot (Keys, Modellgruppen).
    pub snapshot: SnapshotHandle,
    /// Datenebene (Pipeline), siehe [`aigw_proxy::playground::state_for`].
    pub proxy: Arc<ProxyState>,
}

impl fmt::Debug for PlaygroundState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlaygroundState")
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

/// Parameter: Key, in dessen Namen die Anfrage läuft.
#[derive(Debug, Deserialize, IntoParams)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct KeyQuery {
    /// Name eines aktiven virtuellen Keys.
    key: String,
}

/// Eine für den Key freigegebene Modellgruppe.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlaygroundModel {
    /// Kanonischer Name.
    pub name: String,
    /// Aliasse.
    pub aliases: Vec<String>,
    /// Reasoning wird ausgegeben (`settings.exposeReasoning` der Gruppe und Key/Team/Org).
    pub expose_reasoning: bool,
    /// Zahl der aktiven Deployments.
    pub deployments: usize,
    /// Fähigkeiten der Gruppe (leer = nicht eingeschränkt).
    pub capabilities: Vec<String>,
}

/// Kontext des Playgrounds für einen Key.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlaygroundModels {
    /// Key.
    pub key: String,
    /// Organisation des Keys.
    pub organization: String,
    /// Team des Keys.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    /// Konfigurationsversion des Snapshots.
    pub config_version: i64,
    /// Freigegebene Modellgruppen (nach Name).
    pub models: Vec<PlaygroundModel>,
}

/// Key-Kontext samt Berechtigungsprüfung (Schreibrecht auf den Key).
fn key_context(st: &PlaygroundState, p: &Principal, name: &str) -> Result<AuthContext, Problem> {
    let not_found = || {
        Problem::not_found(format!(
            "Kein aktiver API-Key {name:?} (unbekannt, gesperrt, abgelaufen oder nicht sichtbar)"
        ))
    };
    let auth =
        aigw_proxy::playground::key_context(st.snapshot.load(), name, OffsetDateTime::now_utc())
            .ok_or_else(not_found)?;
    let key = auth.key.as_ref().ok_or_else(not_found)?;
    let spec = &key.key.spec;
    let target = match &spec.team {
        Some(t) => Target::in_team(spec.organization.clone(), t.clone()),
        None => Target::InOrganization(spec.organization.clone()),
    };
    if !p.can(Permission::Read, &target) {
        return Err(not_found());
    }
    p.require_on(Permission::Write, &target).map_err(|_| {
        Problem::forbidden(
            "Playground braucht Schreibrecht auf den Key (admin, org-admin bzw. team-admin)",
        )
    })?;
    Ok(auth)
}

/// Modellgruppen, die der Key nutzen darf.
#[utoipa::path(
    get, path = "/admin/api/v1/playground/models", tag = "playground", params(KeyQuery),
    responses(
        (status = 200, description = "Freigegebene Modellgruppen", body = PlaygroundModels),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Key unbekannt, inaktiv oder nicht sichtbar", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn models(
    State(st): State<PlaygroundState>,
    Extension(p): Extension<Principal>,
    q: Result<Query<KeyQuery>, QueryRejection>,
) -> Result<Json<PlaygroundModels>, Problem> {
    let Query(q) = q?;
    let auth = key_context(&st, &p, &q.key)?;
    let hides = auth.hides_reasoning();
    let (Some(snap), Some(key)) = (&auth.snapshot, &auth.key) else {
        return Err(Problem::internal());
    };
    let mut models: Vec<PlaygroundModel> = snap
        .model_groups
        .iter()
        .filter(|(name, g)| **name == g.group.metadata.name && key.allows_model(g))
        .map(|(name, g)| {
            let expose = GroupPolicy::from_settings(&g.group.spec.settings)
                .map(|p| p.expose_reasoning)
                .unwrap_or(true);
            PlaygroundModel {
                name: name.clone(),
                aliases: g.group.spec.aliases.clone(),
                expose_reasoning: expose && !hides,
                deployments: g.deployments.len(),
                capabilities: serde_json::to_value(&g.group.spec.capabilities)
                    .ok()
                    .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok())
                    .unwrap_or_default(),
            }
        })
        .collect();
    models.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(PlaygroundModels {
        key: key.key.metadata.name.clone(),
        organization: key.key.spec.organization.clone(),
        team: key.key.spec.team.clone(),
        config_version: snap.version,
        models,
    }))
}

/// Was aus dem Körper ins Audit-Log geht (keine Inhalte).
#[derive(Deserialize)]
struct AuditPeek {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    stream: Option<bool>,
}

/// Chat im Namen des Keys (Pipeline der Datenebene, OpenAI-Format).
#[utoipa::path(
    post, path = "/admin/api/v1/playground/chat", tag = "playground", params(KeyQuery),
    request_body(content = Object, content_type = "application/json", description = "Chat-Request im OpenAI-Format (wie `POST /v1/chat/completions`)"),
    responses(
        (status = 200, description = "Antwort im OpenAI-Format (JSON) bzw. SSE-Strom bei `stream: true`; Header `x-aigw-request-id`, `x-aigw-deployment`, `x-aigw-model`, `x-aigw-cache`", body = Object),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Key unbekannt, inaktiv oder nicht sichtbar", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Fehler der Datenebene (Format wie `/v1/chat/completions`: `{error: …}`), z. B. Modell nicht freigegeben, Limit, Budget, Guardrail", body = Object),
    ),
    security(("bearer" = []))
)]
pub(crate) async fn chat(
    State(st): State<PlaygroundState>,
    Extension(p): Extension<Principal>,
    q: Result<Query<KeyQuery>, QueryRejection>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Problem> {
    let Query(q) = q?;
    let auth = key_context(&st, &p, &q.key)?;
    let body = body.map_err(|r| Problem::bad_request(r.body_text()))?;
    let peek: AuditPeek = serde_json::from_slice(&body)
        .map_err(|e| Problem::bad_request(format!("Körper ist kein gültiges JSON: {e}")))?;
    let rid = RequestId::new();
    let (key_id, org, team) = auth.key.as_ref().map_or((None, None, None), |k| {
        (
            Some(k.key.id),
            Some(k.key.spec.organization.clone()),
            k.key.spec.team.clone(),
        )
    });
    // Vorab auditieren: ohne Eintrag keine Anfrage.
    st.store
        .record_audit_event_scoped(
            &AuditEvent {
                actor: p.actor().to_owned(),
                action: "chat".into(),
                kind: AUDIT_KIND.into(),
                object_id: key_id,
                object_name: Some(q.key.clone()),
                details: Some(serde_json::json!({
                    "requestId": rid.0,
                    "model": peek.model,
                    "stream": peek.stream.unwrap_or(false),
                })),
            },
            org.as_deref(),
            team.as_deref(),
        )
        .await?;
    Ok(aigw_proxy::playground::chat_completions(&st.proxy, rid, auth, body).await)
}

/// Router der Playground-Routen (mit Anmeldung; Körpergrenze wie auf der Datenebene).
pub fn router(state: PlaygroundState) -> Router {
    let limit = state.proxy.config.max_body_bytes;
    Router::new()
        .route(MODELS_PATH, get(models))
        .route(CHAT_PATH, post(chat).layer(DefaultBodyLimit::max(limit)))
        .route_layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state.auth),
            require_principal,
        ))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_peek_ignores_content() {
        let p: AuditPeek = serde_json::from_slice(
            br#"{"model":"m","stream":true,"messages":[{"role":"user","content":"x"}]}"#,
        )
        .unwrap();
        assert_eq!(p.model.as_deref(), Some("m"));
        assert_eq!(p.stream, Some(true));
    }
}
