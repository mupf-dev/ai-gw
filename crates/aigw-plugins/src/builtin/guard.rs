// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Gemeinsame Hilfen der Guardrail-Plugins (`pii`, `prompt-filter`, `content-safety`):
//! Textstellen in IR-Nachrichten, Rollen-Auswahl, Richtung.

use aigw_core::ir::{ChatRequest, Content, Message, PartKind, Role, StreamEvent};
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

/// Besucht alle Textstellen einer Antwortnachricht einschließlich lesbarem Reasoning (nur
/// lesen; Ausgabeprüfung von `prompt-filter`/`prompt-leak`). `reasoning` meldet, ob die Stelle
/// Reasoning ist (dort darf nichts verändert werden – Signaturen der Anbieter).
pub(crate) fn visit_output<'a>(c: &'a Content, f: &mut impl FnMut(&'a str, bool)) {
    match c {
        Content::Text(t) => f(t, false),
        Content::Parts(parts) => {
            for p in parts {
                match &p.kind {
                    PartKind::Text { text } | PartKind::Refusal { text } => f(text, false),
                    PartKind::ToolCall { arguments, .. } => f(arguments, false),
                    PartKind::ToolResult { content, .. } => visit_output(content, f),
                    PartKind::Reasoning { text, .. } => f(text, true),
                    _ => {}
                }
            }
        }
    }
}

/// Textblock eines Streams (Ausgabeprüfung mit Zurückhalten).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamBlock {
    Text(u32),
    Tool(u32),
    Reasoning(u32),
}

impl StreamBlock {
    fn event(self, text: String) -> StreamEvent {
        match self {
            Self::Text(index) => StreamEvent::TextDelta { index, text },
            Self::Tool(index) => StreamEvent::ToolCallDelta {
                index,
                arguments: text,
            },
            Self::Reasoning(index) => StreamEvent::ReasoningDelta { index, text },
        }
    }

    /// Reasoning (nicht veränderbar)?
    pub(crate) fn is_reasoning(self) -> bool {
        matches!(self, Self::Reasoning(_))
    }
}

/// Zurückhalten im Stream für Ausgabeprüfungen, die nur erkennen bzw. kurze Stellen ersetzen
/// (`prompt-filter` mit `direction: response|both`, `prompt-leak`): Die letzten `window`
/// Bytes eines Blocks bleiben zurück; jede Fundstelle bis zu `window` Bytes Länge liegt
/// damit vollständig im Puffer, bevor ein Teil davon hinausgeht. `scan` sieht jeweils den
/// ganzen Puffer (darf ihn verändern, z. B. schwärzen; `true` = letzter Aufruf des Blocks) und kann die Ausgabe ablehnen.
#[derive(Debug, Default)]
pub(crate) struct HoldBack {
    block: Option<StreamBlock>,
    buf: String,
}

impl HoldBack {
    /// Ein Stream-Ereignis; zurückgehaltene Stücke gehen über `emit` (vor dem aktuellen
    /// Ereignis) hinaus. `Err` = Ausgabe ablehnen.
    pub(crate) fn on_event<E>(
        &mut self,
        ev: &mut StreamEvent,
        window: usize,
        emit: &mut impl FnMut(StreamEvent),
        scan: &mut impl FnMut(StreamBlock, &mut String, bool) -> Result<(), E>,
    ) -> Result<crate::plugin::ChunkFlow, E> {
        let (block, text) = match ev {
            StreamEvent::TextDelta { index, text } => (StreamBlock::Text(*index), text),
            StreamEvent::ToolCallDelta { index, arguments } => {
                (StreamBlock::Tool(*index), arguments)
            }
            StreamEvent::ReasoningDelta { index, text } => (StreamBlock::Reasoning(*index), text),
            _ => {
                if let Some(e) = self.flush(scan)? {
                    emit(e);
                }
                return Ok(crate::plugin::ChunkFlow::Continue);
            }
        };
        if self.block != Some(block) {
            if let Some(e) = self.flush(scan)? {
                emit(e);
            }
            self.block = Some(block);
        }
        self.buf.push_str(text);
        scan(block, &mut self.buf, false)?;
        let limit = floor_boundary(&self.buf, self.buf.len().saturating_sub(window));
        let cut = match self.buf[..limit].rfind(char::is_whitespace) {
            // Leerzeichen sind meist ASCII; auf Zeichengrenze nach dem Leerzeichen schneiden.
            Some(p) => p + self.buf[p..].chars().next().map_or(1, char::len_utf8),
            None => limit,
        };
        if cut == 0 {
            return Ok(crate::plugin::ChunkFlow::Drop);
        }
        *text = self.buf.drain(..cut).collect();
        Ok(crate::plugin::ChunkFlow::Continue)
    }

    /// Gibt den ganzen Puffer aus (nach einer letzten Prüfung).
    pub(crate) fn flush<E>(
        &mut self,
        scan: &mut impl FnMut(StreamBlock, &mut String, bool) -> Result<(), E>,
    ) -> Result<Option<StreamEvent>, E> {
        let Some(block) = self.block.take() else {
            return Ok(None);
        };
        if self.buf.is_empty() {
            return Ok(None);
        }
        scan(block, &mut self.buf, true)?;
        Ok(Some(block.event(std::mem::take(&mut self.buf))))
    }
}
