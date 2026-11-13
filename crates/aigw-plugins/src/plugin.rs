// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Plugin-Schnittstelle (Architektur §9): Trait, Kontexte und Ergebnisse der drei Phasen.
//!
//! - [`Plugin::on_request`] (asynchron möglich) – nach Auth und Limits, vor Cache und Routing.
//! - [`Plugin::on_response`] (asynchron möglich) – auf der vollständigen IR-Antwort, vor dem
//!   Encoder (nicht gestreamte Anfragen und gepufferter Modus).
//! - [`Plugin::on_chunk`] (**synchron**) – auf jedem IR-Stream-Ereignis; Gleitfenster über
//!   [`ChunkCtx::state`] und [`crate::TextWindow`], Zurückhalten über [`ChunkFlow::Drop`] +
//!   [`ChunkCtx::emit`], Ausleeren in [`Plugin::on_stream_end`].
//!
//! Synchrone Plugins geben [`Hook::Ready`] zurück und kosten keine Allokation; nur
//! asynchrone (z. B. Webhook) liefern eine geboxte Future ([`Hook::Pending`]), für die das
//! Zeitlimit der Bindung gilt.

use std::any::Any;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use aigw_core::ir::{ApiError, ChatRequest, ChatResponse, StreamEvent};
use aigw_core::snapshot::KeyCtx;
use aigw_core::wire::WireFormat;
use http::{HeaderMap, HeaderName, HeaderValue};

/// Metadaten, die Plugins zwischen den Phasen einer Anfrage weitergeben (z. B. Zuordnungen
/// einer Pseudonymisierung). JSON, damit Webhook- und spätere WASM-Plugins sie lesen können.
pub type Meta = serde_json::Map<String, serde_json::Value>;

/// Geboxte, `Send`-fähige Future mit Lebensdauer `'a`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Fehler eines Plugins zur Laufzeit (Zeitüberschreitung, Dienst nicht erreichbar …).
///
/// Was daraus wird, entscheidet der Fehlermodus der Bindung ([`crate::FailMode`]):
/// `open` = protokollieren und weiter, `closed` = Anfrage mit `500 plugin_error` ablehnen
/// (bzw. Stream mit Fehlerereignis beenden).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginError(pub String);

impl PluginError {
    /// Neuer Fehler.
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PluginError {}

/// Ergebnis von [`Plugin::on_request`] bzw. [`Plugin::on_response`].
#[derive(Debug, Clone, PartialEq)]
pub enum Flow {
    /// Weiter mit dem nächsten Plugin.
    Continue,
    /// Anfrage-Phase: direkt mit dieser Antwort antworten (kein Upstream-Aufruf, keine
    /// Antwort-Plugins). Antwort-Phase: die Antwort durch diese ersetzen.
    Respond(Box<ChatResponse>),
    /// Mit diesem Fehler ablehnen (Status aus [`ApiError::kind`], Format des Eingangs).
    Reject(ApiError),
}

/// Ergebnis von [`Plugin::on_chunk`].
#[derive(Debug, Clone, PartialEq)]
pub enum ChunkFlow {
    /// Ereignis (ggf. verändert) weitergeben.
    Continue,
    /// Ereignis verwerfen (z. B. zurückhalten; später über [`ChunkCtx::emit`] ausgeben).
    Drop,
    /// Stream mit diesem Fehler beenden ([`StreamEvent::Error`], Upstream wird abgebrochen).
    Reject(ApiError),
}

/// Ergebnis eines Hakens: sofort fertig (synchron, ohne Allokation) oder eine Future.
pub enum Hook<'a, T> {
    /// Sofort fertig.
    Ready(Result<T, PluginError>),
    /// Asynchron; läuft unter dem Zeitlimit der Bindung.
    Pending(BoxFuture<'a, Result<T, PluginError>>),
}

impl<T> Hook<'_, T> {
    /// Sofort fertig mit `value`.
    pub fn ok(value: T) -> Self {
        Self::Ready(Ok(value))
    }
}

impl<'a, T> Hook<'a, T> {
    /// Asynchroner Haken aus einer Future.
    pub fn future(fut: impl Future<Output = Result<T, PluginError>> + Send + 'a) -> Self {
        Self::Pending(Box::pin(fut))
    }
}

impl<T> fmt::Debug for Hook<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ready(_) => f.write_str("Hook::Ready"),
            Self::Pending(_) => f.write_str("Hook::Pending"),
        }
    }
}

/// Phasen, an denen ein Plugin teilnimmt. Die Kette ruft nur die angemeldeten Haken auf.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Phases {
    /// [`Plugin::on_request`].
    pub request: bool,
    /// [`Plugin::on_response`].
    pub response: bool,
    /// [`Plugin::on_chunk`] / [`Plugin::on_stream_end`].
    pub chunk: bool,
}

impl Phases {
    /// Nur Anfrage-Phase.
    pub const REQUEST: Self = Self {
        request: true,
        response: false,
        chunk: false,
    };
    /// Alle Phasen.
    pub const ALL: Self = Self {
        request: true,
        response: true,
        chunk: true,
    };
    /// Nur Ausgabe (Antwort und Stream).
    pub const OUTPUT: Self = Self {
        request: false,
        response: true,
        chunk: true,
    };
}

/// Unveränderliche Angaben zur Anfrage, die alle Phasen sehen.
#[derive(Debug, Clone)]
pub struct CallInfo {
    /// Request-ID (`x-aigw-request-id`).
    pub request_id: String,
    /// Eingangsformat.
    pub ingress: WireFormat,
    /// Geprüfter Key samt Team und Organisation (`None` ohne Schlüsselprüfung).
    pub key: Option<Arc<KeyCtx>>,
    /// Kanonischer Name der Modellgruppe.
    pub model_group: Option<String>,
    /// Hat der Client gestreamt angefragt?
    pub stream: bool,
}

impl CallInfo {
    /// Angaben ohne Key und Modellgruppe (Tests, Benchmarks).
    pub fn new(request_id: impl Into<String>, ingress: WireFormat) -> Self {
        Self {
            request_id: request_id.into(),
            ingress,
            key: None,
            model_group: None,
            stream: false,
        }
    }
}

/// Änderungen an den Antwort-Headern an den Client (gesetzt bzw. entfernt), angewendet ganz
/// am Ende – auch auf Fehlerantworten.
#[derive(Debug, Clone, Default)]
pub struct ResponseHeaders {
    /// Zu setzende Header (überschreiben gleichnamige).
    pub set: HeaderMap,
    /// Zu entfernende Header.
    pub remove: Vec<HeaderName>,
}

impl ResponseHeaders {
    /// Keine Änderungen?
    pub fn is_empty(&self) -> bool {
        self.set.is_empty() && self.remove.is_empty()
    }

    /// Wendet die Änderungen auf `headers` an (erst entfernen, dann setzen).
    /// Geschützte Header ([`crate::headers::is_protected_response`]) bleiben unberührt.
    pub fn apply(&self, headers: &mut HeaderMap) {
        for name in &self.remove {
            if !crate::headers::is_protected_response(name) {
                headers.remove(name);
            }
        }
        for (name, value) in &self.set {
            if !crate::headers::is_protected_response(name) {
                headers.insert(name.clone(), value.clone());
            }
        }
    }

    /// Setzt einen Header.
    pub fn insert(&mut self, name: HeaderName, value: HeaderValue) {
        self.set.insert(name, value);
    }
}

/// Kontext der Anfrage-Phase.
#[derive(Debug)]
pub struct ReqCtx<'a> {
    /// Angaben zur Anfrage.
    pub info: &'a CallInfo,
    /// Anfrage (IR); Änderungen gehen an den Upstream. `model` und `stream` dürfen nicht
    /// verändert werden (die Kette setzt sie zurück und behandelt das als Plugin-Fehler).
    pub request: ChatRequest,
    /// Header der Client-Anfrage (nur lesen). Enthält auch den Gateway-Key
    /// (`authorization`, `api-key`, `x-api-key`) – nie weitergeben oder protokollieren.
    pub client_headers: &'a HeaderMap,
    /// Zusätzliche Header an den Upstream (Auth-/Hop-Header sind gesperrt, siehe
    /// [`crate::headers::is_forbidden_upstream`]).
    pub upstream_headers: HeaderMap,
    /// Änderungen an den Antwort-Headern.
    pub response_headers: ResponseHeaders,
    /// Metadaten für spätere Phasen.
    pub meta: Meta,
}

/// Kontext der Antwort-Phase.
#[derive(Debug)]
pub struct RespCtx<'a> {
    /// Angaben zur Anfrage.
    pub info: &'a CallInfo,
    /// Antwort (IR) vor dem Encoder.
    pub response: ChatResponse,
    /// Änderungen an den Antwort-Headern.
    pub response_headers: &'a mut ResponseHeaders,
    /// Metadaten aus der Anfrage-Phase.
    pub meta: &'a mut Meta,
}

/// Kontext der Stream-Phase (je Plugin und Stream).
pub struct ChunkCtx<'a> {
    /// Angaben zur Anfrage.
    pub info: &'a CallInfo,
    /// Metadaten aus der Anfrage-Phase.
    pub meta: &'a mut Meta,
    pub(crate) state: &'a mut Option<Box<dyn Any + Send>>,
    pub(crate) emitted: &'a mut Vec<StreamEvent>,
}

impl fmt::Debug for ChunkCtx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChunkCtx")
            .field("info", &self.info)
            .field("emitted", &self.emitted.len())
            .finish_non_exhaustive()
    }
}

impl ChunkCtx<'_> {
    /// Zustand dieses Plugins für diesen Stream (z. B. ein [`crate::TextWindow`]); wird beim
    /// ersten Zugriff mit `T::default()` angelegt. Wechselt ein Plugin den Typ, beginnt der
    /// Zustand neu.
    pub fn state<T: Any + Send + Default>(&mut self) -> &mut T {
        if !self.state.as_ref().is_some_and(|s| s.is::<T>()) {
            *self.state = Some(Box::new(T::default()));
        }
        match self.state.as_mut().and_then(|s| s.downcast_mut::<T>()) {
            Some(s) => s,
            // Unerreichbar: oben gerade mit `T` belegt.
            None => unreachable_state(),
        }
    }

    /// Gibt ein zusätzliches Ereignis aus – **vor** dem aktuellen (falls dieses
    /// weitergegeben wird). Es durchläuft die nachfolgenden Plugins der Kette.
    pub fn emit(&mut self, ev: StreamEvent) {
        self.emitted.push(ev);
    }
}

#[cold]
fn unreachable_state() -> ! {
    #[allow(clippy::panic)]
    {
        panic!("ChunkCtx::state: Zustand fehlt")
    }
}

/// Ein Plugin der Kette (Architektur §9).
///
/// Instanzen werden je Bindung beim Snapshot-Aufbau mit validierter Konfiguration gebaut
/// ([`crate::PluginFactory`]) und von allen Anfragen geteilt; Zustand je Stream liegt in
/// [`ChunkCtx::state`], Zustand je Anfrage in [`Meta`].
///
/// Ausgabe-Plugins, die Inhalte prüfen oder verändern, implementieren **beide**
/// Ausgabehaken: [`Plugin::on_response`] (JSON und gepufferter Modus) und
/// [`Plugin::on_chunk`] (Live-Stream). Je Antwort wird genau einer davon aufgerufen.
///
/// `Any` als Obertrait erlaubt der Pipeline, Richtlinien-Plugins ohne Haken (z. B.
/// [`crate::builtin::cache`]) aus der Kette zu lesen.
pub trait Plugin: Any + Send + Sync + 'static {
    /// Name des Plugins (wie in der Bindung, z. B. `header`).
    fn name(&self) -> &'static str;

    /// Phasen, an denen das Plugin teilnimmt.
    fn phases(&self) -> Phases;

    /// Braucht das Plugin die vollständige Ausgabe (Prüfung auf ganzer Antwort)? Dann
    /// werden gestreamte Anfragen auf der Route gepuffert: Upstream nicht gestreamt,
    /// [`Plugin::on_response`] auf der ganzen Antwort, danach als Stream an den Client
    /// (Latenzkosten: TTFT = Gesamtdauer; Header `x-aigw-buffered`).
    fn needs_buffered_output(&self) -> bool {
        false
    }

    /// Anfrage-Phase.
    fn on_request<'a>(&'a self, _ctx: &'a mut ReqCtx<'_>) -> Hook<'a, Flow> {
        Hook::ok(Flow::Continue)
    }

    /// Antwort-Phase (nicht gestreamt bzw. gepuffert).
    fn on_response<'a>(&'a self, _ctx: &'a mut RespCtx<'_>) -> Hook<'a, Flow> {
        Hook::ok(Flow::Continue)
    }

    /// Stream-Phase: ein IR-Ereignis, synchron (kein `await` im Chunk-Pfad).
    fn on_chunk(
        &self,
        _ctx: &mut ChunkCtx<'_>,
        _ev: &mut StreamEvent,
    ) -> Result<ChunkFlow, PluginError> {
        Ok(ChunkFlow::Continue)
    }

    /// Ende des Upstream-Streams: Zurückgehaltenes über [`ChunkCtx::emit`] ausgeben.
    /// `ChunkFlow::Reject` beendet den Stream mit Fehler; `Drop` wirkt wie `Continue`.
    fn on_stream_end(&self, _ctx: &mut ChunkCtx<'_>) -> Result<ChunkFlow, PluginError> {
        Ok(ChunkFlow::Continue)
    }
}
