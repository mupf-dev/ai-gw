// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Gemeinsame Hilfen der Guardrail-Plugins (`pii`, `prompt-filter`, `content-safety`):
//! Textstellen in IR-Nachrichten, Rollen-Auswahl, Richtung.

use aigw_core::ir::{ChatRequest, Content, Message, PartKind, Role};
use serde::Deserialize;

/// Richtung eines Guardrails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Direction {
    /// Nur Eingabe (Anfrage).
    Input,
    /// Nur Ausgabe (Antwort/Stream).
    Output,
    /// Beides.
    #[default]
    Both,
}

impl Direction {
    pub(crate) fn input(self) -> bool {
        matches!(self, Self::Input | Self::Both)
    }

    pub(crate) fn output(self) -> bool {
        matches!(self, Self::Output | Self::Both)
    }
}

/// Rollen, deren Nachrichten geprüft werden.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RoleSel {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

impl RoleSel {
    pub(crate) fn matches(self, r: Role) -> bool {
        matches!(
            (self, r),
            (Self::System, Role::System)
                | (Self::Developer, Role::Developer)
                | (Self::User, Role::User)
                | (Self::Assistant, Role::Assistant)
                | (Self::Tool, Role::Tool)
        )
    }
}

/// Besucht alle Textstellen eines Inhalts veränderbar: Text, Ablehnung, Tool-Argumente,
/// Tool-Ergebnisse. Reasoning bleibt unberührt (Signaturen der Anbieter).
pub(crate) fn visit_mut<E>(
    c: &mut Content,
    f: &mut impl FnMut(&mut String) -> Result<(), E>,
) -> Result<(), E> {
    match c {
        Content::Text(t) => f(t),
        Content::Parts(parts) => {
            for p in parts {
                match &mut p.kind {
                    PartKind::Text { text } | PartKind::Refusal { text } => f(text)?,
                    PartKind::ToolCall { arguments, .. } => f(arguments)?,
                    PartKind::ToolResult { content, .. } => visit_mut(content, f)?,
                    _ => {}
                }
            }
            Ok(())
        }
    }
}

/// Besucht alle Textstellen eines Inhalts (nur lesen).
pub(crate) fn visit<'a>(c: &'a Content, f: &mut impl FnMut(&'a str)) {
    match c {
        Content::Text(t) => f(t),
        Content::Parts(parts) => {
            for p in parts {
                match &p.kind {
                    PartKind::Text { text } | PartKind::Refusal { text } => f(text),
                    PartKind::ToolCall { arguments, .. } => f(arguments),
                    PartKind::ToolResult { content, .. } => visit(content, f),
                    _ => {}
                }
            }
        }
    }
}

/// Nachrichten der Anfrage mit einer der Rollen.
pub(crate) fn messages<'a>(
    req: &'a ChatRequest,
    roles: &'a [RoleSel],
) -> impl Iterator<Item = &'a Message> + 'a {
    req.messages
        .iter()
        .filter(move |m| roles.iter().any(|r| r.matches(m.role)))
}

/// Größter Index ≤ `max` auf einer Zeichengrenze.
pub(crate) fn floor_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut i = max;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}
