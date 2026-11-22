// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Freigaben (`McpGrant`) eines API-Keys auswerten.
//!
//! Regeln (siehe `docs/konzept/15-mcp-gateway.md`):
//! - **Standard: nichts.** Ohne passende Freigabe sieht ein Key keinen MCP-Server.
//! - Freigaben derselben Ebene (Key bzw. Team) für denselben Server werden vereinigt; leere
//!   `tools` heißt „alle Tools“.
//! - Gehört der Key zu einem Team, ist die **Team-Freigabe die Obergrenze**: ohne
//!   Team-Freigabe für den Server kein Zugriff; Key-Freigaben schränken innerhalb davon weiter
//!   ein (wie Modellfreigaben von Key und Team). Keys ohne Team brauchen eine Key-Freigabe.
//! - Ausschlüsse (`!muster`) aus **allen** anwendbaren Freigaben gelten immer (Ausschluss
//!   gewinnt).

use std::collections::BTreeMap;

use aigw_core::mcp::ToolPattern;
use aigw_core::resource::ScopeLevel;
use aigw_core::snapshot::{KeyCtx, Snapshot};

/// Erlaubte Tools einer Ebene (Vereinigung der Freigaben).
#[derive(Debug, Clone, Default)]
struct Rule {
    all: bool,
    allow: Vec<ToolPattern>,
}

impl Rule {
    fn add(&mut self, patterns: &[ToolPattern], deny: &mut Vec<ToolPattern>) {
        let mut any_allow = false;
        for p in patterns {
            if p.deny {
                deny.push(p.clone());
            } else {
                any_allow = true;
                self.allow.push(p.clone());
            }
        }
        // Nur Ausschlüsse oder leer = alle übrigen Tools.
        if !any_allow {
            self.all = true;
        }
    }

    fn matches(&self, tool: &str) -> bool {
        self.all || self.allow.iter().any(|p| p.matches(tool))
    }
}

#[derive(Debug, Clone, Default)]
struct ServerRules {
    key: Option<Rule>,
    team: Option<Rule>,
    deny: Vec<ToolPattern>,
}

/// Freigaben eines Keys, je Server ausgewertet.
#[derive(Debug, Clone, Default)]
pub struct Access {
    servers: BTreeMap<String, ServerRules>,
    in_team: bool,
}

impl Access {
    /// Wertet die Freigaben des Snapshots für `key` aus (nur aktive Server).
    pub fn for_key(snap: &Snapshot, key: &KeyCtx) -> Self {
        let key_name = key.key.metadata.name.as_str();
        let team_name = key.team.as_ref().map(|t| t.metadata.name.as_str());
        let mut servers: BTreeMap<String, ServerRules> = BTreeMap::new();
        for g in &snap.mcp_grants {
            let target = g.spec.subject.target.as_deref();
            let is_key = g.spec.subject.level == ScopeLevel::ApiKey && target == Some(key_name);
            let is_team = g.spec.subject.level == ScopeLevel::Team
                && team_name.is_some()
                && target == team_name;
            if !(is_key || is_team) || !snap.mcp_servers.contains_key(&g.spec.server) {
                continue;
            }
            // Ungültige Muster wurden beim Schreiben abgelehnt; sicherheitshalber werden sie
            // als Ausschluss von allem behandelt, statt sie zu ignorieren.
            let patterns: Vec<ToolPattern> = g
                .spec
                .tools
                .iter()
                .map(|t| {
                    ToolPattern::parse(t).unwrap_or_else(|_| {
                        tracing::warn!(grant = %g.metadata.name, pattern = %t, "ungültiges Tool-Muster");
                        ToolPattern::deny_all()
                    })
                })
                .collect();
            let rules = servers.entry(g.spec.server.clone()).or_default();
            let slot = if is_key {
                &mut rules.key
            } else {
                &mut rules.team
            };
            slot.get_or_insert_with(Rule::default)
                .add(&patterns, &mut rules.deny);
        }
        Self {
            servers,
            in_team: team_name.is_some(),
        }
    }

    /// Darf der Key den Server überhaupt nutzen?
    pub fn server_allowed(&self, server: &str) -> bool {
        self.servers.get(server).is_some_and(|r| {
            if self.in_team {
                r.team.is_some()
            } else {
                r.key.is_some()
            }
        })
    }

    /// Darf der Key das Tool `tool` (Name beim Upstream) des Servers aufrufen?
    pub fn tool_allowed(&self, server: &str, tool: &str) -> bool {
        if !self.server_allowed(server) {
            return false;
        }
        let Some(r) = self.servers.get(server) else {
            return false;
        };
        if r.deny.iter().any(|p| p.matches(tool)) {
            return false;
        }
        let team_ok = r.team.as_ref().is_none_or(|t| t.matches(tool));
        let key_ok = r.key.as_ref().is_none_or(|k| k.matches(tool));
        team_ok && key_ok
    }

    /// Freigegebene Server (sortiert).
    pub fn servers(&self) -> impl Iterator<Item = &str> {
        self.servers
            .keys()
            .map(String::as_str)
            .filter(|s| self.server_allowed(s))
    }

    /// Zugriff auf alle Server und Tools (ohne Snapshot/Key, z. B. [`AllowAll`]-Betrieb in
    /// Tests).
    ///
    /// [`AllowAll`]: https://docs.rs/aigw-proxy
    pub fn all(snap: &Snapshot) -> Self {
        let servers = snap
            .mcp_servers
            .keys()
            .map(|s| {
                (
                    s.clone(),
                    ServerRules {
                        key: Some(Rule {
                            all: true,
                            allow: Vec::new(),
                        }),
                        ..ServerRules::default()
                    },
                )
            })
            .collect();
        Self {
            servers,
            in_team: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aigw_core::resource::{
        ApiKey, ApiKeySpec, ApiKeyStatus, McpGrant, McpGrantSpec, McpServer, McpServerSpec, Object,
        ObjectMeta, Scope, Team, TeamSpec, new_id,
    };
    use aigw_core::snapshot::McpServerCtx;

    use super::*;

    fn obj<S>(name: &str, spec: S) -> Object<S> {
        Object {
            id: new_id(),
            metadata: ObjectMeta::named(name),
            spec,
        }
    }

    fn grant(
        name: &str,
        server: &str,
        level: ScopeLevel,
        target: &str,
        tools: &[&str],
    ) -> Arc<McpGrant> {
        Arc::new(obj(
            name,
            McpGrantSpec {
                server: server.into(),
                subject: Scope {
                    level,
                    target: Some(target.into()),
                },
                tools: tools.iter().map(|s| (*s).to_owned()).collect(),
            },
        ))
    }

    fn snap(grants: Vec<Arc<McpGrant>>) -> Snapshot {
        let mut s = Snapshot::default();
        for n in ["gh", "fs", "off"] {
            if n == "off" {
                continue;
            }
            let server: McpServer = obj(
                n,
                McpServerSpec {
                    secret: None,
                    url: "https://x.example/mcp".into(),
                    transport: Default::default(),
                    credential: None,
                    enabled: true,
                    settings: Default::default(),
                },
            );
            s.mcp_servers.insert(
                n.into(),
                Arc::new(McpServerCtx {
                    server,
                    credential: None,
                }),
            );
        }
        s.mcp_grants = grants;
        s
    }

    fn key(team: Option<&str>) -> KeyCtx {
        let key: ApiKey = ApiKey {
            id: new_id(),
            metadata: ObjectMeta::named("k1"),
            spec: ApiKeySpec {
                team: team.map(str::to_owned),
                ..ApiKeySpec::default()
            },
            status: ApiKeyStatus {
                prefix: "sk-aigw-test".into(),
                state: aigw_core::resource::ApiKeyState::Active,
                last_used_at: None,
            },
        };
        KeyCtx {
            key,
            team: team.map(|t| {
                let t: Team = obj(
                    t,
                    TeamSpec {
                        organization: "o".into(),
                        ..TeamSpec::default()
                    },
                );
                Arc::new(t)
            }),
            organization: None,
            expires_at: None,
        }
    }

    #[test]
    fn default_deny_and_key_grants() {
        let s = snap(vec![]);
        let a = Access::for_key(&s, &key(None));
        assert!(!a.server_allowed("gh"));
        assert_eq!(a.servers().count(), 0);

        let s = snap(vec![
            grant(
                "g1",
                "gh",
                ScopeLevel::ApiKey,
                "k1",
                &["get_*", "!get_secret"],
            ),
            grant("g2", "gh", ScopeLevel::ApiKey, "k1", &["search"]),
            grant("g3", "fs", ScopeLevel::ApiKey, "other", &[]),
            grant("g4", "off", ScopeLevel::ApiKey, "k1", &[]),
        ]);
        let a = Access::for_key(&s, &key(None));
        assert_eq!(a.servers().collect::<Vec<_>>(), vec!["gh"]);
        assert!(a.tool_allowed("gh", "get_issue"));
        assert!(a.tool_allowed("gh", "search"));
        assert!(!a.tool_allowed("gh", "get_secret"));
        assert!(!a.tool_allowed("gh", "delete_repo"));
        assert!(!a.tool_allowed("fs", "read"));
        assert!(!a.server_allowed("off"));
    }

    #[test]
    fn team_is_ceiling() {
        // Key im Team: Key-Freigabe allein reicht nicht.
        let s = snap(vec![grant("g1", "gh", ScopeLevel::ApiKey, "k1", &[])]);
        assert!(!Access::for_key(&s, &key(Some("t"))).server_allowed("gh"));
        // Team-Freigabe genügt; Key-Freigabe schränkt ein; Ausschlüsse beider Ebenen gelten.
        let s = snap(vec![
            grant("t1", "gh", ScopeLevel::Team, "t", &["!admin_*"]),
            grant("k1", "gh", ScopeLevel::ApiKey, "k1", &["get_*", "admin_x"]),
            grant("t2", "fs", ScopeLevel::Team, "t", &[]),
        ]);
        let a = Access::for_key(&s, &key(Some("t")));
        assert!(a.server_allowed("gh") && a.server_allowed("fs"));
        assert!(a.tool_allowed("gh", "get_issue"));
        assert!(!a.tool_allowed("gh", "search"), "Key-Freigabe schränkt ein");
        assert!(!a.tool_allowed("gh", "admin_x"), "Team-Ausschluss gewinnt");
        assert!(a.tool_allowed("fs", "anything"));
        // anderes Team
        assert!(!Access::for_key(&s, &key(Some("u"))).server_allowed("gh"));
    }
}
