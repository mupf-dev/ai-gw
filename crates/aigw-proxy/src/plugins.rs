// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Einhängepunkte der Plugin-Kette (AIGW-36, Architektur §3 Schritte 5 und 10) im Hot Path.
//!
//! Reihenfolge in [`crate::handlers`]:
//!
//! 1. Auth, Modellfreigabe (`authorize_model`)
//! 2. Limits/Budgets reservieren (AIGW-30/31) – **vor** den Pre-Plugins
//! 3. [`PluginRun::run_pre_plugins`] – kann antworten (`Respond`) oder ablehnen (`Reject`)
//! 4. Antwort-Cache ([`crate::cache`], AIGW-39/40; Richtlinie aus dem Plugin `cache`) →
//!    Routing/Upstream
//! 5. Nicht gestreamt bzw. gepuffert: [`PluginRun::run_post_plugins`] vor dem Encoder;
//!    gestreamt: [`PluginRun::wrap_stream`] (Stream-Haken je Ereignis vor dem Encoder)
//!
//! Ohne Bindungen kostet das einen Index-Lookup; Kontext, Meta und Stream-Hülle werden
//! dann gar nicht erst angelegt.

use std::sync::Arc;

use aigw_core::ir::{ApiError, ChatRequest, ChatResponse};
use aigw_core::wire::WireFormat;
use aigw_plugins::{
    CallInfo, Chain, ChunkMode, ChunkRunner, Flow, Meta, PluginStream, ReqCtx, RespCtx,
    ResponseHeaders, response_events,
};
use axum::http::HeaderMap;

use crate::ProxyState;
use crate::auth::AuthContext;
use crate::provider::EventStream;

/// Ergebnis der Pre-Plugins.
#[allow(clippy::large_enum_variant)] // kurzlebig, wird sofort zerlegt
pub(crate) enum Pre {
    /// Weiter mit der (ggf. veränderten) Anfrage.
    Continue(ChatRequest),
    /// Ein Plugin antwortet selbst (kein Upstream-Aufruf).
    Respond(ChatResponse),
}

/// Phase der Kette (für Spans).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PluginPhase {
    /// Anfrage-Haken.
    Request,
    /// Antwort-Haken.
    Response,
}

/// Plugin-Lauf einer Anfrage: wirksame Kette und Metadaten zwischen den Phasen.
pub(crate) struct PluginRun {
    chain: Arc<Chain>,
    info: Option<Arc<CallInfo>>,
    meta: Meta,
}

impl PluginRun {
    /// Wirksame Kette für Key und Modellgruppe (vorberechnet, siehe
    /// [`aigw_plugins::PluginIndex`]).
    pub(crate) fn new(
        st: &ProxyState,
        auth: &AuthContext,
        model_group: Option<&str>,
        request_id: &str,
        ingress: WireFormat,
        stream: bool,
    ) -> Self {
        let chain =
            st.plugins
                .chain_for(auth.snapshot.as_deref(), auth.key.as_deref(), model_group);
        let info = (!chain.is_empty()).then(|| {
            Arc::new(CallInfo {
                request_id: request_id.to_owned(),
                ingress,
                key: auth.key.clone(),
                model_group: model_group.map(str::to_owned),
                stream,
                guardrails: aigw_core::observe::GuardrailHits::default(),
            })
        });
        Self {
            chain,
            info,
            meta: Meta::new(),
        }
    }

    /// Schritt 5 (Pre-Plugins): nach Auth und Limits, vor Cache und Routing.
    pub(crate) async fn run_pre_plugins(
        &mut self,
        req: ChatRequest,
        client_headers: &HeaderMap,
        response_headers: &mut ResponseHeaders,
    ) -> Result<Pre, ApiError> {
        let Some(info) = self.info.as_ref().filter(|_| self.chain.has_request()) else {
            return Ok(Pre::Continue(req));
        };
        let mut ctx = ReqCtx {
            info,
            request: req,
            client_headers,
            upstream_headers: HeaderMap::new(),
            response_headers: std::mem::take(response_headers),
            meta: std::mem::take(&mut self.meta),
        };
        let flow = self.chain.run_request(&mut ctx).await;
        let ReqCtx {
            mut request,
            upstream_headers,
            response_headers: rh,
            meta,
            ..
        } = ctx;
        *response_headers = rh;
        self.meta = meta;
        for (name, value) in &upstream_headers {
            if let Ok(v) = value.to_str() {
                request
                    .hints
                    .upstream_headers
                    .push((name.as_str().to_owned(), v.to_owned()));
            }
        }
        match flow {
            Flow::Continue => Ok(Pre::Continue(request)),
            Flow::Respond(resp) => Ok(Pre::Respond(*resp)),
            Flow::Reject(e) => Err(e),
        }
    }

    /// Wirksame Cache-Richtlinie (Plugin `cache`, AIGW-39/40) oder `None` ohne Bindung.
    pub(crate) fn cache_policy(&self) -> Option<Arc<aigw_plugins::builtin::cache::CachePolicy>> {
        if self.chain.is_empty() {
            return None;
        }
        aigw_plugins::builtin::cache::policy_of(&self.chain)
    }

    /// Sammelstelle der Guardrail-Erkennungen (nur wenn die Anfrage Plugins hat).
    pub(crate) fn guardrail_hits(&self) -> Option<aigw_core::observe::GuardrailHits> {
        self.info.as_ref().map(|i| i.guardrails.clone())
    }

    /// Anzahl der Plugins mit Haken in `phase`.
    pub(crate) fn hooks(&self, phase: PluginPhase) -> usize {
        match phase {
            PluginPhase::Request => self.chain.request_hooks(),
            PluginPhase::Response => self.chain.response_hooks(),
        }
    }

    /// Gepufferter Modus (Ausgabe-Plugins brauchen die ganze Antwort)?
    pub(crate) fn buffers(&self, req: &ChatRequest) -> bool {
        req.is_stream() && self.chain.needs_buffered_output()
    }

    /// Schritt 10 (Post-Plugins) für nicht gestreamte und gepufferte Antworten, vor dem
    /// Encoder.
    pub(crate) async fn run_post_plugins(
        &mut self,
        resp: ChatResponse,
        response_headers: &mut ResponseHeaders,
    ) -> Result<ChatResponse, ApiError> {
        let Some(info) = self.info.as_ref().filter(|_| self.chain.has_response()) else {
            return Ok(resp);
        };
        let mut ctx = RespCtx {
            info,
            response: resp,
            response_headers,
            meta: &mut self.meta,
        };
        match self.chain.run_response(&mut ctx).await {
            Flow::Reject(e) => Err(e),
            Flow::Continue | Flow::Respond(_) => Ok(ctx.response),
        }
    }

    /// Schritt 10 (Stream-Plugins): jedes IR-Ereignis läuft vor dem Encoder durch die
    /// Stream-Haken. Ohne Stream-Haken bleibt der Strom unverändert.
    pub(crate) fn wrap_stream(self, events: EventStream, mode: ChunkMode) -> EventStream {
        match self.info {
            Some(info) if self.chain.has_chunk(mode) => Box::pin(PluginStream::new(
                events,
                ChunkRunner::new(self.chain, info, self.meta, mode),
            )),
            _ => events,
        }
    }
}

/// Antwort → Ereignisstrom (gepufferter Modus, `Respond` bei gestreamter Anfrage).
pub(crate) fn response_stream(resp: &ChatResponse) -> EventStream {
    Box::pin(futures_util::stream::iter(response_events(resp)))
}
