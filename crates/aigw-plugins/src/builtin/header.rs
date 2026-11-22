// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Plugin `header`: Header an den Upstream und an den Client setzen oder entfernen;
//! Allowlist für Client-Header, die an den Upstream durchgereicht werden.
//!
//! ```yaml
//! plugin: header
//! config:
//!   request:                      # an den Upstream
//!     forward: [x-request-source] # Allowlist: Client-Header durchreichen
//!     set: {x-tenant: acme}
//!     remove: [x-debug]           # auch von früheren Plugins gesetzte
//!   response:                     # an den Client
//!     set: {x-policy: strict}
//!     remove: [x-aigw-deployment]
//! ```
//!
//! Zugangsdaten-, Hop-by-Hop- und Signatur-Header sind für den Upstream gesperrt
//! ([`crate::headers::is_forbidden_upstream`]), Rahmen-Header und `x-aigw-request-id` der
//! Antwort sind geschützt ([`crate::headers::is_protected_response`]); beides wird schon beim
//! Snapshot-Aufbau abgewiesen.

use std::collections::BTreeMap;
use std::sync::Arc;

use http::{HeaderName, HeaderValue};
use serde::Deserialize;
use serde_json::Value;

use crate::binding::FailMode;
use crate::headers::{is_forbidden_upstream, is_protected_response};
use crate::plugin::{Flow, Hook, Phases, Plugin, ReqCtx};
use crate::registry::PluginFactory;

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Config {
    #[serde(default)]
    request: RequestConfig,
    #[serde(default)]
    response: ResponseConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RequestConfig {
    #[serde(default)]
    forward: Vec<String>,
    #[serde(default)]
    set: BTreeMap<String, String>,
    #[serde(default)]
    remove: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResponseConfig {
    #[serde(default)]
    set: BTreeMap<String, String>,
    #[serde(default)]
    remove: Vec<String>,
}

/// Plugin `header`.
#[derive(Debug, Default)]
pub struct Header {
    forward: Vec<HeaderName>,
    upstream_set: Vec<(HeaderName, HeaderValue)>,
    upstream_remove: Vec<HeaderName>,
    response_set: Vec<(HeaderName, HeaderValue)>,
    response_remove: Vec<HeaderName>,
}

fn name(n: &str) -> Result<HeaderName, String> {
    HeaderName::from_bytes(n.as_bytes()).map_err(|_| format!("ungültiger Header-Name {n:?}"))
}

fn value(n: &str, v: &str) -> Result<HeaderValue, String> {
    HeaderValue::from_str(v).map_err(|_| format!("ungültiger Wert für Header {n:?}"))
}

fn upstream(n: &str) -> Result<HeaderName, String> {
    let h = name(n)?;
    if is_forbidden_upstream(&h) {
        return Err(format!(
            "Header {n:?} darf nicht an den Upstream gesetzt oder durchgereicht werden"
        ));
    }
    Ok(h)
}

fn response(n: &str) -> Result<HeaderName, String> {
    let h = name(n)?;
    if is_protected_response(&h) {
        return Err(format!(
            "Antwort-Header {n:?} ist geschützt und kann nicht verändert werden"
        ));
    }
    Ok(h)
}

impl Header {
    fn from_config(c: Config) -> Result<Self, String> {
        let pairs = |m: BTreeMap<String, String>,
                     check: fn(&str) -> Result<HeaderName, String>|
         -> Result<Vec<(HeaderName, HeaderValue)>, String> {
            m.iter()
                .map(|(k, v)| Ok((check(k)?, value(k, v)?)))
                .collect()
        };
        Ok(Self {
            forward: c
                .request
                .forward
                .iter()
                .map(|n| upstream(n))
                .collect::<Result<_, _>>()?,
            upstream_set: pairs(c.request.set, upstream)?,
            upstream_remove: c
                .request
                .remove
                .iter()
                .map(|n| name(n))
                .collect::<Result<_, _>>()?,
            response_set: pairs(c.response.set, response)?,
            response_remove: c
                .response
                .remove
                .iter()
                .map(|n| response(n))
                .collect::<Result<_, _>>()?,
        })
    }
}

impl Plugin for Header {
    fn name(&self) -> &'static str {
        "header"
    }

    fn phases(&self) -> Phases {
        Phases::REQUEST
    }

    fn on_request<'a>(&'a self, ctx: &'a mut ReqCtx<'_>) -> Hook<'a, Flow> {
        for n in &self.forward {
            let mut first = true;
            for v in ctx.client_headers.get_all(n) {
                if first {
                    ctx.upstream_headers.insert(n.clone(), v.clone());
                    first = false;
                } else {
                    ctx.upstream_headers.append(n.clone(), v.clone());
                }
            }
        }
        for n in &self.upstream_remove {
            ctx.upstream_headers.remove(n);
        }
        for (n, v) in &self.upstream_set {
            ctx.upstream_headers.insert(n.clone(), v.clone());
        }
        for n in &self.response_remove {
            ctx.response_headers.set.remove(n);
            ctx.response_headers.remove.push(n.clone());
        }
        for (n, v) in &self.response_set {
            ctx.response_headers.insert(n.clone(), v.clone());
        }
        Hook::ok(Flow::Continue)
    }
}

/// Fabrik von [`Header`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Factory;

impl PluginFactory for Factory {
    fn name(&self) -> &'static str {
        "header"
    }

    fn build(&self, config: &Value) -> Result<Arc<dyn Plugin>, String> {
        let c: Config = serde_json::from_value(config.clone()).map_err(|e| e.to_string())?;
        Ok(Arc::new(Header::from_config(c)?))
    }

    fn default_fail_mode(&self) -> FailMode {
        FailMode::Closed
    }
}
