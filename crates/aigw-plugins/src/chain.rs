// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Wirksame Plugin-Kette einer (Org, Team, Key, Modellgruppe)-Kombination und ihre
//! Ausführung in den drei Phasen.
//!
//! Eine [`Chain`] ist unveränderlich und wird beim Snapshot-Aufbau vorberechnet
//! ([`crate::PluginIndex`]); je Phase liegt eine Indexliste der teilnehmenden Plugins bereit,
//! damit leere Phasen nichts kosten.

use std::any::Any;
use std::fmt;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use aigw_core::ir::{ApiError, ErrorKind, StreamEvent};
use aigw_core::resource::ScopeLevel;

use crate::binding::FailMode;
use crate::plugin::CallInfo;
use crate::plugin::{ChunkCtx, ChunkFlow, Flow, Hook, Meta, Plugin, PluginError, ReqCtx, RespCtx};
use crate::registry::DEFAULT_TIMEOUT;

/// Eine Bindung in der Kette: Plugin-Instanz mit Steuerung.
pub struct Entry {
    pub(crate) plugin: Arc<dyn Plugin>,
    pub(crate) binding: Arc<str>,
    pub(crate) level: ScopeLevel,
    pub(crate) order: i32,
    pub(crate) fail_mode: FailMode,
    pub(crate) timeout: Duration,
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
            .finish()
    }
}

impl Entry {
    /// Eintrag mit Standardwerten (globale Ebene, `order` 0, `failMode: closed`, 1 s).
    pub fn new(binding: impl Into<Arc<str>>, plugin: Arc<dyn Plugin>) -> Self {
        Self {
            plugin,
            binding: binding.into(),
            level: ScopeLevel::Global,
            order: 0,
            fail_mode: FailMode::Closed,
            timeout: DEFAULT_TIMEOUT,
        }
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

    fn error(&self, err: &PluginError, info: &CallInfo, phase: &'static str) -> Option<ApiError> {
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
        let request = pick(&|e| e.plugin.phases().request);
        let response = pick(&|e| e.plugin.phases().response);
        let chunk_live = pick(&|e| e.plugin.phases().chunk);
        let chunk_buffered = pick(&|e| {
            let p = e.plugin.phases();
            p.chunk && !p.response
        });
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

    /// Einträge in Ausführungsreihenfolge.
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

    /// Antwort-Phase. [`Flow::Respond`] ersetzt die Antwort und läuft weiter;
    /// [`Flow::Reject`] bricht ab.
    pub async fn run_response(&self, ctx: &mut RespCtx<'_>) -> Flow {
        for &i in &self.response {
            let e = &self.entries[i];
            let res = match e.plugin.on_response(ctx) {
                Hook::Ready(r) => r,
                Hook::Pending(fut) => timed(e.timeout, fut).await,
            };
            match res {
                Ok(Flow::Continue) => {}
                Ok(Flow::Respond(resp)) => ctx.response = *resp,
                Ok(Flow::Reject(err)) => return Flow::Reject(err),
                Err(err) => {
                    if let Some(api) = e.error(&err, ctx.info, "response") {
                        return Flow::Reject(api);
                    }
                }
            }
        }
        Flow::Continue
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

/// Führt die Stream-Haken einer Kette für **einen** Stream aus (Zustand je Plugin, Meta).
pub struct ChunkRunner {
    chain: Arc<Chain>,
    info: Arc<CallInfo>,
    meta: Meta,
    mode: ChunkMode,
    states: Vec<Option<Box<dyn Any + Send>>>,
    terminated: bool,
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
            cur: Vec::with_capacity(4),
            next: Vec::with_capacity(4),
            emitted: Vec::new(),
        }
    }

    /// Wurde der Stream von einem Plugin beendet (Reject oder `failMode: closed`)?
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
    }

    /// Ende des Upstream-Streams: [`Plugin::on_stream_end`] in Kettenreihenfolge; was ein
    /// Plugin dabei ausgibt, durchläuft die nachfolgenden Plugins.
    pub fn finish(&mut self, out: &mut Vec<StreamEvent>) {
        let n = self.states.len();
        for pos in 0..n {
            if self.terminated {
                return;
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
            match res {
                Ok(ChunkFlow::Continue | ChunkFlow::Drop) => {}
                Ok(ChunkFlow::Reject(err)) => {
                    self.cur.push(StreamEvent::Error(err));
                    self.terminated = true;
                }
                Err(err) => {
                    if let Some(api) = e.error(&err, &self.info, "stream_end") {
                        self.cur.push(StreamEvent::Error(api));
                        self.terminated = true;
                    }
                }
            }
            if !self.cur.is_empty() {
                self.run_from(pos + 1, out);
            }
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
                match res {
                    Ok(ChunkFlow::Continue) => self.next.push(ev),
                    Ok(ChunkFlow::Drop) => {}
                    Ok(ChunkFlow::Reject(err)) => {
                        self.next.push(StreamEvent::Error(err));
                        stop = true;
                    }
                    Err(err) => match e.error(&err, &self.info, "chunk") {
                        None => self.next.push(ev),
                        Some(api) => {
                            self.next.push(StreamEvent::Error(api));
                            stop = true;
                        }
                    },
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
