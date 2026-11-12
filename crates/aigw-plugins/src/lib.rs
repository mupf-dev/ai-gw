// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Plugin-Kette (Stub).
//!
//! Plugins greifen an drei Stellen ein: vor dem Upstream-Aufruf ([`Plugin::pre_request`]),
//! nach der Antwort und pro Stream-Chunk. Hier ist zunächst nur der Pre-Request-Haken
//! modelliert; Kontext und Ergebnis werden mit dem Architekturkonzept ausgebaut.

use std::sync::Arc;

/// Kontext eines Requests, den Plugins lesen und verändern dürfen (Platzhalter).
#[derive(Debug, Default, Clone)]
pub struct RequestContext {
    /// Angefragtes Modell.
    pub model: String,
    /// Freie Metadaten, z. B. für Logging.
    pub tags: Vec<String>,
}

/// Ergebnis eines Plugin-Aufrufs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Weiter mit dem nächsten Plugin.
    Continue,
    /// Request ablehnen (HTTP-Status, Begründung).
    Reject {
        /// HTTP-Statuscode.
        status: u16,
        /// Begründung für den Client.
        reason: String,
    },
}

/// Ein Plugin der Kette.
pub trait Plugin: Send + Sync + std::fmt::Debug {
    /// Name des Plugins (für Konfiguration und Logs).
    fn name(&self) -> &str;
    /// Wird vor dem Upstream-Aufruf ausgeführt.
    fn pre_request(&self, ctx: &mut RequestContext) -> Outcome;
}

/// Geordnete Folge von Plugins.
#[derive(Debug, Default, Clone)]
pub struct Chain {
    plugins: Vec<Arc<dyn Plugin>>,
}

impl Chain {
    /// Hängt ein Plugin an.
    #[must_use]
    pub fn with(mut self, plugin: Arc<dyn Plugin>) -> Self {
        self.plugins.push(plugin);
        self
    }

    /// Führt alle Pre-Request-Haken aus; bricht beim ersten [`Outcome::Reject`] ab.
    pub fn run_pre_request(&self, ctx: &mut RequestContext) -> Outcome {
        for p in &self.plugins {
            let outcome = p.pre_request(ctx);
            if outcome != Outcome::Continue {
                return outcome;
            }
        }
        Outcome::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Tag;
    impl Plugin for Tag {
        fn name(&self) -> &str {
            "tag"
        }
        fn pre_request(&self, ctx: &mut RequestContext) -> Outcome {
            ctx.tags.push("seen".into());
            Outcome::Continue
        }
    }

    #[derive(Debug)]
    struct Deny;
    impl Plugin for Deny {
        fn name(&self) -> &str {
            "deny"
        }
        fn pre_request(&self, _ctx: &mut RequestContext) -> Outcome {
            Outcome::Reject {
                status: 403,
                reason: "nein".into(),
            }
        }
    }

    #[test]
    fn chain_stops_at_first_reject() {
        let chain = Chain::default()
            .with(Arc::new(Tag))
            .with(Arc::new(Deny))
            .with(Arc::new(Tag));
        let mut ctx = RequestContext::default();
        let out = chain.run_pre_request(&mut ctx);
        assert!(matches!(out, Outcome::Reject { status: 403, .. }));
        assert_eq!(ctx.tags, vec!["seen".to_owned()]);
    }
}
