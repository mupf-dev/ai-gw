// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Zähler-Backends (E4).
//!
//! [`CounterBackend`] abstrahiert Zählerstände für Token-/Request-Limits und Budgets.
//! Vorgesehen sind `local` (im Prozess), `postgres-sync` (lokal zählen, alle 1–2 s mit
//! Postgres abgleichen) und `redis` (exakt, verteilt). Umgesetzt ist bisher nur [`LocalCounters`].

pub mod local;

pub use local::LocalCounters;

/// Schlüssel eines Zählers, z. B. `key:<id>:tokens:minute`.
pub type CounterKey = str;

/// Abstraktion über Zählerstände.
///
/// Die Methoden sind bewusst synchron und billig: Im Hot Path wird lokal gezählt; verteilte
/// Backends gleichen asynchron im Hintergrund ab bzw. bekommen eigene async-Varianten,
/// sobald `redis` umgesetzt wird.
pub trait CounterBackend: Send + Sync + std::fmt::Debug {
    /// Erhöht den Zähler um `delta` und liefert den neuen Stand.
    fn incr(&self, key: &CounterKey, delta: u64) -> aigw_core::Result<u64>;
    /// Liefert den aktuellen Stand (0, wenn unbekannt).
    fn get(&self, key: &CounterKey) -> aigw_core::Result<u64>;
    /// Setzt den Zähler zurück (z. B. beim Fensterwechsel).
    fn reset(&self, key: &CounterKey) -> aigw_core::Result<()>;
}
