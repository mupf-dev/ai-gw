// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Gemeinsamer Baustein für alle ausgehenden HTTP-Verbindungen von ai-gw (AIGW-72, F63/F48).
//!
//! Anbieter-Adapter, Webhooks, MCP-Client, OIDC-Login, externe Guardrail-Dienste und der
//! OTLP-Export nutzen denselben [`OutboundClient`]:
//!
//! - **Eigene Namensauflösung mit SSRF-Filter** ([`AddressGuard`]): Jede aufgelöste Adresse
//!   wird mit [`UpstreamPolicy::resolved_ip_allowed`] geprüft, bevor eine Verbindung entsteht;
//!   bleibt keine übrig, scheitert der Aufruf mit [`OutboundError::Blocked`]. Damit fällt auch
//!   ein öffentlicher Name auf, der (per DNS-Rebinding) auf eine private Adresse zeigt. IP-Literale
//!   in der URL umgehen die Auflösung und werden direkt geprüft.
//! - **Keine Weiterleitungen:** 3xx-Antworten werden nie verfolgt; [`OutboundClient::fetch`]
//!   wertet sie standardmäßig als Fehler ([`Redirects`]).
//! - **Grenzen:** Verbindungsaufbau-Zeitlimit, Gesamt-Zeitlimit und Höchstgröße der Antwort
//!   ([`FetchLimits`]).
//! - **TLS** nur über rustls mit aws-lc-rs (TLS 1.2/1.3), Vertrauensanker nach
//!   `upstream.roots` (`webpki`|`system`|`both`) plus zusätzliche CA-Dateien ([`TlsOptions`]).
//! - HTTP/2 (ALPN) und HTTP/1.1, Keep-Alive-Pool je Endpunkt, TCP-Keep-Alive, `TCP_NODELAY`.
//!
//! Für Ziele aus der Startkonfiguration (OIDC-IdP, OTLP-Collector) gilt
//! [`AddressGuard::Trusted`]: Sie stammen vom Betreiber, nicht aus der Admin-API.
//!
//! Warum ein eigenes Crate (statt eines Moduls in `aigw-core`): `aigw-core` bleibt frei von
//! hyper/rustls (es wird auch von reinen Logik-Crates wie `aigw-billing` genutzt), und die
//! TLS-/Pool-Logik existiert genau einmal statt in jedem Client-Crate.

mod client;
mod error;
mod resolve;
mod tls;

pub use aigw_core::config::{RootsKind, UpstreamConfig};
pub use aigw_core::upstream::UpstreamPolicy;
pub use client::{FetchLimits, OutboundBuilder, OutboundClient, OutboundOptions, Redirects, Reply};
pub use error::OutboundError;
pub use resolve::{AddressGuard, Resolve, ResolveFuture, StaticResolver, SystemResolver};
pub use tls::{TlsOptions, default_tls, install_default_tls};

/// `User-Agent` ausgehender Aufrufe ohne eigenen Wert.
pub const USER_AGENT: &str = concat!("ai-gw/", env!("CARGO_PKG_VERSION"));
