// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! [`PluginHost`] des Gateways (AIGW-38): Zugangsdaten aus dem aktuellen Snapshot,
//! HTTP-Aufrufe über den gemeinsamen Outbound-Baustein (AIGW-72: SSRF-Regeln auch nach der
//! Namensauflösung, keine Weiterleitungen, Größen- und Zeitlimit) und Modellaufrufe über das
//! eigene Routing – ohne Plugin-Kette (keine Rekursion) und ohne Limits des Clients. Deren
//! Verbrauch wird in [`PluginCalls`] gebucht und am Ende der Anfrage dem Key/Team zugerechnet
//! (AIGW-89/S-13). Entra-ID-Tokens für Plugins (`content-safety` mit `auth: entraId`) kommen
//! aus derselben Token-Quelle wie bei Deployments ([`aigw_providers::entra`]).

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use aigw_core::ir::{ChatRequest, ChatResponse};
use aigw_core::observe::{PluginCall, PluginCalls};
use aigw_core::resource::{
    CredentialData, Deployment, DeploymentSpec, JsonObject, ObjectMeta, SecretString,
};
use aigw_core::snapshot::DeploymentCtx;
use aigw_core::snapshot::SnapshotHandle;
use aigw_core::upstream::UpstreamPolicy;
use aigw_counters::CounterBackend;
use aigw_outbound::{AddressGuard, FetchLimits, OutboundClient, OutboundOptions};
use aigw_plugins::{BoxFuture, HttpCall, HttpReply, PluginError, PluginHost};
use aigw_providers::UpstreamClient;
use aigw_providers::entra::{EntraEnv, EntraTokenSource};
use bytes::Bytes;

use crate::routes::ModelRoutes;
use crate::routing::exec::{self, CapacityCtx, Outcome};

/// Höchstgröße einer Antwort externer Prüfdienste.
const MAX_REPLY_BYTES: usize = 256 * 1024;

struct Inner {
    routes: Arc<dyn ModelRoutes>,
    counters: Arc<dyn CounterBackend>,
    snapshot: Option<SnapshotHandle>,
    policy: UpstreamPolicy,
    client: OutboundClient,
    /// Prozessumgebung für Entra ID (`None` = aus dem Prozess; Tests setzen sie).
    entra_env: Option<EntraEnv>,
    /// Token-Quellen je `ProviderCredential` (Name → Snapshot-Version, Quelle).
    entra: Mutex<HashMap<String, (i64, Arc<EntraTokenSource>)>>,
    /// Variante nur für öffentliche Ziele (S-22), einmal gebaut.
    public: OnceLock<Arc<dyn PluginHost>>,
}

/// Dienste des Gateways für Plugins (günstig klonbar).
#[derive(Clone)]
pub struct GatewayHost(Arc<Inner>);

impl fmt::Debug for GatewayHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GatewayHost")
            .field("policy", &self.0.policy)
            .finish_non_exhaustive()
    }
}

impl GatewayHost {
    /// Host aus Routing, Zähler-Backend (Deployment-Kapazität), Snapshot (Zugangsdaten),
    /// SSRF-Regeln (Prüfung der URL) und HTTP-Client (Prüfung nach der Namensauflösung; sollte
    /// dieselben Regeln tragen).
    pub fn new(
        routes: Arc<dyn ModelRoutes>,
        counters: Arc<dyn CounterBackend>,
        snapshot: Option<SnapshotHandle>,
        policy: UpstreamPolicy,
        client: OutboundClient,
    ) -> Self {
        Self(Arc::new(Inner {
            routes,
            counters,
            snapshot,
            policy,
            client,
            entra_env: None,
            entra: Mutex::new(HashMap::new()),
            public: OnceLock::new(),
        }))
    }

    /// Wie [`GatewayHost::new`], Entra ID aber mit vorgegebener Umgebung (Tests: IMDS-Endpunkt,
    /// föderiertes Token).
    pub fn with_entra_env(self, env: EntraEnv) -> Self {
        let i = &self.0;
        Self(Arc::new(Inner {
            routes: Arc::clone(&i.routes),
            counters: Arc::clone(&i.counters),
            snapshot: i.snapshot.clone(),
            policy: i.policy.clone(),
            client: i.client.clone(),
            entra_env: Some(env),
            entra: Mutex::new(HashMap::new()),
            public: OnceLock::new(),
        }))
    }
}

impl Inner {
    /// Token-Quelle für die `ProviderCredential` `name` (mit `spec.entra`). Gleiche Merkmale →
    /// dieselbe Quelle wie bei Deployments (Cache in [`aigw_providers::entra`]).
    fn entra_source(&self, name: &str) -> Result<Arc<EntraTokenSource>, PluginError> {
        let snap = self
            .snapshot
            .as_ref()
            .ok_or_else(|| PluginError::new("kein Snapshot für Zugangsdaten"))?
            .load();
        if let Some((v, src)) = self
            .entra
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            && *v == snap.version
        {
            return Ok(Arc::clone(src));
        }
        let cred = snap
            .credentials
            .get(name)
            .ok_or_else(|| PluginError::new(format!("Zugangsdaten {name:?} nicht gefunden")))?;
        let entra = cred.spec.entra.as_ref().ok_or_else(|| {
            PluginError::new(format!(
                "Zugangsdaten {name:?} ohne spec.entra (für auth: entraId nötig)"
            ))
        })?;
        let mut settings = JsonObject::new();
        entra.apply_to(&mut settings);
        let dep = DeploymentCtx {
            deployment: Deployment {
                id: cred.id,
                metadata: ObjectMeta::named(format!("plugin:{name}")),
                spec: DeploymentSpec {
                    model_group: String::new(),
                    provider: cred.spec.provider,
                    endpoint: None,
                    provider_model: String::new(),
                    credential: Some(name.to_owned()),
                    weight: 1,
                    priority: 0,
                    capacity: Default::default(),
                    enabled: true,
                    settings,
                },
            },
            credential: snap.credential_data.get(name).cloned(),
        };
        let src = match &self.entra_env {
            Some(env) => aigw_providers::entra::token_source_with_env(&dep, env),
            None => aigw_providers::entra::token_source(&dep),
        }
        .map_err(|e| PluginError::new(format!("Entra ID ({name}): {e}")))?;
        src.use_client(&UpstreamClient::from_outbound(self.client.clone()));
        self.entra
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(name.to_owned(), (snap.version, Arc::clone(&src)));
        Ok(src)
    }
}

impl GatewayHost {
    /// Wie [`GatewayHost::new`] mit eigenem HTTP-Client (SSRF-Regeln `policy`, eigener Pool).
    pub fn with_defaults(
        routes: Arc<dyn ModelRoutes>,
        counters: Arc<dyn CounterBackend>,
        snapshot: Option<SnapshotHandle>,
        policy: UpstreamPolicy,
    ) -> Result<Self, String> {
        let client = OutboundClient::builder(AddressGuard::policy(policy.clone()))
            .options(OutboundOptions {
                connect_timeout: std::time::Duration::from_secs(5),
                ..OutboundOptions::default()
            })
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self::new(routes, counters, snapshot, policy, client))
    }
}

impl PluginHost for GatewayHost {
    fn credential(&self, name: &str) -> Option<Arc<CredentialData>> {
        self.0
            .snapshot
            .as_ref()?
            .load()
            .credential_data
            .get(name)
            .cloned()
    }

    fn secret(&self, name: &str) -> Option<Arc<CredentialData>> {
        self.0
            .snapshot
            .as_ref()?
            .load()
            .secret_data
            .get(name)
            .cloned()
    }

    fn check_endpoint(&self, url: &str) -> Result<(), String> {
        self.0.policy.check_endpoint(url)
    }

    fn public_only(&self) -> Arc<dyn PluginHost> {
        let i = &self.0;
        Arc::clone(i.public.get_or_init(|| {
            let policy = i.policy.public_only();
            match i.client.public_only() {
                Ok(client) => Arc::new(GatewayHost(Arc::new(Inner {
                    routes: Arc::clone(&i.routes),
                    counters: Arc::clone(&i.counters),
                    snapshot: i.snapshot.clone(),
                    policy,
                    client,
                    entra_env: i.entra_env.clone(),
                    entra: Mutex::new(HashMap::new()),
                    public: OnceLock::new(),
                }))) as Arc<dyn PluginHost>,
                Err(e) => {
                    // Ohne eigenen Client keine Aufrufe (nie auf die privaten Regeln ausweichen).
                    tracing::error!(error = %e, "Plugin-Host nur für öffentliche Ziele nicht baubar");
                    Arc::new(aigw_plugins::NoHost::with_policy(policy))
                }
            }
        }))
    }

    fn post_json(&self, call: HttpCall) -> BoxFuture<'static, Result<HttpReply, PluginError>> {
        let inner = Arc::clone(&self.0);
        Box::pin(async move {
            inner
                .policy
                .check_endpoint(&call.url)
                .map_err(PluginError::new)?;
            let mut req = axum::http::Request::post(&call.url)
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .header(axum::http::header::ACCEPT, "application/json")
                .header(axum::http::header::USER_AGENT, aigw_outbound::USER_AGENT);
            for (k, v) in &call.headers {
                req = req.header(*k, v.as_str());
            }
            let req = req
                .body(Bytes::from(call.body))
                .map_err(|e| PluginError::new(format!("Anfrage ungültig: {e}")))?;
            let reply = inner
                .client
                .fetch(req, &FetchLimits::new(call.timeout, MAX_REPLY_BYTES))
                .await
                .map_err(|e| match e {
                    aigw_outbound::OutboundError::Timeout(_) => {
                        PluginError::new("Zeitlimit des Dienstes überschritten")
                    }
                    aigw_outbound::OutboundError::Blocked(m) => {
                        PluginError::new(format!("Ziel nicht erlaubt: {m}"))
                    }
                    other => PluginError::new(format!("Dienst nicht erreichbar: {other}")),
                })?;
            Ok(HttpReply {
                status: reply.status.as_u16(),
                body: reply.body.to_vec(),
            })
        })
    }

    fn entra_token(
        &self,
        credential: &str,
    ) -> BoxFuture<'static, Result<SecretString, PluginError>> {
        let inner = Arc::clone(&self.0);
        let name = credential.to_owned();
        Box::pin(async move {
            let src = inner.entra_source(&name)?;
            src.ensure()
                .await
                .map_err(|e| PluginError::new(format!("Entra ID: {}", e.error.message)))?;
            aigw_providers::auth::TokenSource::bearer_token(src.as_ref())
                .map_err(|e| PluginError::new(format!("Entra ID: {}", e.message)))
        })
    }

    fn chat(
        &self,
        model_group: &str,
        req: ChatRequest,
        calls: &PluginCalls,
    ) -> BoxFuture<'static, Result<ChatResponse, PluginError>> {
        let inner = Arc::clone(&self.0);
        let group = model_group.to_owned();
        let calls = calls.clone();
        Box::pin(async move {
            let route = inner
                .routes
                .route(&group)
                .ok_or_else(|| PluginError::new(format!("Modellgruppe {group:?} unbekannt")))?;
            let tokens = crate::observe::estimate_tokens(crate::observe::request_bytes(&req))
                + req.max_tokens.unwrap_or(256);
            let routed = exec::execute(
                inner.routes.as_ref(),
                route,
                req,
                CapacityCtx {
                    counters: &inner.counters,
                    tokens,
                    grants: None,
                },
            )
            .await;
            match routed.result {
                Ok(Outcome::Json(resp)) => {
                    // S-13: Verbrauch dem Key/Team der Anfrage zurechnen (Recorder).
                    let (provider, provider_model) =
                        routed.last.as_ref().map_or((None, None), |m| {
                            let d = m.provider.describe();
                            (d.provider, Some(d.provider_model.to_owned()))
                        });
                    calls.record(PluginCall {
                        model_group: group,
                        provider,
                        provider_model,
                        usage: resp.usage.clone().unwrap_or_default(),
                    });
                    Ok(resp)
                }
                Ok(Outcome::Stream(_)) => Err(PluginError::new("Prüfmodell hat gestreamt")),
                Err(e) => Err(PluginError::new(format!(
                    "Prüfmodell {group:?}: {}",
                    e.error.message
                ))),
            }
        })
    }
}
