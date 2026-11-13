// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Ausgehende Webhooks von ai-gw (AIGW-61).
//!
//! Eigene kleine Crate, weil zwei Stellen dieselbe Technik brauchen – das Plugin `webhook`
//! (`aigw-plugins`, synchron im Hot Path) und Ereignis-Webhooks (`aigw-billing`, asynchron)
//! – und weder `aigw-core` (keine HTTP-Abhängigkeiten) noch `aigw-providers` (Anbieter-Adapter)
//! der richtige Ort ist:
//!
//! - [`WebhookClient`]: hyper-util + rustls (aws-lc-rs), Keep-Alive-Pool, **SSRF-Schutz** in
//!   zwei Stufen: URL-Prüfung nach [`aigw_core::upstream::UpstreamPolicy::check_endpoint`]
//!   (Konfiguration) und Prüfung jeder aufgelösten IP-Adresse beim Verbindungsaufbau
//!   ([`aigw_core::upstream::UpstreamPolicy::ip_allowed`], schützt vor DNS-Rebinding).
//!   Keine Weiterleitungen, Größenlimit für Antworten, mTLS als Erweiterungspunkt
//!   ([`ClientOptions::tls`]).
//! - [`sign`]: HMAC-SHA256-Signatur `x-aigw-signature: t=<ts>,v1=<hex>` und
//!   Referenz-Verifier mit Replay-Zeitfenster.
//! - [`SecretResolver`]: Geheimnis aus `ProviderCredential` (über den Snapshot, Rotation ohne
//!   Neustart), Umgebungsvariable oder Datei – nie Klartext in der Konfiguration.
//! - [`EventDispatcher`]: Ereignis-Webhooks mit Puffer, Wiederholung mit Backoff,
//!   Dead-Letter-Zähler und Deduplizierung im Cluster ([`EventDedup`], Tabelle
//!   `event_dedup` in `aigw-store`).

mod client;
mod events;
mod secret;
pub mod sign;

pub use client::{
    ClientOptions, DELIVERY_HEADER, REQUEST_ID_HEADER, WebhookClient, WebhookError, WebhookResponse,
};
pub use events::{
    ATTEMPT_HEADER, Dedup, DispatcherOptions, EVENT_TYPE_HEADER, Event, EventDedup,
    EventDispatcher, EventMetrics, EventTarget, EventTask,
};
pub use secret::{CredentialSource, SecretResolver};
pub use sign::{DEFAULT_TOLERANCE, SIGNATURE_HEADER};
