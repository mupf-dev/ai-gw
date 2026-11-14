// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Preisvorschläge (AIGW-85).
//!
//! - `GET /admin/api/v1/price-suggestions?modelGroup=&deployment=&refresh=` – je Deployment
//!   Vorschlag (Azure Retail Prices bzw. Preiskatalog) neben dem aktuell gültigen Preis,
//!   Status `missing|unchanged|differs|conflict|no-suggestion`, Zustand der Quellen.
//! - `POST /admin/api/v1/price-suggestions:accept` – übernimmt ausgewählte Vorschläge als
//!   **neue Preisversionen** (`validFrom` = jetzt bzw. angegeben, nie rückwirkend). Jede
//!   Version wird wie über `POST /prices` angelegt (Audit-Eintrag je Preis) und trägt die
//!   Labels `aigw.io/price-source`, `…-region`, `…-deployment-type`, `…-deployment`.
//!
//! **Manuelle Preise haben Vorrang:** weicht ein Vorschlag von einem manuell gepflegten Preis
//! (ohne Label `aigw.io/price-source`) ab, wird er nur mit `overrideManual: true` übernommen.
//! Die Server-Seite berechnet die Vorschläge bei der Übernahme neu (aus dem Zwischenspeicher)
//! – Preise aus dem Request-Body gibt es nicht.

// `#[utoipa::path(params(…))]` erzeugt für IntoParams-Typen qualifizierte Pfade.
#![allow(unused_qualifications)]

use std::collections::BTreeMap;

use aigw_billing::suggest::{
    LABEL_DEPLOYMENT, LABEL_DEPLOYMENT_TYPE, LABEL_REGION, LABEL_SOURCE, PriceSuggestionReport,
    PriceSuggestionStatus, differences,
};
use aigw_core::resource::{DeploymentSpec, ObjectMeta, PriceSpec};
use aigw_core::validate;
use aigw_store::WriteExtras;
use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use utoipa::{IntoParams, ToSchema};

use crate::auth::{Permission, Principal, Target};
use crate::prices::{Price, price_name};
use crate::problem::Problem;
use crate::resources::{ApiResult, St, json_with};

/// Pfad der Vorschläge.
pub const PATH: &str = "/admin/api/v1/price-suggestions";
/// Pfad der Übernahme.
pub const ACCEPT_PATH: &str = "/admin/api/v1/price-suggestions:accept";

/// Filter der Vorschläge.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query, rename_all = "camelCase")]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SuggestionQuery {
    /// Nur Deployments dieser Modellgruppe.
    pub model_group: Option<String>,
    /// Nur dieses Deployment.
    pub deployment: Option<String>,
    /// Quellen neu abrufen (Azure Retail Prices, Katalog-URL) statt Zwischenspeicher.
    #[serde(default)]
    pub refresh: bool,
}

/// Auswahl eines Vorschlags.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PriceSuggestionAcceptItem {
    /// Deployment, dessen Vorschlag übernommen wird.
    pub deployment: String,
    /// Einen abweichenden **manuellen** Preis ersetzen (neue Version).
    #[serde(default)]
    pub override_manual: bool,
}

/// Übernahme von Vorschlägen.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PriceSuggestionAccept {
    /// Ausgewählte Deployments (1–500).
    pub items: Vec<PriceSuggestionAcceptItem>,
    /// Gültig ab (RFC 3339, nicht in der Vergangenheit); Standard: jetzt.
    #[serde(default, with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub valid_from: Option<OffsetDateTime>,
}

/// Nicht angelegter Vorschlag.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PriceSuggestionSkipped {
    /// Deployment.
    pub deployment: String,
    /// Grund.
    pub reason: String,
}

/// Ergebnis der Übernahme.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PriceSuggestionAcceptResult {
    /// Angelegte Preisversionen.
    pub created: Vec<Price>,
    /// Übersprungen (Preis bereits aktuell, gleicher Preisschlüssel doppelt gewählt).
    pub skipped: Vec<PriceSuggestionSkipped>,
}

fn suggester(st: &crate::AdminState) -> Result<&aigw_billing::suggest::Suggester, Problem> {
    st.price_suggester.as_deref().ok_or_else(|| {
        Problem::not_found("Preisvorschläge sind ausgeschaltet (billing.price_suggestions.enabled)")
    })
}

async fn build_report(
    st: &crate::AdminState,
    refresh: bool,
    now: OffsetDateTime,
) -> Result<PriceSuggestionReport, Problem> {
    let s = suggester(st)?;
    let deployments = st.store.list::<DeploymentSpec>().await?;
    let prices = st.store.list::<PriceSpec>().await?;
    Ok(s.report(&deployments, &prices, now, refresh).await)
}

/// Preisvorschläge je Deployment.
#[utoipa::path(
    get, path = "/admin/api/v1/price-suggestions", tag = "prices",
    params(SuggestionQuery),
    responses(
        (status = 200, description = "Vorschläge", body = aigw_billing::suggest::PriceSuggestionReport),
        (status = 400, description = "Ungültige Anfrage", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Preisvorschläge ausgeschaltet", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub(crate) async fn list_suggestions(
    State(st): St,
    Extension(p): Extension<Principal>,
    q: Result<Query<SuggestionQuery>, QueryRejection>,
) -> ApiResult {
    p.require_on(Permission::Read, &Target::Catalog)?;
    let Query(q) = q?;
    let mut report = build_report(&st, q.refresh, OffsetDateTime::now_utc()).await?;
    report.items.retain(|i| {
        q.model_group.as_ref().is_none_or(|g| *g == i.model_group)
            && q.deployment.as_ref().is_none_or(|d| *d == i.deployment)
    });
    Ok(json_with(StatusCode::OK, None, &report))
}

/// Übernimmt Vorschläge als neue Preisversionen.
#[utoipa::path(
    post, path = "/admin/api/v1/price-suggestions:accept", tag = "prices",
    request_body = PriceSuggestionAccept,
    responses(
        (status = 200, description = "Angelegte und übersprungene Vorschläge", body = PriceSuggestionAcceptResult),
        (status = 400, description = "Ungültige Anfrage", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Preisvorschläge ausgeschaltet", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Manueller Preis ohne overrideManual, widersprüchliche Auswahl oder neuere Version vorhanden", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Unbekanntes Deployment, kein Vorschlag oder validFrom in der Vergangenheit", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub(crate) async fn accept_suggestions(
    State(st): St,
    Extension(p): Extension<Principal>,
    body: Result<Json<PriceSuggestionAccept>, JsonRejection>,
) -> ApiResult {
    p.require_on(Permission::Write, &Target::Catalog)?;
    let Json(req) = body?;
    if req.items.is_empty() || req.items.len() > 500 {
        return Err(Problem::invalid("items: 1–500 Deployments"));
    }
    let now = OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .unwrap_or(OffsetDateTime::now_utc());
    let valid_from = req.valid_from.unwrap_or(now);
    if valid_from < now - time::Duration::minutes(1) {
        return Err(Problem::invalid(
            "validFrom: nicht in der Vergangenheit (bereits gültige Preise sind unveränderlich)",
        ));
    }
    // Vorschläge vor der Schreibsperre (Abruf aus dem Netz möglich).
    let report = build_report(&st, false, now).await?;
    let _guard = st.write_lock.lock().await;
    let by_name: BTreeMap<&str, _> = report
        .items
        .iter()
        .map(|i| (i.deployment.as_str(), i))
        .collect();

    // Erst alles prüfen, dann schreiben.
    let mut invalid = Vec::new();
    let mut conflicts = Vec::new();
    let mut skipped = Vec::new();
    let mut planned: BTreeMap<(&'static str, String), (String, PriceSpec, ObjectMeta)> =
        BTreeMap::new();
    for item in &req.items {
        let Some(s) = by_name.get(item.deployment.as_str()) else {
            invalid.push(format!("{}: unbekanntes Deployment", item.deployment));
            continue;
        };
        let Some(suggested) = &s.suggested else {
            invalid.push(format!("{}: kein Vorschlag", item.deployment));
            continue;
        };
        if s.status == PriceSuggestionStatus::Unchanged {
            skipped.push(PriceSuggestionSkipped {
                deployment: item.deployment.clone(),
                reason: "Preis bereits aktuell".into(),
            });
            continue;
        }
        if let Some(c) = &s.current {
            if c.manual && !item.override_manual {
                conflicts.push(format!(
                    "{}: manueller Preis {} hat Vorrang – overrideManual setzen, um ihn zu ersetzen",
                    item.deployment, c.name
                ));
                continue;
            }
            if c.spec.valid_from >= valid_from {
                conflicts.push(format!(
                    "{}: Preisversion {} gilt ab {} – validFrom muss später liegen",
                    item.deployment, c.name, c.spec.valid_from
                ));
                continue;
            }
        }
        let mut spec = suggested.clone();
        spec.valid_from = valid_from;
        if let Err(e) = validate::price(&spec) {
            invalid.push(format!("{}: {e}", item.deployment));
            continue;
        }
        let key = (s.provider.as_str(), s.provider_model.clone());
        if let Some((other, prev, _)) = planned.get(&key) {
            if differences(prev, &spec).is_empty() {
                skipped.push(PriceSuggestionSkipped {
                    deployment: item.deployment.clone(),
                    reason: format!("gleicher Preis wie {other} (gleicher Preisschlüssel)"),
                });
            } else {
                conflicts.push(format!(
                    "{} und {other}: gleicher Preisschlüssel {}/{} mit abweichenden Vorschlägen – nur einen wählen",
                    item.deployment,
                    s.provider.as_str(),
                    s.provider_model
                ));
            }
            continue;
        }
        let mut meta = ObjectMeta::named(price_name(&spec));
        if let Some(src) = s.source {
            meta.labels.insert(LABEL_SOURCE.into(), src.as_str().into());
        }
        if let Some(r) = &s.region {
            meta.labels.insert(LABEL_REGION.into(), r.clone());
        }
        if let Some(t) = s.deployment_type {
            meta.labels
                .insert(LABEL_DEPLOYMENT_TYPE.into(), t.as_str().into());
        }
        meta.labels
            .insert(LABEL_DEPLOYMENT.into(), item.deployment.clone());
        planned.insert(key, (item.deployment.clone(), spec, meta));
    }
    if !invalid.is_empty() {
        return Err(Problem::invalid(invalid.join("; ")));
    }
    if !conflicts.is_empty() {
        return Err(Problem::conflict(conflicts.join("; ")));
    }
    let mut created = Vec::with_capacity(planned.len());
    for (_, (_, spec, meta)) in planned {
        let obj = st
            .store
            .create_with(p.actor(), meta, spec, &WriteExtras::default())
            .await?;
        created.push(Price::from(obj));
    }
    Ok(json_with(
        StatusCode::OK,
        None,
        &PriceSuggestionAcceptResult { created, skipped },
    ))
}

/// Routen der Preisvorschläge (ohne Authentifizierungsschicht).
pub(crate) fn router() -> axum::Router<crate::AdminState> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route(PATH, get(list_suggestions))
        .route(ACCEPT_PATH, post(accept_suggestions))
}

/// Startet den periodischen Abgleich (`billing.price_suggestions.check_interval_secs`):
/// Warnung im Log bei neu abweichenden oder fehlenden Preisen, Ergebnis in
/// `GET /price-suggestions` (`lastCheck`).
pub fn spawn_check(
    store: aigw_store::Store,
    suggester: std::sync::Arc<aigw_billing::suggest::Suggester>,
    interval: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let deps = store.list::<DeploymentSpec>().await;
            let prices = store.list::<PriceSpec>().await;
            match (deps, prices) {
                (Ok(d), Ok(p)) => {
                    let s = suggester.check(&d, &p).await;
                    tracing::info!(
                        differing = s.differing.len(),
                        missing = s.missing.len(),
                        "Preisabgleich abgeschlossen"
                    );
                }
                (Err(e), _) | (_, Err(e)) => {
                    tracing::warn!(error = %e, "Preisabgleich: Datenbank nicht lesbar");
                }
            }
        }
    })
}
