// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! MCP-Konstanten: Protokollversionen, Header, Methoden.

/// Neueste unterstützte Protokollversion (Streamable HTTP).
pub const LATEST_VERSION: &str = "2025-11-25";

/// Unterstützte Protokollversionen, neueste zuerst. `2024-11-05` ist die Version des älteren
/// HTTP+SSE-Transports (nur als Client zu Upstreams sinnvoll, wird aber auch Clients
/// angeboten, die sie verlangen).
pub const SUPPORTED_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// Annahme ohne `MCP-Protocol-Version`-Header (Spezifikation: rückwärtskompatibel).
pub const DEFAULT_HEADER_VERSION: &str = "2025-03-26";

/// Header mit der Session-ID.
pub const SESSION_HEADER: &str = "mcp-session-id";
/// Header mit der ausgehandelten Protokollversion.
pub const PROTOCOL_HEADER: &str = "mcp-protocol-version";

/// Aushandlung: die angefragte Version, falls unterstützt, sonst die neueste.
pub fn negotiate(requested: &str) -> &'static str {
    SUPPORTED_VERSIONS
        .iter()
        .find(|v| **v == requested)
        .copied()
        .unwrap_or(LATEST_VERSION)
}

/// Ist `v` eine unterstützte Version?
pub fn is_supported(v: &str) -> bool {
    SUPPORTED_VERSIONS.contains(&v)
}

/// Methodennamen.
pub mod method {
    /// Initialisierung.
    pub const INITIALIZE: &str = "initialize";
    /// Abschluss der Initialisierung (Benachrichtigung).
    pub const INITIALIZED: &str = "notifications/initialized";
    /// Lebenszeichen.
    pub const PING: &str = "ping";
    /// Tools auflisten.
    pub const TOOLS_LIST: &str = "tools/list";
    /// Tool aufrufen.
    pub const TOOLS_CALL: &str = "tools/call";
    /// Ressourcen auflisten.
    pub const RESOURCES_LIST: &str = "resources/list";
    /// Ressourcen-Vorlagen auflisten.
    pub const RESOURCE_TEMPLATES_LIST: &str = "resources/templates/list";
    /// Ressource lesen.
    pub const RESOURCES_READ: &str = "resources/read";
    /// Prompts auflisten.
    pub const PROMPTS_LIST: &str = "prompts/list";
    /// Prompt abrufen.
    pub const PROMPTS_GET: &str = "prompts/get";
    /// Abbruch (Benachrichtigung).
    pub const CANCELLED: &str = "notifications/cancelled";
    /// Fortschritt (Benachrichtigung).
    pub const PROGRESS: &str = "notifications/progress";
    /// Log-Meldung des Servers (Benachrichtigung).
    pub const MESSAGE: &str = "notifications/message";
    /// Tool-Liste geändert.
    pub const TOOLS_CHANGED: &str = "notifications/tools/list_changed";
    /// Ressourcen-Liste geändert.
    pub const RESOURCES_CHANGED: &str = "notifications/resources/list_changed";
    /// Prompt-Liste geändert.
    pub const PROMPTS_CHANGED: &str = "notifications/prompts/list_changed";
}

/// Art einer Liste (für Cache und `list_changed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ListKind {
    /// Tools.
    Tools,
    /// Ressourcen (und Vorlagen).
    Resources,
    /// Prompts.
    Prompts,
}

impl ListKind {
    /// Alle Arten.
    pub const ALL: [Self; 3] = [Self::Tools, Self::Resources, Self::Prompts];

    /// Benachrichtigung `…/list_changed`.
    pub fn changed_method(self) -> &'static str {
        match self {
            Self::Tools => method::TOOLS_CHANGED,
            Self::Resources => method::RESOURCES_CHANGED,
            Self::Prompts => method::PROMPTS_CHANGED,
        }
    }

    /// Art zu einer `list_changed`-Benachrichtigung.
    pub fn from_changed(method: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.changed_method() == method)
    }

    /// Schlüssel der Fähigkeit in `capabilities`.
    pub fn capability(self) -> &'static str {
        match self {
            Self::Tools => "tools",
            Self::Resources => "resources",
            Self::Prompts => "prompts",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation() {
        assert_eq!(negotiate("2025-06-18"), "2025-06-18");
        assert_eq!(negotiate("1999-01-01"), LATEST_VERSION);
        assert!(is_supported("2025-03-26"));
        assert_eq!(
            ListKind::from_changed("notifications/prompts/list_changed"),
            Some(ListKind::Prompts)
        );
    }
}
