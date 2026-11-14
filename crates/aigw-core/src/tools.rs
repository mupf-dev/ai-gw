// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Tool-Inventar und Tool-Richtlinien (AIGW-87, `docs/konzept/16-tools.md`).
//!
//! - [`ToolPolicySpec`] – Ressource `ToolPolicy`: Muster auf Tool-Namen und MCP-Server, Ebene
//!   (`global`, `organization`, `team`, `api-key`, `model-group`) und Aktion (`allow`,
//!   `block-strip`, `block-reject`, `observe`, `require-approval`).
//! - [`ToolPolicyIndex`] – vorberechnete Richtlinien eines Snapshots; [`ToolPolicyIndex::decide`]
//!   entscheidet je Tool und Anfrage-Kontext (Hot Path: nur Vergleiche, keine Allokation).
//! - [`origin_of`] – vermutete Herkunft eines Tools aus Namenskonventionen
//!   (`mcp__<server>__<tool>` von Claude Code, `<server>__<tool>`, sonst Client).
//! - [`ToolBatch`]/[`ToolInventorySink`] – gepufferte, aggregierte Übergabe des Inventars an
//!   die Persistenz (analog zum Request-Log).

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::resource::{Id, Object, Scope, ScopeLevel};

/// Höchstzahl Muster je Liste einer Richtlinie.
pub const MAX_PATTERNS: usize = 512;
/// Höchstlänge eines Musters.
const MAX_PATTERN_LEN: usize = 128;
/// Herkunft „vom Client definiert“ (kein MCP-Server erkennbar).
pub const CLIENT_ORIGIN: &str = "client";

fn default_true() -> bool {
    true
}

fn is_false(v: &bool) -> bool {
    !*v
}

/// Aktion einer Tool-Richtlinie.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ToolAction {
    /// Erlauben (auch: Freigabe im Modus `require-approval`).
    Allow,
    /// Tool aus der Anfrage entfernen (`tool_choice` wird angepasst).
    BlockStrip,
    /// Anfrage ablehnen (`403`, Code `tool_blocked`).
    BlockReject,
    /// Nur protokollieren (Warnung im Log, Zähler) – keine Wirkung.
    Observe,
    /// Unbekannte Tools erst nach Freigabe: passende Tools ohne `allow`-Richtlinie werden je
    /// `unapproved` entfernt oder abgelehnt.
    RequireApproval,
}

impl ToolAction {
    /// Bezeichner wie in YAML.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::BlockStrip => "block-strip",
            Self::BlockReject => "block-reject",
            Self::Observe => "observe",
            Self::RequireApproval => "require-approval",
        }
    }
}

/// Behandlung nicht freigegebener Tools im Modus `require-approval`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum UnapprovedAction {
    /// Aus der Anfrage entfernen (Standard).
    #[default]
    Strip,
    /// Anfrage ablehnen.
    Reject,
}

/// Richtlinie für Tools, die Anwendungen in Anfragen mitschicken (Ressource `ToolPolicy`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolPolicySpec {
    /// Ebene und Ziel (`global` ohne Ziel).
    pub scope: Scope,
    /// Glob-Muster auf den Tool-Namen (`*`, `?`; `!muster` = Ausschluss). Ein Muster passt auf
    /// den vollen Namen (`mcp__github__create_issue`) oder den Namen ohne Server-Präfix
    /// (`create_issue`). Leer = alle Tools.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    /// Glob-Muster auf die Herkunft (MCP-Server-Name; vom Client definierte Tools heißen
    /// `client`). Leer = jede Herkunft.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub servers: Vec<String>,
    /// Aktion.
    pub action: ToolAction,
    /// Nur `require-approval`: nicht freigegebene Tools entfernen (`strip`, Standard) oder die
    /// Anfrage ablehnen (`reject`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unapproved: Option<UnapprovedAction>,
    /// Gesperrt: eine passende Sperre (`block-*`) gilt auch gegen spezifischere `allow`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub locked: bool,
    /// Aktiv?
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Beschreibung (Anzeige).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl ToolPolicySpec {
    /// Prüft Muster und Felder (Admin-API, GitOps).
    pub fn check(&self) -> Result<(), String> {
        if self.tools.len() > MAX_PATTERNS || self.servers.len() > MAX_PATTERNS {
            return Err(format!("höchstens {MAX_PATTERNS} Muster je Liste"));
        }
        for t in &self.tools {
            Glob::parse(t).map_err(|m| format!("tools: {m}"))?;
        }
        for s in &self.servers {
            Glob::parse(s).map_err(|m| format!("servers: {m}"))?;
        }
        if self.unapproved.is_some() && self.action != ToolAction::RequireApproval {
            return Err("unapproved gibt es nur mit action: require-approval".into());
        }
        if self.locked
            && !matches!(
                self.action,
                ToolAction::BlockStrip | ToolAction::BlockReject
            )
        {
            return Err("locked gibt es nur mit action: block-strip oder block-reject".into());
        }
        if self.description.as_ref().is_some_and(|d| d.len() > 1024) {
            return Err("description: höchstens 1024 Zeichen".into());
        }
        if self.scope.level == ScopeLevel::Global && self.scope.target.is_some() {
            return Err("scope: Ebene `global` hat kein Ziel".into());
        }
        if self.scope.level != ScopeLevel::Global
            && self.scope.target.as_deref().is_none_or(str::is_empty)
        {
            return Err(format!(
                "scope: Ebene `{}` braucht ein Ziel (`target`)",
                self.scope.level.as_str()
            ));
        }
        Ok(())
    }
}

/// Tool-Richtlinie.
pub type ToolPolicy = Object<ToolPolicySpec>;

// ---------------------------------------------------------------------------------------------
// Muster
// ---------------------------------------------------------------------------------------------

/// Glob-Muster (`*` beliebig viele, `?` genau ein Byte) mit optionalem `!`-Ausschluss.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Glob {
    deny: bool,
    pat: Box<[u8]>,
}

impl Glob {
    /// Parst einen Eintrag.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let (deny, body) = match raw.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, raw),
        };
        if body.is_empty()
            || body.len() > MAX_PATTERN_LEN
            || body
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || c == '!')
        {
            return Err(format!(
                "ungültiges Muster {raw:?} (1–{MAX_PATTERN_LEN} Zeichen ohne Leerzeichen, \
                 `!` nur am Anfang)"
            ));
        }
        Ok(Self {
            deny,
            pat: body.as_bytes().into(),
        })
    }

    /// Passt das Muster auf `s`?
    pub fn matches(&self, s: &str) -> bool {
        glob(&self.pat, s.as_bytes())
    }
}

/// Glob-Vergleich (iterativ, linear mit Rücksprung auf den letzten `*`), ohne Allokation.
fn glob(p: &[u8], t: &[u8]) -> bool {
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        match p.get(pi) {
            Some(b'*') => {
                star = Some((pi, ti));
                pi += 1;
            }
            Some(c) if *c == b'?' || Some(c) == t.get(ti) => {
                pi += 1;
                ti += 1;
            }
            _ => match star {
                Some((sp, st)) => {
                    pi = sp + 1;
                    ti = st + 1;
                    star = Some((sp, st + 1));
                }
                None => return false,
            },
        }
    }
    p[pi..].iter().all(|c| *c == b'*')
}

/// Musterliste: leer = alles; sonst mindestens ein Einschluss passt (ohne Einschlüsse: alles)
/// und kein Ausschluss.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Patterns(Vec<Glob>);

impl Patterns {
    fn parse(list: &[String]) -> Self {
        // Ungültige Muster (nur bei Altdaten möglich) passen nie.
        Self(list.iter().filter_map(|p| Glob::parse(p).ok()).collect())
    }

    fn matches(&self, candidates: &[&str]) -> bool {
        if self.0.is_empty() {
            return true;
        }
        let mut any_include = false;
        let mut included = false;
        for g in &self.0 {
            let hit = candidates.iter().any(|c| g.matches(c));
            if g.deny {
                if hit {
                    return false;
                }
            } else {
                any_include = true;
                included |= hit;
            }
        }
        included || !any_include
    }
}

// ---------------------------------------------------------------------------------------------
// Herkunft
// ---------------------------------------------------------------------------------------------

/// Vermutete Herkunft eines Tools: `(server, name ohne Präfix)`. `mcp__<server>__<tool>`
/// (Claude Code, Claude Desktop), `<server>__<tool>` (aggregierte MCP-Gateways, auch das
/// eigene `/mcp`); sonst `None` (vom Client definiert).
pub fn origin_of(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("mcp__").unwrap_or(name);
    let (server, tool) = rest.split_once("__")?;
    (!server.is_empty() && !tool.is_empty()).then_some((server, tool))
}

/// Art eines Tools im Inventar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ToolKindName {
    /// Funktion mit JSON-Schema (vom Client ausgeführt).
    Function,
    /// Eingebautes Tool des Anbieters ohne Schema (z. B. OpenAI `web_search`,
    /// `code_interpreter`).
    Builtin,
    /// Server-Tool des Anbieters mit Versionskennung (z. B. Anthropic
    /// `web_search_20250305`, `bash_20250124`).
    ServerTool,
}

impl ToolKindName {
    /// Bezeichner.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Builtin => "builtin",
            Self::ServerTool => "server-tool",
        }
    }

    /// Gegenstück zu [`Self::as_str`].
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "function" => Self::Function,
            "builtin" => Self::Builtin,
            "server-tool" => Self::ServerTool,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Entscheidung
// ---------------------------------------------------------------------------------------------

/// Kontext einer Anfrage (Namen wie in den Ressourcen).
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolCtx<'a> {
    /// Organisation.
    pub organization: Option<&'a str>,
    /// Team.
    pub team: Option<&'a str>,
    /// API-Key.
    pub api_key: Option<&'a str>,
    /// Modellgruppe.
    pub model_group: Option<&'a str>,
}

/// Wirkung auf ein Tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Effect {
    /// Durchlassen.
    Allow,
    /// Aus der Anfrage entfernen.
    Strip,
    /// Anfrage ablehnen.
    Reject,
}

/// Warum eine Wirkung gilt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Keine Richtlinie greift.
    Default,
    /// Ausdrückliche Richtlinie (`allow`/`block-*`).
    Explicit,
    /// Gesperrte Sperre einer äußeren Ebene (`locked`).
    Locked,
    /// Nicht freigegeben (`require-approval`).
    Unapproved,
}

/// Entscheidung für ein Tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// Wirkung.
    pub effect: Effect,
    /// Grund.
    pub reason: Reason,
    /// Entscheidende Richtlinie (Name).
    pub policy: Option<Arc<str>>,
    /// Passende `observe`-Richtlinie (Name), unabhängig von der Wirkung.
    pub observed: Option<Arc<str>>,
}

impl Verdict {
    const ALLOW: Self = Self {
        effect: Effect::Allow,
        reason: Reason::Default,
        policy: None,
        observed: None,
    };
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Compiled {
    name: Arc<str>,
    level: ScopeLevel,
    target: Option<Box<str>>,
    tools: Patterns,
    servers: Patterns,
    action: ToolAction,
    unapproved: UnapprovedAction,
    locked: bool,
}

fn rank(l: ScopeLevel) -> u8 {
    match l {
        ScopeLevel::Global => 0,
        ScopeLevel::Organization => 1,
        ScopeLevel::Team => 2,
        ScopeLevel::ModelGroup => 3,
        ScopeLevel::ApiKey => 4,
    }
}

impl Compiled {
    fn applies(&self, ctx: &ToolCtx<'_>) -> bool {
        let want = match self.level {
            ScopeLevel::Global => return true,
            ScopeLevel::Organization => ctx.organization,
            ScopeLevel::Team => ctx.team,
            ScopeLevel::ApiKey => ctx.api_key,
            ScopeLevel::ModelGroup => ctx.model_group,
        };
        want.is_some_and(|w| self.target.as_deref() == Some(w))
    }

    fn matches(&self, name: &str, origin: Option<(&str, &str)>) -> bool {
        let server = origin.map_or(CLIENT_ORIGIN, |(s, _)| s);
        if !self.servers.matches(&[server]) {
            return false;
        }
        match origin {
            Some((_, short)) => self.tools.matches(&[name, short]),
            None => self.tools.matches(&[name]),
        }
    }
}

/// Vorberechnete Tool-Richtlinien eines Snapshots (nur aktive).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolPolicyIndex {
    policies: Vec<Compiled>,
}

impl ToolPolicyIndex {
    /// Baut den Index aus den Richtlinien (inaktive werden übersprungen).
    pub fn new<'a>(policies: impl IntoIterator<Item = &'a ToolPolicy>) -> Self {
        let mut policies: Vec<Compiled> = policies
            .into_iter()
            .filter(|p| p.spec.enabled)
            .map(|p| Compiled {
                name: Arc::from(p.metadata.name.as_str()),
                level: p.spec.scope.level,
                target: p.spec.scope.target.as_deref().map(Box::from),
                tools: Patterns::parse(&p.spec.tools),
                servers: Patterns::parse(&p.spec.servers),
                action: p.spec.action,
                unapproved: p.spec.unapproved.unwrap_or_default(),
                locked: p.spec.locked,
            })
            .collect();
        policies.sort_by(|a, b| (rank(a.level), &a.name).cmp(&(rank(b.level), &b.name)));
        Self { policies }
    }

    /// Keine Richtlinien (dann ist jede Entscheidung `allow`).
    pub fn is_empty(&self) -> bool {
        self.policies.is_empty()
    }

    /// Gibt es für diesen Kontext überhaupt anwendbare Richtlinien?
    pub fn any_applies(&self, ctx: &ToolCtx<'_>) -> bool {
        self.policies.iter().any(|p| p.applies(ctx))
    }

    /// Entscheidung für das Tool `name` im Kontext `ctx`.
    ///
    /// 1. Eine passende gesperrte Sperre (`locked`) gewinnt immer.
    /// 2. Sonst die passende `allow`/`block-*`-Richtlinie der spezifischsten Ebene
    ///    (`api-key` > `model-group` > `team` > `organization` > `global`); auf gleicher Ebene
    ///    die strengere.
    /// 3. Ohne solche Richtlinie: passt `require-approval`, gilt dessen `unapproved`.
    /// 4. Sonst erlaubt. `observe` wirkt nie, wird aber gemeldet.
    pub fn decide(&self, ctx: &ToolCtx<'_>, name: &str) -> Verdict {
        if self.policies.is_empty() {
            return Verdict::ALLOW;
        }
        let origin = origin_of(name);
        let mut explicit: Option<(u8, Effect, &Compiled)> = None;
        let mut locked: Option<(Effect, &Compiled)> = None;
        let mut approval: Option<(Effect, &Compiled)> = None;
        let mut observed: Option<&Compiled> = None;
        for p in &self.policies {
            if !p.applies(ctx) || !p.matches(name, origin) {
                continue;
            }
            let effect = match p.action {
                ToolAction::Allow => Effect::Allow,
                ToolAction::BlockStrip => Effect::Strip,
                ToolAction::BlockReject => Effect::Reject,
                ToolAction::Observe => {
                    observed.get_or_insert(p);
                    continue;
                }
                ToolAction::RequireApproval => {
                    let e = match p.unapproved {
                        UnapprovedAction::Strip => Effect::Strip,
                        UnapprovedAction::Reject => Effect::Reject,
                    };
                    if approval.is_none_or(|(old, _)| e > old) {
                        approval = Some((e, p));
                    }
                    continue;
                }
            };
            if p.locked && locked.is_none_or(|(old, _)| effect > old) {
                locked = Some((effect, p));
            }
            let r = rank(p.level);
            if explicit.is_none_or(|(or, oe, _)| r > or || (r == or && effect > oe)) {
                explicit = Some((r, effect, p));
            }
        }
        let observed = observed.map(|p| Arc::clone(&p.name));
        let (effect, reason, p) = if let Some((e, p)) = locked {
            (e, Reason::Locked, Some(p))
        } else if let Some((_, e, p)) = explicit {
            (e, Reason::Explicit, Some(p))
        } else if let Some((e, p)) = approval {
            (e, Reason::Unapproved, Some(p))
        } else {
            (Effect::Allow, Reason::Default, None)
        };
        Verdict {
            effect,
            reason,
            policy: p.map(|p| Arc::clone(&p.name)),
            observed,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Inventar: Übergabe an die Persistenz
// ---------------------------------------------------------------------------------------------

/// Identität eines Inventar-Eintrags: Tool-Name je Organisation, Team und Key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ToolKey {
    /// Organisation (ID).
    pub organization: Option<Id>,
    /// Team (ID).
    pub team: Option<Id>,
    /// API-Key (ID).
    pub api_key: Option<Id>,
    /// Tool-Name.
    pub name: Box<str>,
}

/// Definition eines Tools, wie sie zuletzt gesehen wurde (nur bei neuen oder geänderten Tools
/// im Batch).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDefinition {
    /// Beschreibung – nur, wenn der wirksame `payloadMode` Inhalte erlaubt (`masked`/`full`).
    pub description: Option<String>,
    /// JSON-Schema der Parameter (kompakt) – Regel wie bei `description`.
    pub schema: Option<String>,
    /// Treffer des Prompt-Injection-Erkenners in der Beschreibung (Signalnamen).
    pub injection: Vec<String>,
}

/// Aggregierte Beobachtungen eines Tools seit dem letzten Schreiben.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolObservation {
    /// Identität.
    pub key: ToolKey,
    /// Art.
    pub kind: ToolKindName,
    /// Herkunft (MCP-Server oder [`CLIENT_ORIGIN`]).
    pub origin: String,
    /// Client (User-Agent bzw. Anwendungs-Header, gekürzt) der letzten Anfrage.
    pub client: Option<String>,
    /// Erste Sichtung in diesem Batch (ms seit Epoche).
    pub first_seen_ms: i64,
    /// Letzte Sichtung (ms seit Epoche).
    pub last_seen_ms: i64,
    /// Anfragen mit diesem Tool.
    pub requests: u64,
    /// Tool-Aufrufe des Modells.
    pub calls: u64,
    /// Blockierte Anfragen/Aufrufe (entfernt, abgelehnt oder Aufruf unterbunden).
    pub blocked: u64,
    /// Hash der Beschreibung (hex, 128 Bit).
    pub description_hash: String,
    /// Hash des Schemas (hex, 128 Bit).
    pub schema_hash: String,
    /// Definition (nur, wenn die Hashes gegenüber dem bekannten Stand neu sind).
    pub definition: Option<ToolDefinition>,
}

/// Ein Schreibvorgang des Inventars.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolBatch {
    /// Beobachtungen.
    pub items: Vec<ToolObservation>,
}

/// Art eines Inventar-Ereignisses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolEventKind {
    /// Erstmals gesehen.
    Discovered,
    /// Beschreibung oder Schema eines bekannten Tools geändert.
    Changed,
    /// Beschreibung mit Treffern des Prompt-Injection-Erkenners.
    Suspicious,
}

impl ToolEventKind {
    /// Ereignistyp der Webhooks (`tool.discovered`, `tool.changed`, `tool.suspicious`).
    pub fn event_type(self) -> &'static str {
        match self {
            Self::Discovered => "tool.discovered",
            Self::Changed => "tool.changed",
            Self::Suspicious => "tool.suspicious",
        }
    }
}

/// Ereignis aus dem Abgleich mit der Datenbank.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolEvent {
    /// Art.
    pub kind: ToolEventKind,
    /// Eintrag (ID in `tool_inventory`).
    pub id: Id,
    /// Identität.
    pub key: ToolKey,
    /// Herkunft.
    pub origin: String,
    /// Treffer des Injection-Erkenners (Signalnamen).
    pub signals: Vec<String>,
    /// Automatisch gesperrt (`tools.on_change: block`) in dieser Richtlinie.
    pub blocked_by: Option<String>,
}

/// Ergebnis des Schreibens: bekannte Hashes nach dem Abgleich mit der Datenbank.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolFlushResult {
    /// Gespeicherte (aktuelle) Hashes je Tool: (Beschreibung, Schema).
    pub known: HashMap<ToolKey, (String, String)>,
    /// Ereignisse (neu, geändert, verdächtig).
    pub events: Vec<ToolEvent>,
}

/// Rückgabe eines boxed Futures.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Persistenz des Inventars (SQLite/Postgres in `aigw-store`).
pub trait ToolInventorySink: Send + Sync + fmt::Debug + 'static {
    /// Schreibt einen Batch (Upsert, Verlauf, Änderungserkennung).
    fn flush<'a>(&'a self, batch: ToolBatch) -> BoxFuture<'a, Result<ToolFlushResult, String>>;
}

/// Verhalten bei geänderter Beschreibung bzw. geändertem Schema eines bekannten Tools.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolChangeAction {
    /// Ereignis (`tool.changed`), Audit-Eintrag, Markierung „geändert“ (Standard).
    #[default]
    Warn,
    /// Zusätzlich sperren: Tool landet in der Sperr-Richtlinie des Keys (`block-strip`).
    Block,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::ObjectMeta;

    fn pol(
        name: &str,
        level: ScopeLevel,
        target: Option<&str>,
        tools: &[&str],
        action: ToolAction,
    ) -> ToolPolicy {
        Object {
            id: crate::resource::new_id(),
            metadata: ObjectMeta::named(name),
            spec: ToolPolicySpec {
                scope: Scope {
                    level,
                    target: target.map(str::to_owned),
                },
                tools: tools.iter().map(|s| (*s).to_owned()).collect(),
                servers: Vec::new(),
                action,
                unapproved: None,
                locked: false,
                enabled: true,
                description: None,
            },
        }
    }

    const CTX: ToolCtx<'static> = ToolCtx {
        organization: Some("acme"),
        team: Some("web"),
        api_key: Some("k1"),
        model_group: Some("gpt"),
    };

    #[test]
    fn origin_conventions() {
        assert_eq!(
            origin_of("mcp__github__create_issue"),
            Some(("github", "create_issue"))
        );
        assert_eq!(origin_of("jira__search"), Some(("jira", "search")));
        assert_eq!(origin_of("mcp__srv__a__b"), Some(("srv", "a__b")));
        assert_eq!(origin_of("get_weather"), None);
        assert_eq!(origin_of("__x"), None);
        assert_eq!(origin_of("x__"), None);
    }

    #[test]
    fn glob_bytes() {
        let m = |p: &str, t: &str| Glob::parse(p).unwrap().matches(t);
        assert!(m("get_*", "get_issue"));
        assert!(!m("get_*", "list"));
        assert!(m("*", ""));
        assert!(m("a?c", "abc"));
        assert!(Glob::parse("!").is_err());
        assert!(Glob::parse("a b").is_err());
        assert!(Glob::parse("a!b").is_err());
    }

    #[test]
    fn precedence() {
        let ps = [
            pol(
                "g-block",
                ScopeLevel::Global,
                None,
                &["delete_*"],
                ToolAction::BlockStrip,
            ),
            pol(
                "k-allow",
                ScopeLevel::ApiKey,
                Some("k1"),
                &["delete_tmp"],
                ToolAction::Allow,
            ),
            pol(
                "t-reject",
                ScopeLevel::Team,
                Some("web"),
                &["shell"],
                ToolAction::BlockReject,
            ),
            pol(
                "t-allow",
                ScopeLevel::Team,
                Some("web"),
                &["shell"],
                ToolAction::Allow,
            ),
            pol(
                "other",
                ScopeLevel::Team,
                Some("ops"),
                &["*"],
                ToolAction::BlockReject,
            ),
        ];
        let idx = ToolPolicyIndex::new(ps.iter());
        let d = idx.decide(&CTX, "delete_repo");
        assert_eq!((d.effect, d.reason), (Effect::Strip, Reason::Explicit));
        assert_eq!(d.policy.as_deref(), Some("g-block"));
        // Spezifischere Ebene gewinnt.
        assert_eq!(idx.decide(&CTX, "delete_tmp").effect, Effect::Allow);
        // Gleiche Ebene: strenger gewinnt.
        assert_eq!(idx.decide(&CTX, "shell").effect, Effect::Reject);
        // Fremdes Team greift nicht.
        assert_eq!(idx.decide(&CTX, "search").effect, Effect::Allow);
        assert_eq!(idx.decide(&CTX, "search").reason, Reason::Default);
    }

    #[test]
    fn locked_and_approval_and_observe() {
        let mut lock = pol(
            "g-lock",
            ScopeLevel::Global,
            None,
            &["rm_*"],
            ToolAction::BlockReject,
        );
        lock.spec.locked = true;
        let mut appr = pol(
            "o-appr",
            ScopeLevel::Organization,
            Some("acme"),
            &[],
            ToolAction::RequireApproval,
        );
        appr.spec.unapproved = Some(UnapprovedAction::Reject);
        let ps = [
            lock,
            pol(
                "k-allow",
                ScopeLevel::ApiKey,
                Some("k1"),
                &["rm_*", "ok_*"],
                ToolAction::Allow,
            ),
            appr,
            pol(
                "obs",
                ScopeLevel::Global,
                None,
                &["mcp__*", "!*secret*"],
                ToolAction::Observe,
            ),
        ];
        let idx = ToolPolicyIndex::new(ps.iter());
        let d = idx.decide(&CTX, "rm_rf");
        assert_eq!((d.effect, d.reason), (Effect::Reject, Reason::Locked));
        assert_eq!(idx.decide(&CTX, "ok_go").effect, Effect::Allow);
        let d = idx.decide(&CTX, "new_tool");
        assert_eq!((d.effect, d.reason), (Effect::Reject, Reason::Unapproved));
        // observe zählt nur; Muster auf vollen Namen, Ausschluss.
        let d = idx.decide(&CTX, "mcp__gh__x");
        assert_eq!(d.observed.as_deref(), Some("obs"));
        assert_eq!(idx.decide(&CTX, "mcp__gh__secret").observed, None);
    }

    #[test]
    fn servers_and_short_names() {
        let mut p = pol(
            "gh",
            ScopeLevel::Global,
            None,
            &["create_*"],
            ToolAction::BlockStrip,
        );
        p.spec.servers = vec!["github".into()];
        let mut c = pol("cl", ScopeLevel::Global, None, &[], ToolAction::Observe);
        c.spec.servers = vec![CLIENT_ORIGIN.into()];
        let idx = ToolPolicyIndex::new([&p, &c]);
        assert_eq!(
            idx.decide(&CTX, "mcp__github__create_issue").effect,
            Effect::Strip
        );
        assert_eq!(idx.decide(&CTX, "github__create_pr").effect, Effect::Strip);
        assert_eq!(
            idx.decide(&CTX, "mcp__gitlab__create_issue").effect,
            Effect::Allow
        );
        assert_eq!(idx.decide(&CTX, "create_x").effect, Effect::Allow);
        assert!(idx.decide(&CTX, "create_x").observed.is_some());
    }

    #[test]
    fn check_spec() {
        let mut p = pol("x", ScopeLevel::Global, None, &["a"], ToolAction::Allow);
        assert!(p.spec.check().is_ok());
        p.spec.locked = true;
        assert!(p.spec.check().is_err());
        p.spec.locked = false;
        p.spec.unapproved = Some(UnapprovedAction::Strip);
        assert!(p.spec.check().is_err());
        p.spec.unapproved = None;
        p.spec.tools = vec!["a b".into()];
        assert!(p.spec.check().is_err());
        let p = pol("x", ScopeLevel::Team, None, &[], ToolAction::Allow);
        assert!(p.spec.check().is_err());
        // Disabled-Richtlinien bleiben draußen.
        let mut d = pol("d", ScopeLevel::Global, None, &[], ToolAction::BlockReject);
        d.spec.enabled = false;
        assert!(ToolPolicyIndex::new([&d]).is_empty());
    }
}
