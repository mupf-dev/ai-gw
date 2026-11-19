// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Wirksame Plugin-Kette einer (Org, Team, Key, Modellgruppe)-Kombination und ihre
//! Ausführung in den drei Phasen.
//!
//! Eine [`Chain`] ist unveränderlich und wird beim Snapshot-Aufbau vorberechnet
//! ([`crate::PluginIndex`]); je Phase liegt eine Indexliste der teilnehmenden Plugins bereit,
//! damit leere Phasen nichts kosten.
//!
//! **Zwiebelprinzip (F167b):** Die Anfrage-Haken laufen in Kettenreihenfolge (`order`
//! aufsteigend), die Antwort- und Stream-Haken in **umgekehrter** Reihenfolge. Ein Plugin, das
//! die Anfrage früh verändert (z. B. `pii` pseudonymisiert), sieht die Antwort als letztes und
//! kann sie zurückübersetzen – alle Plugins dazwischen (z. B. ein Webhook) sehen in beiden
//! Richtungen nur die veränderte Form.

use std::any::Any;
use std::fmt;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use aigw_core::ir::{ApiError, ChatResponse, Content, ErrorKind, StopReason, StreamEvent};
use aigw_core::resource::ScopeLevel;
use http::HeaderValue;

use crate::binding::{FailMode, OutputRejectMode};
use crate::metrics::PluginMetrics;
use crate::plugin::CallInfo;
use crate::plugin::{
    ChunkCtx, ChunkFlow, Flow, Hook, Meta, Plugin, PluginError, ReqCtx, RespCtx, ResponseHeaders,
};
use crate::registry::DEFAULT_TIMEOUT;

/// Eine Bindung in der Kette: Plugin-Instanz mit Steuerung.
pub struct Entry {
    pub(crate) plugin: Arc<dyn Plugin>,
    pub(crate) binding: Arc<str>,
    pub(crate) level: ScopeLevel,
    pub(crate) order: i32,
    pub(crate) fail_mode: FailMode,
    pub(crate) timeout: Duration,
    pub(crate) output_reject: OutputRejectMode,
    pub(crate) metrics: Arc<PluginMetrics>,
}

impl fmt::Debug for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Entry")
            .field("plugin", &self.plugin.name())
            .field("binding", &self.binding)
            .field("level", &self.level)
            .field("order", &self.order)
            .field("fail_mode", &self.fail_mode)
            .field("timeout", &self.timeout)
            .field("output_reject", &self.output_reject)
            .finish()
    }
}

impl Entry {
    /// Eintrag mit Standardwerten (globale Ebene, `order` 0, `failMode: closed`, 1 s,
    /// `outputRejectMode: finish`, eigene Metriken).
    pub fn new(binding: impl Into<Arc<str>>, plugin: Arc<dyn Plugin>) -> Self {
        Self {
            plugin,
            binding: binding.into(),
            level: ScopeLevel::Global,
            order: 0,
            fail_mode: FailMode::Closed,
            timeout: DEFAULT_TIMEOUT,
            output_reject: OutputRejectMode::Finish,
            metrics: Arc::default(),
        }
    }

    /// Setzt die Wirkung einer Ausgabe-Ablehnung.
    #[must_use]
    pub fn with_output_reject(mut self, mode: OutputRejectMode) -> Self {
        self.output_reject = mode;
        self
    }

    /// Setzt die Metriken (z. B. die der Registry, [`crate::Registry::metrics`]).
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<PluginMetrics>) -> Self {
        self.metrics = metrics;
        self
    }

    /// Setzt den Fehlermodus.
    #[must_use]
    pub fn with_fail_mode(mut self, mode: FailMode) -> Self {
        self.fail_mode = mode;
        self
    }

    /// Setzt das Zeitlimit asynchroner Haken.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Setzt die Reihenfolge.
    #[must_use]
    pub fn with_order(mut self, order: i32) -> Self {
        self.order = order;
        self
    }

    /// Name der Bindung.
    pub fn binding(&self) -> &str {
        &self.binding
    }

    /// Plugin-Instanz.
    pub fn plugin(&self) -> &Arc<dyn Plugin> {
        &self.plugin
    }

    /// Plugin-Name.
    pub fn plugin_name(&self) -> &'static str {
        self.plugin.name()
    }

    /// Ebene der Bindung.
    pub fn level(&self) -> ScopeLevel {
        self.level
    }

    /// Fehlermodus.
    pub fn fail_mode(&self) -> FailMode {
        self.fail_mode
    }

    /// Wirkung einer Ausgabe-Ablehnung.
    pub fn output_reject(&self) -> OutputRejectMode {
        self.output_reject
    }

    /// Setzt `x-aigw-guardrail: <plugin>` und protokolliert die Ablehnung (ohne Inhalte).
    fn blocked(&self, info: &CallInfo, phase: &'static str, headers: Option<&mut ResponseHeaders>) {
        tracing::info!(
            request_id = %info.request_id,
            plugin = self.plugin.name(),
            binding = %self.binding,
            phase,
            mode = self.output_reject.as_str(),
            "Guardrail hat blockiert"
        );
        if let Some(h) = headers {
            h.insert(
                http::HeaderName::from_static(crate::GUARDRAIL_HEADER),
                HeaderValue::from_static(self.plugin.name()),
            );
        }
    }

    /// Endereignis einer Ausgabe-Ablehnung im Stream.
    fn block_event(&self, err: ApiError) -> StreamEvent {
        match self.output_reject {
            OutputRejectMode::Finish => StreamEvent::Stop {
                reason: StopReason::ContentFilter,
                stop_sequence: None,
            },
            OutputRejectMode::Error => StreamEvent::Error(err),
        }
    }

    fn error(&self, err: &PluginError, info: &CallInfo, phase: &'static str) -> Option<ApiError> {
        self.metrics
            .failmode_triggered(self.plugin.name(), self.fail_mode);
        match self.fail_mode {
            FailMode::Open => {
                tracing::warn!(
                    request_id = %info.request_id,
                    plugin = self.plugin.name(),
                    binding = %self.binding,
                    phase,
                    error = %err,
                    "Plugin fehlgeschlagen – übersprungen (failMode: open)"
                );
                None
            }
            FailMode::Closed => {
                tracing::warn!(
                    request_id = %info.request_id,
                    plugin = self.plugin.name(),
                    binding = %self.binding,
                    phase,
                    error = %err,
                    "Plugin fehlgeschlagen – Anfrage abgelehnt (failMode: closed)"
                );
                Some(
                    ApiError::new(
                        ErrorKind::Internal,
                        format!("Plugin {:?} fehlgeschlagen", &*self.binding),
                    )
                    .with_code("plugin_error"),
                )
            }
        }
    }
}

/// Welche Stream-Haken laufen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkMode {
    /// Live-Stream: alle Plugins mit Stream-Phase.
    Live,
    /// Gepuffert: die Antwort lief schon durch [`Plugin::on_response`]; nur Plugins **ohne**
    /// Antwort-Phase sehen die daraus erzeugten Ereignisse (jede Ausgabe genau einmal).
    Buffered,
}

/// Unveränderliche, geordnete Plugin-Kette.
#[derive(Default)]
pub struct Chain {
    entries: Box<[Arc<Entry>]>,
    request: Box<[usize]>,
    response: Box<[usize]>,
    chunk_live: Box<[usize]>,
    chunk_buffered: Box<[usize]>,
    buffered: bool,
}

impl fmt::Debug for Chain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Chain")
            .field("bindings", &self.bindings().collect::<Vec<_>>())
            .field("buffered", &self.buffered)
            .finish()
    }
}

static EMPTY: LazyLock<Arc<Chain>> = LazyLock::new(|| Arc::new(Chain::default()));

impl Chain {
    /// Kette aus bereits geordneten Einträgen.
    pub fn new(entries: Vec<Arc<Entry>>) -> Self {
        let pick = |f: &dyn Fn(&Entry) -> bool| -> Box<[usize]> {
            entries
                .iter()
                .enumerate()
                .filter(|(_, e)| f(e))
                .map(|(i, _)| i)
                .collect()
        };
        // Zwiebelprinzip (F167b): Antwort- und Stream-Haken in umgekehrter Reihenfolge.
        let rev = |mut v: Box<[usize]>| {
            v.reverse();
            v
        };
        let request = pick(&|e| e.plugin.phases().request);
        let response = rev(pick(&|e| e.plugin.phases().response));
        let chunk_live = rev(pick(&|e| e.plugin.phases().chunk));
        let chunk_buffered = rev(pick(&|e| {
            let p = e.plugin.phases();
            p.chunk && !p.response
        }));
        let buffered = entries.iter().any(|e| e.plugin.needs_buffered_output());
        Self {
            entries: entries.into_boxed_slice(),
            request,
            response,
            chunk_live,
            chunk_buffered,
            buffered,
        }
    }

    /// Geteilte leere Kette.
    pub fn empty() -> Arc<Self> {
        Arc::clone(&EMPTY)
    }

    /// Keine Plugins?
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Einträge in Ausführungsreihenfolge der Anfrage-Phase (Antwort und Stream: umgekehrt).
    pub fn entries(&self) -> &[Arc<Entry>] {
        &self.entries
    }

    /// Namen der Bindungen in Ausführungsreihenfolge.
    pub fn bindings(&self) -> impl Iterator<Item = &str> + '_ {
        self.entries.iter().map(|e| e.binding())
    }

    /// Anzahl der Plugins mit Anfrage-Haken.
    pub fn request_hooks(&self) -> usize {
        self.request.len()
    }

    /// Anzahl der Plugins mit Antwort-Haken.
    pub fn response_hooks(&self) -> usize {
        self.response.len()
    }

    /// Gibt es Anfrage-Haken?
    pub fn has_request(&self) -> bool {
        !self.request.is_empty()
    }

    /// Gibt es Antwort-Haken?
    pub fn has_response(&self) -> bool {
        !self.response.is_empty()
    }

    /// Gibt es Stream-Haken für `mode`?
    pub fn has_chunk(&self, mode: ChunkMode) -> bool {
        !self.chunk_slots(mode).is_empty()
    }

    /// Braucht mindestens ein Plugin die vollständige Ausgabe (gepufferter Modus)?
    pub fn needs_buffered_output(&self) -> bool {
        self.buffered
    }

    fn chunk_slots(&self, mode: ChunkMode) -> &[usize] {
        match mode {
            ChunkMode::Live => &self.chunk_live,
            ChunkMode::Buffered => &self.chunk_buffered,
        }
    }

    /// Anfrage-Phase. Liefert [`Flow::Continue`], wenn die Anfrage weiterlaufen soll.
    ///
    /// `model` und `stream` der Anfrage darf kein Plugin ändern (Modellfreigabe und Routing
    /// sind schon entschieden): Die Kette setzt sie zurück und behandelt die Änderung als
    /// Plugin-Fehler.
    pub async fn run_request(&self, ctx: &mut ReqCtx<'_>) -> Flow {
        if self.request.is_empty() {
            return Flow::Continue;
        }
        let model = ctx.request.model.clone();
        let stream = ctx.request.stream;
        for &i in &self.request {
            let e = &self.entries[i];
            let mut res = match e.plugin.on_request(ctx) {
                Hook::Ready(r) => r,
                Hook::Pending(fut) => timed(e.timeout, fut).await,
            };
            if ctx.request.model != model || ctx.request.stream != stream {
                ctx.request.model.clone_from(&model);
                ctx.request.stream = stream;
                res = Err(PluginError::new(
                    "Plugin hat `model` oder `stream` verändert (Modell-Umleitung ist gesperrt)",
                ));
            }
            match res {
                Ok(Flow::Continue) => {}
                // Eingabe-Ablehnung eines Guardrails: immer Fehler (F160).
                Ok(Flow::Block(err)) => {
                    e.blocked(ctx.info, "request", Some(&mut ctx.response_headers));
                    return Flow::Reject(err);
                }
                Ok(flow) => return flow,
                Err(err) => {
                    if let Some(api) = e.error(&err, ctx.info, "request") {
                        return Flow::Reject(api);
                    }
                }
            }
        }
        Flow::Continue
    }

    /// Antwort-Phase, in **umgekehrter** Kettenreihenfolge (Zwiebelprinzip). [`Flow::Respond`]
    /// ersetzt die Antwort und läuft weiter (bei [`Plugin::rerun_inner_on_respond`] zuerst
    /// noch einmal durch die inneren Plugins, AIGW-113); [`Flow::Reject`] bricht ab;
    /// [`Flow::Block`] beendet die Kette je `outputRejectMode` mit gefilterter Antwort
    /// (`Continue` mit [`content_filtered`] in `ctx.response`) oder als `Reject`.
    pub async fn run_response(&self, ctx: &mut RespCtx<'_>) -> Flow {
        for (pos, &i) in self.response.iter().enumerate() {
            let e = &self.entries[i];
            match Self::response_step(e, ctx).await {
                Step::Next => {}
                Step::Replaced => {
                    if e.plugin.rerun_inner_on_respond() {
                        // Neue Antwort: die inneren Plugins (schon gelaufen) prüfen sie erneut;
                        // deren Ersetzungen laufen nicht noch einmal zurück (keine Schleife).
                        for &j in &self.response[..pos] {
                            if let Step::Done(flow) =
                                Self::response_step(&self.entries[j], ctx).await
                            {
                                return flow;
                            }
                        }
                    }
                }
                Step::Done(flow) => return flow,
            }
        }
        Flow::Continue
    }

    /// Ein Antwort-Haken.
    async fn response_step(e: &Entry, ctx: &mut RespCtx<'_>) -> Step {
        let res = match e.plugin.on_response(ctx) {
            Hook::Ready(r) => r,
            Hook::Pending(fut) => timed(e.timeout, fut).await,
        };
        match res {
            Ok(Flow::Continue) => Step::Next,
            Ok(Flow::Respond(resp)) => {
                ctx.response = *resp;
                Step::Replaced
            }
            Ok(Flow::Reject(err)) => Step::Done(Flow::Reject(err)),
            Ok(Flow::Block(err)) => {
                e.blocked(ctx.info, "response", Some(ctx.response_headers));
                Step::Done(match e.output_reject {
                    OutputRejectMode::Finish => {
                        content_filtered(&mut ctx.response);
                        Flow::Continue
                    }
                    OutputRejectMode::Error => Flow::Reject(err),
                })
            }
            Err(err) => match e.error(&err, ctx.info, "response") {
                Some(api) => Step::Done(Flow::Reject(api)),
                None => Step::Next,
            },
        }
    }
}

/// Ergebnis eines Antwort-Hakens in [`Chain::run_response`].
enum Step {
    /// Weiter.
    Next,
    /// Antwort ersetzt ([`Flow::Respond`]), weiter.
    Replaced,
    /// Kette endet mit diesem Ergebnis.
    Done(Flow),
}

/// Entfernt den Inhalt aller Varianten und setzt `stop_reason: content_filter` (OpenAI
/// `finish_reason: content_filter`, Anthropic `stop_reason: refusal`). Usage, ID und Modell
/// bleiben (der Upstream-Verbrauch wird abgerechnet).
pub fn content_filtered(resp: &mut ChatResponse) {
    for c in &mut resp.choices {
        c.message.content = Content::Text(String::new());
        c.message.extra.clear();
        c.message.name = None;
        c.stop_reason = Some(StopReason::ContentFilter);
        c.stop_sequence = None;
        // z. B. `logprobs` mit den Tokens der Antwort
        c.extra.clear();
    }
}

async fn timed<T>(
    timeout: Duration,
    fut: crate::plugin::BoxFuture<'_, Result<T, PluginError>>,
) -> Result<T, PluginError> {
    match tokio::time::timeout(timeout, fut).await {
        Ok(r) => r,
        Err(_) => Err(PluginError::new(format!(
            "Zeitlimit von {} ms überschritten",
            timeout.as_millis()
        ))),
    }
}

/// Führt die Stream-Haken einer Kette für **einen** Stream aus (Zustand je Plugin, Meta) – in
/// umgekehrter Kettenreihenfolge (Zwiebelprinzip, F167b).
pub struct ChunkRunner {
    chain: Arc<Chain>,
    info: Arc<CallInfo>,
    meta: Meta,
    mode: ChunkMode,
    states: Vec<Option<Box<dyn Any + Send>>>,
    terminated: bool,
    /// Endereignis eines Abbruchs (Fehler bzw. `Stop` mit `content_filter`), bis es
    /// ausgegeben ist – auch wenn ein äußeres Plugin es zurückhält.
    terminal: Option<StreamEvent>,
    cur: Vec<StreamEvent>,
    next: Vec<StreamEvent>,
    emitted: Vec<StreamEvent>,
}

impl fmt::Debug for ChunkRunner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChunkRunner")
            .field("chain", &self.chain)
            .field("mode", &self.mode)
            .field("terminated", &self.terminated)
            .finish_non_exhaustive()
    }
}

fn is_terminal(ev: &StreamEvent) -> bool {
    matches!(ev, StreamEvent::Stop { .. } | StreamEvent::Error(_))
}

impl ChunkRunner {
    /// Neuer Läufer.
    pub fn new(chain: Arc<Chain>, info: Arc<CallInfo>, meta: Meta, mode: ChunkMode) -> Self {
        let n = chain.chunk_slots(mode).len();
        Self {
            chain,
            info,
            meta,
            mode,
            states: (0..n).map(|_| None).collect(),
            terminated: false,
            terminal: None,
            cur: Vec::with_capacity(4),
            next: Vec::with_capacity(4),
            emitted: Vec::new(),
        }
    }

    /// Wurde der Stream von einem Plugin beendet (Reject, Block oder `failMode: closed`)?
    pub fn is_terminated(&self) -> bool {
        self.terminated
    }

    /// Verarbeitet ein Upstream-Ereignis; Ergebnisse werden an `out` angehängt.
    pub fn push(&mut self, ev: StreamEvent, out: &mut Vec<StreamEvent>) {
        if self.terminated {
            return;
        }
        self.cur.push(ev);
        self.run_from(0, out);
        self.flush_terminal(out);
    }

    /// Ende des Upstream-Streams: [`Plugin::on_stream_end`] in Stream-Reihenfolge (innen nach
    /// außen); was ein Plugin dabei ausgibt, durchläuft die nachfolgenden (äußeren) Plugins.
    pub fn finish(&mut self, out: &mut Vec<StreamEvent>) {
        let n = self.states.len();
        for pos in 0..n {
            if self.terminated {
                break;
            }
            let idx = self.chain.chunk_slots(self.mode)[pos];
            let e = Arc::clone(&self.chain.entries[idx]);
            let mut ctx = ChunkCtx {
                info: &self.info,
                meta: &mut self.meta,
                state: &mut self.states[pos],
                emitted: &mut self.emitted,
            };
            let res = e.plugin.on_stream_end(&mut ctx);
            self.cur.append(&mut self.emitted);
            let end = match res {
                Ok(ChunkFlow::Continue | ChunkFlow::Drop) => None,
                Ok(ChunkFlow::Reject(err)) => Some(StreamEvent::Error(err)),
                Ok(ChunkFlow::Block(err)) => {
                    e.blocked(&self.info, "stream_end", None);
                    Some(e.block_event(err))
                }
                Err(err) => e
                    .error(&err, &self.info, "stream_end")
                    .map(StreamEvent::Error),
            };
            if let Some(ev) = end {
                self.terminal = Some(ev.clone());
                self.cur.push(ev);
                self.terminated = true;
            }
            if !self.cur.is_empty() {
                self.run_from(pos + 1, out);
            }
        }
        self.flush_terminal(out);
    }

    /// Gibt das Endereignis eines Abbruchs aus, falls ein äußeres Plugin es zurückgehalten
    /// hat (danach liest niemand mehr – sonst ginge es verloren).
    fn flush_terminal(&mut self, out: &mut Vec<StreamEvent>) {
        if !self.terminated {
            return;
        }
        if let Some(t) = self.terminal.take()
            && out.iter().rev().find(|e| is_terminal(e)) != Some(&t)
        {
            out.push(t);
        }
    }

    /// Lässt `self.cur` ab Plugin-Position `start` durch die Kette laufen.
    fn run_from(&mut self, start: usize, out: &mut Vec<StreamEvent>) {
        let n = self.states.len();
        for pos in start..n {
            let idx = self.chain.chunk_slots(self.mode)[pos];
            let e = &self.chain.entries[idx];
            let mut stop = false;
            for mut ev in self.cur.drain(..) {
                if stop {
                    break;
                }
                let mut ctx = ChunkCtx {
                    info: &self.info,
                    meta: &mut self.meta,
                    state: &mut self.states[pos],
                    emitted: &mut self.emitted,
                };
                let res = e.plugin.on_chunk(&mut ctx, &mut ev);
                self.next.append(&mut self.emitted);
                let end = match res {
                    Ok(ChunkFlow::Continue) => {
                        self.next.push(ev);
                        None
                    }
                    Ok(ChunkFlow::Drop) => None,
                    Ok(ChunkFlow::Reject(err)) => Some(StreamEvent::Error(err)),
                    Ok(ChunkFlow::Block(err)) => {
                        e.blocked(&self.info, "chunk", None);
                        Some(e.block_event(err))
                    }
                    Err(err) => match e.error(&err, &self.info, "chunk") {
                        None => {
                            self.next.push(ev);
                            None
                        }
                        Some(api) => Some(StreamEvent::Error(api)),
                    },
                };
                if let Some(t) = end {
                    self.terminal = Some(t.clone());
                    self.next.push(t);
                    stop = true;
                }
            }
            self.terminated |= stop;
            std::mem::swap(&mut self.cur, &mut self.next);
            if self.cur.is_empty() {
                return;
            }
        }
        out.append(&mut self.cur);
    }
}
