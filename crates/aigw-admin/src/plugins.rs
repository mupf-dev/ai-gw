// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Plugin-Katalog und Testlauf für die Editoren der Admin-UI (AIGW-52).
//!
//! - `GET /admin/api/v1/plugins` – registrierte Plugins mit Standard-Fehlermodus (F148),
//!   Standard-Zeitlimit und ob ein Testlauf möglich ist. Lesen: jede Rolle mit Leserecht auf
//!   den Katalog.
//! - `POST /admin/api/v1/plugin-bindings:test` – führt eine (ungespeicherte)
//!   Plugin-Konfiguration auf einem Beispieltext aus und liefert die Vorschau: unverändert,
//!   verändert (z. B. maskierte PII) oder abgelehnt, dazu die Guardrail-Treffer (nur Zähler).
//!   **Nur lokale Plugins** (`pii`, `prompt-filter`, `link-guard`, `language`): Plugins mit
//!   ausgehenden Aufrufen (`webhook`, `content-safety`) werden nicht ausgeführt (`422`), damit
//!   der Testlauf keine externen Dienste anspricht. Nichts wird gespeichert (kein Audit-Eintrag); im Modus
//!   „nur YAML“ erlaubt.

use std::sync::Arc;

use aigw_core::ir::{ChatRequest, ChatResponse, Choice, Message, Role, StopReason};
use aigw_core::resource::JsonObject;
use aigw_core::wire::WireFormat;
use aigw_plugins::{CallInfo, Chain, Entry, Flow, Meta, ReqCtx, RespCtx, ResponseHeaders};
use aigw_proxy::PluginRegistry as Registry;
use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::auth::{Permission, Principal, Target};
use crate::problem::Problem;
use crate::resources::{ApiResult, St, json_with};

/// Pfad des Katalogs.
pub const CATALOG_PATH: &str = "/admin/api/v1/plugins";
/// Pfad des Testlaufs (im Modus „nur YAML“ erlaubt, schreibt nichts).
pub const TEST_PATH: &str = "/admin/api/v1/plugin-bindings:test";

/// Plugins, die der Testlauf ausführt (ohne ausgehende Aufrufe).
const TESTABLE: &[&str] = &["pii", "prompt-filter", "link-guard", "language"];
/// Höchstlänge des Beispieltexts (Bytes).
const MAX_TEXT: usize = 32 * 1024;

/// Ein registriertes Plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginInfo {
    /// Name, unter dem Bindungen das Plugin ansprechen.
    pub name: String,
    /// Standard-Fehlermodus der Plugin-Art (F148): `open` oder `closed`.
    #[schema(value_type = String, example = "closed")]
    pub default_fail_mode: String,
    /// Standard-Zeitlimit asynchroner Haken (Millisekunden).
    pub default_timeout_ms: u64,
    /// Testlauf (`POST /plugin-bindings:test`) möglich?
    pub testable: bool,
}

/// Antwort von `GET /plugins`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct PluginCatalog {
    /// Plugins (nach Name sortiert).
    pub items: Vec<PluginInfo>,
}

/// Phase des Testlaufs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PluginTestPhase {
    /// Eingabe: der Text ist eine Nutzernachricht der Anfrage.
    #[default]
    Request,
    /// Ausgabe: der Text ist die Antwort des Modells.
    Response,
}

/// Anfrage an `POST /plugin-bindings:test`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginTestRequest {
    /// Plugin (z. B. `pii`).
    pub plugin: String,
    /// Plugin-Konfiguration wie in `PluginBinding.spec.config`.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub config: JsonObject,
    /// Ausgabe-Ablehnung (`finish` | `error`, Standard `finish`).
    #[serde(default)]
    pub output_reject_mode: Option<String>,
    /// Phase (Standard `request`).
    #[serde(default)]
    pub phase: PluginTestPhase,
    /// Beispieltext (höchstens 32 KiB).
    pub text: String,
}

/// Ergebnis eines Testlaufs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PluginTestOutcome {
    /// Unverändert durchgelassen.
    Pass,
    /// Text verändert (Maskierung, Pseudonymisierung …).
    Modified,
    /// Ausgabe gefiltert (`outputRejectMode: finish` – 200 mit `content_filter`).
    Filtered,
    /// Abgelehnt (Fehler an den Client).
    Rejected,
}

/// Erkennung eines Guardrails (nur Zähler, nie Inhalte).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct PluginTestHit {
    /// Kategorie (z. B. `email`, `injection`).
    pub category: String,
    /// Ausgeführte Aktion (`mask`, `reject`, `flag` …).
    pub action: String,
    /// Anzahl.
    pub count: u32,
}

/// Antwort von `POST /plugin-bindings:test`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginTestResult {
    /// Ergebnis.
    pub outcome: PluginTestOutcome,
    /// Text nach dem Plugin (bei `pass`/`modified`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// HTTP-Status der Ablehnung (bei `rejected`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Fehlercode der Ablehnung (z. B. `pii_detected`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Meldung der Ablehnung.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Erkennungen.
    pub hits: Vec<PluginTestHit>,
}

/// Registrierte Plugins.
#[utoipa::path(
    get, path = "/admin/api/v1/plugins", tag = "plugin-bindings",
    responses(
        (status = 200, description = "Katalog", body = PluginCatalog),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub(crate) async fn plugin_catalog(Extension(p): Extension<Principal>) -> ApiResult {
    p.require_on(Permission::Read, &Target::Catalog)?;
    let reg = Registry::with_builtin();
    let items = reg
        .names()
        .filter_map(|n| {
            let f = reg.get(n)?;
            Some(PluginInfo {
                name: n.to_owned(),
                default_fail_mode: f.default_fail_mode().as_str().to_owned(),
                default_timeout_ms: u64::try_from(f.default_timeout().as_millis())
                    .unwrap_or(u64::MAX),
                testable: TESTABLE.contains(&n),
            })
        })
        .collect();
    Ok(json_with(StatusCode::OK, None, &PluginCatalog { items }))
}

/// Testlauf einer Plugin-Konfiguration auf einem Beispieltext (nur lokale Plugins).
#[utoipa::path(
    post, path = "/admin/api/v1/plugin-bindings:test", tag = "plugin-bindings",
    request_body = PluginTestRequest,
    responses(
        (status = 200, description = "Ergebnis", body = PluginTestResult),
        (status = 400, description = "Ungültige Anfrage", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Nicht angemeldet", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Keine Berechtigung", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Ungültige Konfiguration oder Plugin nicht testbar", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub(crate) async fn test_plugin(
    State(st): St,
    Extension(p): Extension<Principal>,
    body: Result<Json<PluginTestRequest>, JsonRejection>,
) -> ApiResult {
    p.require_on(Permission::Read, &Target::Catalog)?;
    let Json(req) = body?;
    if !TESTABLE.contains(&req.plugin.as_str()) {
        return Err(Problem::invalid(format!(
            "plugin: Testlauf nur für lokale Plugins ({}) – {:?} ruft externe Dienste auf \
             bzw. hat keine Textwirkung",
            TESTABLE.join(", "),
            req.plugin
        )));
    }
    if req.text.len() > MAX_TEXT {
        return Err(Problem::invalid(format!(
            "text: höchstens {MAX_TEXT} Bytes"
        )));
    }
    let mut config = req.config.clone();
    if let Some(m) = &req.output_reject_mode {
        config.insert("outputRejectMode".into(), m.clone().into());
    }
    let result = run(&st.upstreams, &req.plugin, &config, req.phase, &req.text)
        .await
        .map_err(|e| Problem::invalid(format!("config: Plugin-Bindung ({}): {e}", req.plugin)))?;
    Ok(json_with(StatusCode::OK, None, &result))
}

/// Baut das Plugin wie die Kette (Steuerung + Konfiguration) und führt eine Phase aus.
async fn run(
    policy: &aigw_core::upstream::UpstreamPolicy,
    plugin: &str,
    config: &JsonObject,
    phase: PluginTestPhase,
    text: &str,
) -> Result<PluginTestResult, String> {
    let reg = Registry::for_validation(policy);
    let control = reg.effective(plugin, config)?;
    let factory = reg
        .get(plugin)
        .ok_or_else(|| format!("unbekanntes Plugin {plugin:?}"))?;
    let split = aigw_plugins::binding::split(config);
    let instance = factory.build(&split.plugin_config)?;
    let entry = Entry::new("test", instance)
        .with_fail_mode(control.fail_mode)
        .with_timeout(control.timeout)
        .with_output_reject(control.output_reject);
    let chain = Chain::new(vec![Arc::new(entry)]);
    let info = CallInfo::new("plugin-test", WireFormat::OpenAiChat);
    let headers = HeaderMap::new();
    let mut response_headers = ResponseHeaders::default();
    let mut meta = Meta::new();
    let (flow, out) = match phase {
        PluginTestPhase::Request => {
            let mut ctx = ReqCtx {
                info: &info,
                request: ChatRequest {
                    model: "test".into(),
                    messages: vec![Message::text(Role::User, text)],
                    ..ChatRequest::default()
                },
                client_headers: &headers,
                upstream_headers: HeaderMap::new(),
                response_headers: ResponseHeaders::default(),
                meta: Meta::new(),
            };
            let flow = chain.run_request(&mut ctx).await;
            let out = ctx
                .request
                .messages
                .iter()
                .map(|m| m.content.text())
                .collect::<Vec<_>>()
                .join("\n");
            (flow, out)
        }
        PluginTestPhase::Response => {
            let mut ctx = RespCtx {
                info: &info,
                response: response(text),
                response_headers: &mut response_headers,
                meta: &mut meta,
            };
            let flow = chain.run_response(&mut ctx).await;
            let filtered = ctx
                .response
                .choices
                .iter()
                .any(|c| c.stop_reason == Some(StopReason::ContentFilter));
            let out = ctx
                .response
                .choices
                .iter()
                .map(|c| c.message.content.text())
                .collect::<Vec<_>>()
                .join("\n");
            if filtered && matches!(flow, Flow::Continue) {
                return Ok(PluginTestResult {
                    outcome: PluginTestOutcome::Filtered,
                    text: Some(out),
                    status: Some(200),
                    code: Some("content_filter".into()),
                    message: None,
                    hits: hits(&info),
                });
            }
            (flow, out)
        }
    };
    let hits = hits(&info);
    Ok(match flow {
        Flow::Continue | Flow::Respond(_) => PluginTestResult {
            outcome: if out == text {
                PluginTestOutcome::Pass
            } else {
                PluginTestOutcome::Modified
            },
            text: Some(out),
            status: None,
            code: None,
            message: None,
            hits,
        },
        Flow::Reject(e) | Flow::Block(e) => PluginTestResult {
            outcome: PluginTestOutcome::Rejected,
            text: None,
            status: Some(e.status()),
            code: e.code.clone(),
            message: Some(e.message.clone()),
            hits,
        },
    })
}

fn response(text: &str) -> ChatResponse {
    ChatResponse {
        id: "plugin-test".into(),
        model: "test".into(),
        created: None,
        choices: vec![Choice {
            index: 0,
            message: Message::text(Role::Assistant, text),
            stop_reason: Some(StopReason::EndTurn),
            stop_sequence: None,
            extra: Default::default(),
        }],
        usage: None,
        extra: Default::default(),
        source: None,
    }
}

fn hits(info: &CallInfo) -> Vec<PluginTestHit> {
    info.guardrails
        .snapshot()
        .into_iter()
        .map(|h| PluginTestHit {
            category: h.category,
            action: h.action.to_owned(),
            count: h.count,
        })
        .collect()
}

/// Routen (ohne Authentifizierungsschicht).
pub(crate) fn router() -> axum::Router<crate::AdminState> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route(CATALOG_PATH, get(plugin_catalog))
        .route(TEST_PATH, post(test_plugin))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use aigw_core::upstream::UpstreamPolicy;
    use serde_json::json;

    fn obj(v: serde_json::Value) -> JsonObject {
        match v {
            serde_json::Value::Object(m) => m,
            _ => JsonObject::new(),
        }
    }

    #[tokio::test]
    async fn pii_masks_and_rejects() {
        let p = UpstreamPolicy::strict();
        let r = run(
            &p,
            "pii",
            &obj(json!({"action": "mask"})),
            PluginTestPhase::Request,
            "Mail an max@example.org bitte",
        )
        .await
        .unwrap();
        assert_eq!(r.outcome, PluginTestOutcome::Modified, "{r:?}");
        assert!(
            !r.text.as_deref().unwrap().contains("max@example.org"),
            "{r:?}"
        );
        assert!(r.hits.iter().any(|h| h.category == "email"), "{r:?}");

        let r = run(
            &p,
            "pii",
            &obj(json!({"action": "reject"})),
            PluginTestPhase::Request,
            "max@example.org",
        )
        .await
        .unwrap();
        assert_eq!(r.outcome, PluginTestOutcome::Rejected, "{r:?}");
        assert_eq!(r.status, Some(400));

        let r = run(
            &p,
            "pii",
            &obj(json!({})),
            PluginTestPhase::Request,
            "Hallo Welt",
        )
        .await
        .unwrap();
        assert_eq!(r.outcome, PluginTestOutcome::Pass, "{r:?}");
        assert!(r.hits.is_empty());
    }

    #[tokio::test]
    async fn output_block_is_filtered_or_rejected() {
        let p = UpstreamPolicy::strict();
        let cfg = json!({"action": "reject", "direction": "output"});
        let r = run(
            &p,
            "pii",
            &obj(cfg.clone()),
            PluginTestPhase::Response,
            "max@example.org",
        )
        .await
        .unwrap();
        assert_eq!(r.outcome, PluginTestOutcome::Filtered, "{r:?}");
        let mut c = obj(cfg);
        c.insert("outputRejectMode".into(), "error".into());
        let r = run(&p, "pii", &c, PluginTestPhase::Response, "max@example.org")
            .await
            .unwrap();
        assert_eq!(r.outcome, PluginTestOutcome::Rejected, "{r:?}");
    }

    #[tokio::test]
    async fn link_guard_and_language_are_testable() {
        let p = UpstreamPolicy::strict();
        let r = run(
            &p,
            "link-guard",
            &obj(json!({"allowDomains": ["example.com"]})),
            PluginTestPhase::Response,
            "Bild ![x](https://evil.test/p.png?d=1) und ![y](https://example.com/y.png)",
        )
        .await
        .unwrap();
        assert_eq!(r.outcome, PluginTestOutcome::Modified, "{r:?}");
        assert_eq!(
            r.text.as_deref(),
            Some("Bild  und ![y](https://example.com/y.png)")
        );
        assert_eq!(r.hits.len(), 1);
        let r = run(
            &p,
            "language",
            &obj(json!({"allow": ["de"]})),
            PluginTestPhase::Request,
            "Please summarize the following report about the quarterly figures briefly.",
        )
        .await
        .unwrap();
        assert_eq!(r.outcome, PluginTestOutcome::Rejected, "{r:?}");
        assert_eq!(r.code.as_deref(), Some("language_not_allowed"));
    }

    #[tokio::test]
    async fn invalid_config_is_an_error() {
        let p = UpstreamPolicy::strict();
        assert!(
            run(
                &p,
                "pii",
                &obj(json!({"acton": "mask"})),
                PluginTestPhase::Request,
                "x"
            )
            .await
            .is_err()
        );
        assert!(
            run(
                &p,
                "prompt-filter",
                &obj(json!({"regex": [{"name": "r", "pattern": "("}]})),
                PluginTestPhase::Request,
                "x"
            )
            .await
            .is_err()
        );
    }
}
