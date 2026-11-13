// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Eingebaute Plugins.

pub mod header;
pub mod request_transform;

use std::sync::Arc;

use aigw_core::ir::{ApiError, ErrorKind};

use crate::plugin::{Flow, Hook, Phases, Plugin, ReqCtx};

/// Platzhalter für eine ungültige Bindung mit `failMode: closed`: lehnt jede Anfrage der
/// betroffenen Ketten mit `500 plugin_config_invalid` ab.
#[derive(Debug)]
pub(crate) struct Broken {
    binding: String,
}

impl Broken {
    pub(crate) fn plugin(binding: &str) -> Arc<dyn Plugin> {
        Arc::new(Self {
            binding: binding.to_owned(),
        })
    }
}

impl Plugin for Broken {
    fn name(&self) -> &'static str {
        "invalid-binding"
    }

    fn phases(&self) -> Phases {
        Phases::REQUEST
    }

    fn on_request<'a>(&'a self, _ctx: &'a mut ReqCtx<'_>) -> Hook<'a, Flow> {
        Hook::ok(Flow::Reject(
            ApiError::new(
                ErrorKind::Internal,
                format!(
                    "Plugin-Bindung {:?} ist ungültig konfiguriert",
                    self.binding
                ),
            )
            .with_code("plugin_config_invalid"),
        ))
    }
}
