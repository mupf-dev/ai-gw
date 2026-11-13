// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Plugin-Kette von ai-gw (AIGW-36, Architektur §9, Doku `docs/konzept/12-plugins.md`).
//!
//! - [`Plugin`]: drei Haken – [`Plugin::on_request`] (Continue | Respond | Reject),
//!   [`Plugin::on_response`] und synchron [`Plugin::on_chunk`] auf IR-Stream-Ereignissen;
//!   [`Plugin::needs_buffered_output`] schaltet gestreamte Anfragen in den gepufferten Modus.
//! - [`Chain`]: unveränderliche, geordnete Kette mit Fehlermodus und Zeitlimit je Bindung
//!   ([`FailMode`]); [`ChunkRunner`]/[`PluginStream`] für den Stream-Pfad. Antwort- und
//!   Stream-Haken laufen in umgekehrter Reihenfolge (Zwiebelprinzip, F167b);
//!   Guardrail-Ablehnungen der Ausgabe ([`Flow::Block`]) enden je [`OutputRejectMode`] mit
//!   `content_filter`/`refusal` (F160). Fehlermodus-Zähler: [`PluginMetrics`] (F148).
//! - [`PluginIndex`]/[`PluginChains`]: Ketten je (Org, Team, Key, Modellgruppe), beim
//!   Snapshot-Aufbau vorberechnet, Hot Reload über [`PluginChains::spawn`].
//! - [`Registry`]/[`PluginFactory`]: Plugin-Name → validierte Instanz; eingebaut sind
//!   [`builtin::header`], [`builtin::request_transform`] sowie die Guardrails
//!   [`builtin::pii`], [`builtin::prompt_filter`] und [`builtin::content_safety`]
//!   (AIGW-37/38, `docs/konzept/13-guardrails.md`); externe Dienste über [`PluginHost`].

pub mod binding;
pub mod builtin;
mod chain;
pub mod headers;
mod host;
mod index;
mod metrics;
mod plugin;
mod registry;
mod stream;

pub use binding::{Control, FailMode, Inherit, OutputRejectMode};
pub use chain::{Chain, ChunkMode, ChunkRunner, Entry, content_filtered};
pub use host::{HttpCall, HttpReply, NoHost, PluginHost};
pub use index::{ChainsTask, IndexOptions, PluginChains, PluginIndex};
pub use metrics::PluginMetrics;
pub use plugin::{
    BoxFuture, CallInfo, ChunkCtx, ChunkFlow, Flow, Hook, INTERNAL_META_PREFIX, Meta, MetaExt,
    Phases, Plugin, PluginError, ReqCtx, RespCtx, ResponseHeaders,
};
pub use registry::{DEFAULT_TIMEOUT, EffectiveControl, PluginFactory, Registry};
pub use stream::{PluginStream, TextWindow, response_events};

/// Antwort-Header: Antwort wurde wegen Ausgabe-Plugins gepuffert (Wert `plugins`).
pub const BUFFERED_HEADER: &str = "x-aigw-buffered";

/// Antwort-Header: ein Guardrail hat Anfrage oder Antwort blockiert (Wert: Plugin-Name, z. B.
/// `pii`, `content-safety`; F160). Im Live-Stream sind die Header schon gesendet – dort
/// zeigt nur `finish_reason: content_filter` bzw. `stop_reason: refusal` die Ablehnung an.
pub const GUARDRAIL_HEADER: &str = "x-aigw-guardrail";
