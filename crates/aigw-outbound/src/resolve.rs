// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Namensauflösung mit SSRF-Filter.
//!
//! hyper-util ruft den Resolver für jeden neuen Verbindungsaufbau (nicht für wiederverwendete
//! Pool-Verbindungen). Geprüft wird also genau die Adresse, mit der verbunden wird – ein
//! Name, der zwischen Konfiguration und Verbindungsaufbau auf eine private Adresse umgestellt
//! wird (DNS-Rebinding), fällt hier auf.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use aigw_core::upstream::UpstreamPolicy;
use hyper_util::client::legacy::connect::dns::Name;

/// Ergebnis einer Namensauflösung.
pub type ResolveFuture = Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send>>;

/// Namensauflösung (austauschbar, z. B. für Tests mit festen Antworten).
pub trait Resolve: Send + Sync + fmt::Debug {
    /// Löst `host` (ohne Port, ohne Klammern) zu IP-Adressen auf.
    fn resolve(&self, host: &str) -> ResolveFuture;
}

/// Resolver des Betriebssystems (`getaddrinfo` über Tokio).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemResolver;

impl Resolve for SystemResolver {
    fn resolve(&self, host: &str) -> ResolveFuture {
        let host = host.to_owned();
        Box::pin(async move {
            Ok(tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .map(|a| a.ip())
                .collect())
        })
    }
}

/// Resolver mit festen Einträgen (Tests: „öffentlicher“ Name → private Adresse); unbekannte
/// Namen scheitern.
#[derive(Debug, Clone, Default)]
pub struct StaticResolver {
    entries: HashMap<String, Vec<IpAddr>>,
}

impl StaticResolver {
    /// Leerer Resolver.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fügt einen Eintrag hinzu.
    #[must_use]
    pub fn with(mut self, host: &str, ips: impl IntoIterator<Item = IpAddr>) -> Self {
        self.entries
            .insert(host.to_ascii_lowercase(), ips.into_iter().collect());
        self
    }
}

impl Resolve for StaticResolver {
    fn resolve(&self, host: &str) -> ResolveFuture {
        let res = self
            .entries
            .get(&host.to_ascii_lowercase())
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{host}: unbekannt")));
        Box::pin(async move { res })
    }
}

/// Welche Zieladressen ein Client ansprechen darf.
#[derive(Clone)]
pub enum AddressGuard {
    /// SSRF-Regeln aus `admin.allow_*_upstreams` (Anbieter, Webhooks, MCP, Guardrail-Dienste).
    Policy(Arc<UpstreamPolicy>),
    /// Ohne Adressfilter: nur für Ziele aus der Startkonfiguration des Betreibers (OIDC-IdP,
    /// OTLP-Collector) sowie Tests und Benchmarks.
    Trusted,
}

impl fmt::Debug for AddressGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Policy(p) => f.debug_tuple("Policy").field(p).finish(),
            Self::Trusted => f.write_str("Trusted"),
        }
    }
}

impl AddressGuard {
    /// Regeln `policy`.
    pub fn policy(policy: UpstreamPolicy) -> Self {
        Self::Policy(Arc::new(policy))
    }

    /// Strenge Standardregeln (keine privaten Ziele).
    pub fn strict() -> Self {
        Self::policy(UpstreamPolicy::strict())
    }

    /// Regeln, falls vorhanden.
    pub fn upstream_policy(&self) -> Option<&UpstreamPolicy> {
        match self {
            Self::Policy(p) => Some(p),
            Self::Trusted => None,
        }
    }

    /// Darf mit `ip` verbunden werden, die über `host` aufgelöst wurde (bei IP-Literalen ist
    /// `host` die Adresse selbst)?
    pub fn allows(&self, host: &str, ip: IpAddr) -> bool {
        match self {
            Self::Policy(p) => p.resolved_ip_allowed(host, ip),
            Self::Trusted => true,
        }
    }
}

/// SSRF-Sperre (Ursache im Fehler des Verbindungsaufbaus; siehe `OutboundError::from_client`).
#[derive(Debug, thiserror::Error)]
#[error(
    "{host} löst nur auf nicht erlaubte Adressen auf ({addrs}) – SSRF-Schutz, Freischaltung über \
     admin.allow_private_upstreams"
)]
pub(crate) struct Blocked {
    pub(crate) host: String,
    pub(crate) addrs: String,
}

/// Resolver für hyper-util mit Adressfilter.
#[derive(Clone)]
pub(crate) struct GuardedResolver {
    pub(crate) guard: AddressGuard,
    pub(crate) resolver: Arc<dyn Resolve>,
}

impl tower_service::Service<Name> for GuardedResolver {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, io::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        let guard = self.guard.clone();
        let host = name.as_str().to_owned();
        let lookup = self.resolver.resolve(&host);
        Box::pin(async move {
            let all = lookup.await?;
            let allowed: Vec<SocketAddr> = all
                .iter()
                .copied()
                .filter(|ip| guard.allows(&host, *ip))
                .map(|ip| SocketAddr::new(ip, 0))
                .collect();
            if allowed.is_empty() {
                let addrs = all
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                tracing::warn!(host = %host, addrs = %addrs, "ausgehende Verbindung durch SSRF-Regeln gesperrt");
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    Blocked { host, addrs },
                ));
            }
            Ok(allowed.into_iter())
        })
    }
}
