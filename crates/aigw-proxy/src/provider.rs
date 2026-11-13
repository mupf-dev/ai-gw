// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Einfache Anbieter-Schnittstelle der Datenebene (Platzhalter bis AIGW-15).
//!
//! Der endgültige Adapter-Trait (Architektur §5: `build_request`, `parse_response`,
//! `stream_decoder`, `classify_error`) kommt mit den echten Anbietern; dieser Trait kapselt
//! nur „IR rein, IR raus“ und lässt sich darüber implementieren.

use std::future::Future;
use std::pin::Pin;

use aigw_core::ir::{ApiError, ChatRequest, ChatResponse, ModelInfo, StreamEvent};
use futures_util::Stream;

/// Geboxte Future (`Send`).
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Strom von IR-Ereignissen einer gestreamten Antwort.
pub type EventStream = Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>>;

/// Anbieter für Chat-Anfragen.
pub trait ChatProvider: Send + Sync + 'static {
    /// Verfügbare Modelle.
    fn models(&self) -> Vec<ModelInfo>;

    /// Nicht gestreamte Antwort.
    fn chat(&self, req: ChatRequest) -> BoxFuture<Result<ChatResponse, ApiError>>;

    /// Gestreamte Antwort. Fehler vor dem ersten Ereignis kommen als `Err` (der Client erhält
    /// dann eine normale Fehlerantwort), spätere als [`StreamEvent::Error`].
    fn chat_stream(&self, req: ChatRequest) -> BoxFuture<Result<EventStream, ApiError>>;
}
