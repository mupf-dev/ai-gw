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
//! Antwort- und Stream-Haken laufen in umgekehrter Kettenreihenfolge (Zwiebelprinzip,
//! F167b); Ausgabe-Guardrails beenden je `outputRejectMode` mit `200` +
//! `content_filter`/`refusal` (F160) – beides regelt [`aigw_plugins::Chain`].
//!
//! Ohne Bindungen kostet das einen Index-Lookup; Kontext, Meta und Stream-Hülle werden
//! dann gar nicht erst angelegt.

use std::sync::Arc;

use aigw_core::ir::{ApiError, ChatRequest, ChatResponse};
use aigw_core::wire::WireFormat;
use aigw_plugins::{
    CallInfo, Chain, ChunkMode, ChunkRunner, Flow, Meta, PluginStream, ReqCtx, RespCtx,
    ResponseHeaders,
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
    /// `limits.default_max_tokens` (Grundlage der Admission ohne `max_tokens`, S-30).
    default_max_tokens: u64,
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
                plugin_calls: aigw_core::observe::PluginCalls::default(),
            })
        });
        Self {
            chain,
            info,
            meta: Meta::new(),
            default_max_tokens: st.limits.default_max_tokens(),
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
        let admitted = (req.choices(), req.max_tokens);
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
        if matches!(flow, Flow::Continue) {
            keep_admitted(
                &mut request,
                admitted,
                self.default_max_tokens,
                &info.request_id,
            );
            // AIGW-113: Endgültige Anfrage für Plugins, die selbst neu anfragen (`schema-guard`
            // mit `retry`) – nur auf deren Wunsch (Schlüssel gesetzt), sonst kostenlos.
            if self.meta.contains_key(aigw_plugins::FINAL_REQUEST_META) {
                match serde_json::to_value(&request) {
                    Ok(v) => {
                        self.meta
                            .insert(aigw_plugins::FINAL_REQUEST_META.to_owned(), v);
                    }
                    Err(_) => {
                        self.meta.remove(aigw_plugins::FINAL_REQUEST_META);
                    }
                }
            }
        }
        match flow {
            Flow::Continue => Ok(Pre::Continue(request)),
            Flow::Respond(resp) => Ok(Pre::Respond(*resp)),
            // `Block` setzt die Kette in `Reject` um (Header schon gesetzt).
            Flow::Reject(e) | Flow::Block(e) => Err(e),
        }
    }

    /// Anfrage-Haken für Endpunkte ohne Chat-Ausgabe (Embeddings, Bilder, Sprachausgabe;
    /// AIGW-21-Folge): Die Eingabetexte laufen als Nutzernachrichten einer Chat-Sicht durch
    /// die Kette. Ablehnungen (Guardrails, Webhook-`reject`) gelten, geänderte Texte (z. B.
    /// maskierte PII) werden übernommen; eine Plugin-Antwort (`Respond`) ist hier nicht
    /// möglich (`400`). Antwort-Haken gibt es für diese Endpunkte nicht.
    pub(crate) async fn check_input(
        &mut self,
        model: &str,
        texts: Vec<String>,
        client_headers: &HeaderMap,
        response_headers: &mut ResponseHeaders,
    ) -> Result<Vec<String>, ApiError> {
        if self.info.is_none() || !self.chain.has_request() || texts.is_empty() {
            return Ok(texts);
        }
        let n = texts.len();
        let view = ChatRequest {
            model: model.to_owned(),
            messages: texts
                .iter()
                .map(|t| aigw_core::ir::Message::text(aigw_core::ir::Role::User, t.clone()))
                .collect(),
            ..ChatRequest::default()
        };
        match self
            .run_pre_plugins(view, client_headers, response_headers)
            .await?
        {
            Pre::Continue(req) if req.messages.len() == n => {
                Ok(req.messages.iter().map(|m| m.content.text()).collect())
            }
            // Struktur verändert (Nachrichten ergänzt/entfernt): Eingabe unverändert lassen.
            Pre::Continue(_) => Ok(texts),
            Pre::Respond(_) => Err(ApiError::invalid_request(
                "Plugin-Antwort ist für diesen Endpunkt nicht möglich",
            )
            .with_code("plugin_respond_unsupported")),
        }
    }

    /// Wirksame Cache-Richtlinie (Plugin `cache`, AIGW-39/40) oder `None` ohne Bindung.
    pub(crate) fn cache_policy(&self) -> Option<Arc<aigw_plugins::builtin::cache::CachePolicy>> {
        if self.chain.is_empty() {
            return None;
        }
        aigw_plugins::builtin::cache::policy_of(&self.chain)
    }

    /// Anfrage für den Cache-Schlüssel: ohne das Canary-Token von `prompt-leak` (AIGW-112).
    pub(crate) fn cache_view<'a>(&self, req: &'a ChatRequest) -> std::borrow::Cow<'a, ChatRequest> {
        aigw_plugins::builtin::prompt_leak::cache_view(&self.meta, req)
    }

    /// Sammelstellen der Guardrail-Erkennungen und der Plugin-Modellaufrufe (nur wenn die
    /// Anfrage Plugins hat).
    pub(crate) fn plugin_sinks(
        &self,
    ) -> Option<(
        aigw_core::observe::GuardrailHits,
        aigw_core::observe::PluginCalls,
    )> {
        self.info
            .as_ref()
            .map(|i| (i.guardrails.clone(), i.plugin_calls.clone()))
    }

    /// Anzahl der Plugins mit Haken in `phase`.
    pub(crate) fn hooks(&self, phase: PluginPhase) -> usize {
        match phase {
            PluginPhase::Request => self.chain.request_hooks(),
            PluginPhase::Response => self.chain.response_hooks(),
        }
    }

    /// Laufen Stream-Haken (live) auf der Antwort?
    pub(crate) fn streams_live(&self) -> bool {
        self.info.is_some() && self.chain.has_chunk(ChunkMode::Live)
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
            // Ausgabe-Guardrail mit `outputRejectMode: finish` kommt als `Continue` mit
            // gefilterter Antwort (200, content_filter/refusal, F160).
            Flow::Reject(e) | Flow::Block(e) => Err(e),
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
    // Mehrere Varianten (`n > 1`, F12) mit `Choice`-Markern.
    Box::pin(futures_util::stream::iter(crate::choices::response_events(
        resp,
    )))
}

/// S-30: Limits und Budgets wurden **vor** den Pre-Plugins mit `n` und `max_tokens` der
/// Client-Anfrage reserviert (Admission). Ein Plugin (z. B. Webhook `modify`, WASM,
/// `request-transform`) darf die Anfrage danach nicht teurer machen: `n` wird höchstens auf den
/// zugelassenen Wert begrenzt, `max_tokens` je Variante höchstens auf den zugelassenen Wert
/// (`max_tokens` des Clients bzw. `limits.default_max_tokens`). Verringern ist erlaubt.
///
/// Begrenzen statt ablehnen: Die Anfrage bleibt so innerhalb der Reservierung, andere
/// Änderungen des Plugins (z. B. Schwärzungen) bleiben wirksam, und ein falsch konfigurierter
/// Webhook legt nicht den ganzen Verkehr lahm. Die Begrenzung wird geloggt (`warn`).
pub(crate) fn keep_admitted(
    req: &mut ChatRequest,
    (n, max_tokens): (u32, Option<u64>),
    default_max_tokens: u64,
    request_id: &str,
) {
    if req.choices() > n {
        tracing::warn!(
            request_id,
            requested = req.choices(),
            admitted = n,
            "Plugin hat n erhöht – auf den zugelassenen Wert begrenzt (S-30)"
        );
        req.n = (n > 1).then_some(n);
    }
    let allowed = max_tokens.unwrap_or(default_max_tokens);
    match (max_tokens, req.max_tokens) {
        (_, Some(m)) if m > allowed => {
            tracing::warn!(
                request_id,
                requested = m,
                admitted = allowed,
                "Plugin hat max_tokens erhöht – auf den zugelassenen Wert begrenzt (S-30)"
            );
            req.max_tokens = Some(allowed);
        }
        // Das Plugin hat die Grenze des Clients entfernt: wieder setzen.
        (Some(orig), None) => {
            tracing::warn!(
                request_id,
                admitted = orig,
                "Plugin hat max_tokens entfernt – zugelassener Wert wieder gesetzt (S-30)"
            );
            req.max_tokens = Some(orig);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(n: Option<u32>, max_tokens: Option<u64>) -> ChatRequest {
        ChatRequest {
            model: "m".into(),
            n,
            max_tokens,
            ..ChatRequest::default()
        }
    }

    /// S-30: Plugins dürfen `n`/`max_tokens` nach der Admission nicht erhöhen.
    #[test]
    fn plugin_changes_stay_within_admission() {
        let cases = [
            // (Client n/max, nach Plugin n/max, erwartet n/max)
            (
                (None, Some(100)),
                (Some(4), Some(100_000)),
                (None, Some(100)),
            ),
            (
                (Some(2), Some(100)),
                (Some(3), Some(50)),
                (Some(2), Some(50)),
            ),
            ((None, Some(100)), (None, None), (None, Some(100))),
            ((None, None), (None, Some(9_999)), (None, Some(4096))),
            ((None, None), (None, Some(10)), (None, Some(10))),
            ((Some(3), None), (Some(1), None), (Some(1), None)),
        ];
        for ((cn, cm), (pn, pm), (wn, wm)) in cases {
            let client = req(cn, cm);
            let mut r = req(pn, pm);
            keep_admitted(&mut r, (client.choices(), client.max_tokens), 4096, "rid");
            assert_eq!(
                (r.n, r.max_tokens),
                (wn, wm),
                "{cn:?}/{cm:?} → {pn:?}/{pm:?}"
            );
        }
    }
}
