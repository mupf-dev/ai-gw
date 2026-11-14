// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Tool-Inventar (AIGW-87, Migration 0007): Schreiben der aggregierten Beobachtungen
//! ([`StoreToolSink`]), Abfragen für die Admin-API und Freigeben/Sperren über
//! `ToolPolicy`-Ressourcen ([`Store::set_tool_decision`]).
//!
//! Je Eintrag (Organisation, Team, Key, Tool-Name) speichert die Tabelle `tool_inventory`
//! Zähler, erste/letzte Sichtung, den letzten Client und die Hashes von Beschreibung und
//! Schema; Beschreibung/Schema selbst nur, wenn der Proxy sie mitgibt (wirksamer
//! `payloadMode` `masked`/`full`). Neue und geänderte Definitionen landen im Verlauf
//! (`tool_inventory_history`); Änderungen setzen `changed`, schreiben einen Audit-Eintrag
//! (`tool`/`changed`) und sperren das Tool optional (`tools.on_change: block`).

use std::collections::HashMap;

use aigw_core::resource::{Id, ObjectMeta, Scope, ScopeLevel, new_id};
use aigw_core::tools::{
    BoxFuture, ToolAction, ToolBatch, ToolChangeAction, ToolEvent, ToolEventKind, ToolFlushResult,
    ToolInventorySink, ToolKey, ToolObservation, ToolPolicySpec,
};

use crate::db::{DbRow, Q, Val};
use crate::{AuditEvent, Store, StoreError};

/// Akteur automatischer Änderungen.
pub const TOOL_ACTOR: &str = "system:tool-inventory";

fn scope_key(k: &ToolKey) -> String {
    let f = |v: Option<Id>| v.map_or_else(|| "-".to_owned(), |i| i.to_string());
    format!("{}|{}|{}", f(k.organization), f(k.team), f(k.api_key))
}

fn now_ms() -> i64 {
    i64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
}

/// Eine Zeile des Inventars.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRow {
    /// ID.
    pub id: Id,
    /// Tool-Name.
    pub name: String,
    /// Organisation (ID).
    pub organization_id: Option<Id>,
    /// Team (ID).
    pub team_id: Option<Id>,
    /// API-Key (ID).
    pub api_key_id: Option<Id>,
    /// Art (`function`, `builtin`, `server-tool`).
    pub kind: String,
    /// Herkunft (MCP-Server oder `client`).
    pub origin: String,
    /// Letzter Client.
    pub client: Option<String>,
    /// Erste Sichtung (ms).
    pub first_seen_ms: i64,
    /// Letzte Sichtung (ms).
    pub last_seen_ms: i64,
    /// Anfragen.
    pub requests: i64,
    /// Tool-Aufrufe des Modells.
    pub calls: i64,
    /// Blockiert (entfernt, abgelehnt, Aufruf unterbunden).
    pub blocked: i64,
    /// Hash der Beschreibung.
    pub description_hash: String,
    /// Hash des Schemas.
    pub schema_hash: String,
    /// Beschreibung (nur bei passendem `payloadMode`).
    pub description: Option<String>,
    /// Schema (JSON, nur bei passendem `payloadMode`).
    pub parameters: Option<String>,
    /// Geändert und noch nicht bestätigt.
    pub changed: bool,
    /// Zeitpunkt der letzten Änderung (ms).
    pub changed_at_ms: Option<i64>,
    /// Treffer des Injection-Erkenners (Signalnamen; leer = keine).
    pub suspicious: Vec<String>,
}

impl ToolRow {
    fn from_row(r: &DbRow) -> Result<Self, StoreError> {
        Ok(Self {
            id: r.uuid("id")?,
            name: r.text("name")?,
            organization_id: r.opt_uuid("organization_id")?,
            team_id: r.opt_uuid("team_id")?,
            api_key_id: r.opt_uuid("api_key_id")?,
            kind: r.text("kind")?,
            origin: r.text("origin")?,
            client: r.opt_text("client")?,
            first_seen_ms: r.ms("first_seen")?,
            last_seen_ms: r.ms("last_seen")?,
            requests: r.i64("requests")?,
            calls: r.i64("calls")?,
            blocked: r.i64("blocked")?,
            description_hash: r.text("description_hash")?,
            schema_hash: r.text("schema_hash")?,
            description: r.opt_text("description")?,
            parameters: r.opt_text("parameters")?,
            changed: r.bool("changed")?,
            changed_at_ms: r.opt_ms("changed_at")?,
            suspicious: r.opt_json("suspicious")?.unwrap_or_default(),
        })
    }
}

/// Eintrag im Verlauf eines Tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolHistoryRow {
    /// Zeitpunkt (ms).
    pub seen_at_ms: i64,
    /// `discovered` oder `changed`.
    pub event: String,
    /// Hash der Beschreibung.
    pub description_hash: String,
    /// Hash des Schemas.
    pub schema_hash: String,
    /// Beschreibung (falls gespeichert).
    pub description: Option<String>,
    /// Schema (falls gespeichert).
    pub parameters: Option<String>,
    /// Client.
    pub client: Option<String>,
}

/// Sichtbarer Bereich (eingeschränkte Rollen): diese Teams und teamlose Einträge dieser
/// Organisationen. `None` = alles.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolVisibility {
    /// Team-IDs.
    pub teams: Vec<Id>,
    /// Organisations-IDs (nur Einträge ohne Team).
    pub teamless_orgs: Vec<Id>,
}

/// Filter der Inventarliste.
#[derive(Debug, Clone, Default)]
pub struct ToolQuery {
    /// Seitengröße.
    pub limit: u32,
    /// Fortsetzung nach (Name, ID).
    pub after: Option<(String, Id)>,
    /// Teilstring im Namen.
    pub search: Option<String>,
    /// Herkunft (exakt).
    pub origin: Option<String>,
    /// Teilstring im Client.
    pub client: Option<String>,
    /// Art (exakt).
    pub kind: Option<String>,
    /// Organisation (ID).
    pub organization: Option<Id>,
    /// Team (ID).
    pub team: Option<Id>,
    /// API-Key (ID).
    pub api_key: Option<Id>,
    /// Nur geänderte.
    pub changed: Option<bool>,
    /// Nur verdächtige.
    pub suspicious: Option<bool>,
    /// Exakter Name.
    pub name: Option<String>,
    /// Sichtbarer Bereich.
    pub visibility: Option<ToolVisibility>,
}

/// Optionen des Schreibens.
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolSinkOptions {
    /// Verhalten bei geänderten Definitionen.
    pub on_change: ToolChangeAction,
}

/// [`ToolInventorySink`] über den Store.
#[derive(Debug, Clone)]
pub struct StoreToolSink {
    store: Store,
    opts: ToolSinkOptions,
}

impl StoreToolSink {
    /// Neuer Sink.
    pub fn new(store: Store, opts: ToolSinkOptions) -> Self {
        Self { store, opts }
    }
}

impl ToolInventorySink for StoreToolSink {
    fn flush<'a>(&'a self, batch: ToolBatch) -> BoxFuture<'a, Result<ToolFlushResult, String>> {
        Box::pin(async move {
            self.store
                .tool_inventory_flush(batch, self.opts)
                .await
                .map_err(|e| e.to_string())
        })
    }
}

fn contains(backend: crate::Backend, col: &str, p: &str) -> String {
    match backend {
        crate::Backend::Sqlite => format!("instr({col}, {p}) > 0"),
        crate::Backend::Postgres => format!("strpos({col}, {p}) > 0"),
    }
}

impl Store {
    /// Schreibt einen Batch des Inventars in einer Transaktion; danach Audit-Einträge und ggf.
    /// automatische Sperren für geänderte Tools.
    pub async fn tool_inventory_flush(
        &self,
        batch: ToolBatch,
        opts: ToolSinkOptions,
    ) -> Result<ToolFlushResult, StoreError> {
        let mut out = ToolFlushResult::default();
        if batch.items.is_empty() {
            return Ok(out);
        }
        let now = now_ms();
        let mut tx = self.pool.begin().await?;
        for item in batch.items {
            let sk = scope_key(&item.key);
            let existing = Q::new(
                "SELECT id, description_hash, schema_hash FROM tool_inventory \
                 WHERE scope_key = $1 AND name = $2",
            )
            .bind(Val::text(sk.clone()))
            .bind(Val::text(item.key.name.to_string()))
            .fetch_optional(&mut tx)
            .await?;
            let def = item.definition.clone();
            let (desc, params, signals) = match &def {
                Some(d) => (d.description.clone(), d.schema.clone(), d.injection.clone()),
                None => (None, None, Vec::new()),
            };
            let suspicious = if signals.is_empty() {
                Val::Json(None)
            } else {
                Val::json(&signals)?
            };
            match existing {
                None => {
                    let id = new_id();
                    insert(
                        &mut tx,
                        id,
                        &sk,
                        &item,
                        desc.clone(),
                        params.clone(),
                        suspicious,
                    )
                    .await?;
                    history(&mut tx, id, now, "discovered", &item, desc, params).await?;
                    push_events(&mut out, ToolEventKind::Discovered, id, &item, &signals);
                }
                Some(row) => {
                    let id = row.uuid("id")?;
                    let differs = row.text("description_hash")? != item.description_hash
                        || row.text("schema_hash")? != item.schema_hash;
                    if differs {
                        Q::new(
                            "UPDATE tool_inventory SET description_hash = $1, schema_hash = $2, \
                             description = $3, parameters = $4, suspicious = $5, changed = $6, \
                             changed_at = $7 WHERE id = $8",
                        )
                        .bind(Val::text(item.description_hash.clone()))
                        .bind(Val::text(item.schema_hash.clone()))
                        .bind(Val::Text(desc.clone()))
                        .bind(Val::Text(params.clone()))
                        .bind(suspicious)
                        .bind(Val::bool(true))
                        .bind(Val::ms(now))
                        .bind(Val::uuid(id))
                        .execute(&mut tx)
                        .await?;
                        history(&mut tx, id, now, "changed", &item, desc, params).await?;
                        push_events(&mut out, ToolEventKind::Changed, id, &item, &signals);
                    } else if def.is_some() {
                        // Gleiche Hashes, Inhalte jetzt erlaubt bzw. Prüfung erneut gelaufen.
                        Q::new(
                            "UPDATE tool_inventory SET description = COALESCE($1, description), \
                             parameters = COALESCE($2, parameters), \
                             suspicious = COALESCE($3, suspicious) WHERE id = $4",
                        )
                        .bind(Val::Text(desc))
                        .bind(Val::Text(params))
                        .bind(suspicious)
                        .bind(Val::uuid(id))
                        .execute(&mut tx)
                        .await?;
                    }
                    counters(&mut tx, id, &item).await?;
                }
            }
            out.known.insert(
                item.key.clone(),
                (item.description_hash.clone(), item.schema_hash.clone()),
            );
        }
        tx.commit().await?;
        self.after_flush(&mut out, opts).await;
        Ok(out)
    }

    /// Audit-Einträge und automatische Sperren nach dem Commit (Fehler nur geloggt).
    async fn after_flush(&self, out: &mut ToolFlushResult, opts: ToolSinkOptions) {
        for ev in &mut out.events {
            if ev.kind == ToolEventKind::Discovered {
                continue;
            }
            let (org, team, key) = match self.scope_names(&ev.key).await {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!(error = %e, "Tool-Inventar: Namen nicht auflösbar");
                    (None, None, None)
                }
            };
            if ev.kind == ToolEventKind::Changed && opts.on_change == ToolChangeAction::Block {
                let scope = match (&key, &team, &org) {
                    (Some(k), _, _) => Scope {
                        level: ScopeLevel::ApiKey,
                        target: Some(k.clone()),
                    },
                    (None, Some(t), _) => Scope {
                        level: ScopeLevel::Team,
                        target: Some(t.clone()),
                    },
                    (None, None, Some(o)) => Scope {
                        level: ScopeLevel::Organization,
                        target: Some(o.clone()),
                    },
                    _ => Scope::global(),
                };
                match self
                    .set_tool_decision(TOOL_ACTOR, &scope, &ev.key.name, ToolAction::BlockStrip)
                    .await
                {
                    Ok(name) => ev.blocked_by = Some(name),
                    Err(e) => {
                        tracing::warn!(tool = %ev.key.name, error = %e, "Tool-Inventar: automatische Sperre fehlgeschlagen");
                    }
                }
            }
            let action = match ev.kind {
                ToolEventKind::Changed => "changed",
                _ => "suspicious",
            };
            let details = serde_json::json!({
                "tool": &*ev.key.name,
                "origin": ev.origin,
                "apiKey": key,
                "signals": ev.signals,
                "blockedBy": ev.blocked_by,
            });
            if let Err(e) = self
                .record_audit_event_scoped(
                    &AuditEvent {
                        actor: TOOL_ACTOR.to_owned(),
                        action: action.to_owned(),
                        kind: "tool".to_owned(),
                        object_id: Some(ev.id),
                        object_name: Some(ev.key.name.to_string()),
                        details: Some(details),
                    },
                    org.as_deref(),
                    team.as_deref(),
                )
                .await
            {
                tracing::warn!(error = %e, "Tool-Inventar: Audit-Eintrag fehlgeschlagen");
            }
        }
    }

    /// Namen von Organisation, Team und Key eines Eintrags.
    async fn scope_names(
        &self,
        k: &ToolKey,
    ) -> Result<(Option<String>, Option<String>, Option<String>), StoreError> {
        async fn name(
            store: &Store,
            table: &str,
            id: Option<Id>,
        ) -> Result<Option<String>, StoreError> {
            let Some(id) = id else { return Ok(None) };
            Q::new(format!("SELECT name FROM {table} WHERE id = $1"))
                .bind(Val::uuid(id))
                .fetch_optional(&store.pool)
                .await?
                .map(|r| r.text("name"))
                .transpose()
        }
        Ok((
            name(self, "organization", k.organization).await?,
            name(self, "team", k.team).await?,
            name(self, "api_key", k.api_key).await?,
        ))
    }

    /// Bekannte Hashes aller Einträge (Warmstart des RAM-Abgleichs), höchstens `limit`.
    pub async fn tool_inventory_known(
        &self,
        limit: u32,
    ) -> Result<HashMap<ToolKey, (String, String)>, StoreError> {
        let rows = Q::new(
            "SELECT organization_id, team_id, api_key_id, name, description_hash, schema_hash \
             FROM tool_inventory ORDER BY last_seen DESC LIMIT $1",
        )
        .bind(Val::int(i64::from(limit)))
        .fetch_all(&self.pool)
        .await?;
        let mut out = HashMap::with_capacity(rows.len());
        for r in rows {
            out.insert(
                ToolKey {
                    organization: r.opt_uuid("organization_id")?,
                    team: r.opt_uuid("team_id")?,
                    api_key: r.opt_uuid("api_key_id")?,
                    name: r.text("name")?.into(),
                },
                (r.text("description_hash")?, r.text("schema_hash")?),
            );
        }
        Ok(out)
    }

    /// Liste des Inventars (Keyset über (Name, ID)); liefert höchstens `limit + 1` Zeilen –
    /// der Aufrufer erkennt daran eine weitere Seite.
    pub async fn list_tools(&self, f: &ToolQuery) -> Result<Vec<ToolRow>, StoreError> {
        let backend = self.backend();
        let mut q = Q::new(String::new());
        let mut conds: Vec<String> = Vec::new();
        if let Some((name, id)) = &f.after {
            let n = q.arg(Val::text(name.clone()));
            let i = q.arg(Val::uuid(*id));
            conds.push(format!("(t.name > {n} OR (t.name = {n} AND t.id > {i}))"));
        }
        if let Some(s) = &f.search {
            let p = q.arg(Val::text(s.clone()));
            conds.push(contains(backend, "t.name", &p));
        }
        if let Some(n) = &f.name {
            conds.push(format!("t.name = {}", q.arg(Val::text(n.clone()))));
        }
        if let Some(o) = &f.origin {
            conds.push(format!("t.origin = {}", q.arg(Val::text(o.clone()))));
        }
        if let Some(c) = &f.client {
            let p = q.arg(Val::text(c.clone()));
            conds.push(format!(
                "t.client IS NOT NULL AND {}",
                contains(backend, "t.client", &p)
            ));
        }
        if let Some(k) = &f.kind {
            conds.push(format!("t.kind = {}", q.arg(Val::text(k.clone()))));
        }
        if let Some(o) = f.organization {
            conds.push(format!("t.organization_id = {}", q.arg(Val::uuid(o))));
        }
        if let Some(t) = f.team {
            conds.push(format!("t.team_id = {}", q.arg(Val::uuid(t))));
        }
        if let Some(k) = f.api_key {
            conds.push(format!("t.api_key_id = {}", q.arg(Val::uuid(k))));
        }
        if let Some(c) = f.changed {
            conds.push(format!("t.changed = {}", q.arg(Val::bool(c))));
        }
        if let Some(s) = f.suspicious {
            conds.push(if s {
                "t.suspicious IS NOT NULL".to_owned()
            } else {
                "t.suspicious IS NULL".to_owned()
            });
        }
        if let Some(v) = &f.visibility {
            let mut ors = Vec::new();
            if !v.teams.is_empty() {
                let l = q.list(v.teams.iter().map(|i| Val::uuid(*i)));
                ors.push(format!("t.team_id IN ({l})"));
            }
            if !v.teamless_orgs.is_empty() {
                let l = q.list(v.teamless_orgs.iter().map(|i| Val::uuid(*i)));
                ors.push(format!(
                    "(t.team_id IS NULL AND t.organization_id IN ({l}))"
                ));
            }
            conds.push(if ors.is_empty() {
                "1 = 0".to_owned()
            } else {
                format!("({})", ors.join(" OR "))
            });
        }
        let lim = q.arg(Val::int(i64::from(f.limit.max(1)) + 1));
        let filter = if conds.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conds.join(" AND "))
        };
        q.sql =
            format!("SELECT t.* FROM tool_inventory t {filter} ORDER BY t.name, t.id LIMIT {lim}");
        q.fetch_all(&self.pool)
            .await?
            .iter()
            .map(ToolRow::from_row)
            .collect()
    }

    /// Ein Eintrag (`None`, wenn unbekannt).
    pub async fn get_tool(&self, id: Id) -> Result<Option<ToolRow>, StoreError> {
        Q::new("SELECT t.* FROM tool_inventory t WHERE t.id = $1")
            .bind(Val::uuid(id))
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(ToolRow::from_row)
            .transpose()
    }

    /// Verlauf eines Eintrags (neueste zuerst).
    pub async fn tool_history(
        &self,
        id: Id,
        limit: u32,
    ) -> Result<Vec<ToolHistoryRow>, StoreError> {
        Q::new(
            "SELECT * FROM tool_inventory_history WHERE tool_id = $1 \
             ORDER BY seen_at DESC, id DESC LIMIT $2",
        )
        .bind(Val::uuid(id))
        .bind(Val::int(i64::from(limit.max(1))))
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| {
            Ok(ToolHistoryRow {
                seen_at_ms: r.ms("seen_at")?,
                event: r.text("event")?,
                description_hash: r.text("description_hash")?,
                schema_hash: r.text("schema_hash")?,
                description: r.opt_text("description")?,
                parameters: r.opt_text("parameters")?,
                client: r.opt_text("client")?,
            })
        })
        .collect()
    }

    /// Markierung „geändert“ zurücksetzen (Bestätigung durch einen Admin).
    pub async fn acknowledge_tool(&self, id: Id) -> Result<(), StoreError> {
        Q::new("UPDATE tool_inventory SET changed = $1 WHERE id = $2")
            .bind(Val::bool(false))
            .bind(Val::uuid(id))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Löscht einen Eintrag samt Verlauf (z. B. veraltete Tools).
    pub async fn delete_tool(&self, id: Id) -> Result<bool, StoreError> {
        let mut tx = self.pool.begin().await?;
        Q::new("DELETE FROM tool_inventory_history WHERE tool_id = $1")
            .bind(Val::uuid(id))
            .execute(&mut tx)
            .await?;
        let n = Q::new("DELETE FROM tool_inventory WHERE id = $1")
            .bind(Val::uuid(id))
            .execute(&mut tx)
            .await?;
        tx.commit().await?;
        Ok(n > 0)
    }

    /// Freigeben (`allow`) bzw. Sperren (`block-strip`/`block-reject`) eines Tools auf einer
    /// Ebene: Das Tool landet in der Richtlinie `tools-allow-<ebene>[-<ziel>]` bzw.
    /// `tools-block-<ebene>[-<ziel>]` (angelegt bei Bedarf) und wird aus der Gegenrichtlinie
    /// derselben Ebene entfernt (leere Richtlinien werden gelöscht). Liefert den Namen der
    /// geschriebenen Richtlinie. Jede Änderung läuft über die Ressourcen-API (Audit je Objekt,
    /// neuer Snapshot).
    pub async fn set_tool_decision(
        &self,
        actor: &str,
        scope: &Scope,
        tool: &str,
        action: ToolAction,
    ) -> Result<String, StoreError> {
        if tool.is_empty() || tool.len() > 256 || tool.contains(['*', '?', '!']) {
            return Err(StoreError::Invalid(format!(
                "Tool-Name {tool:?} lässt sich nicht als Muster verwenden"
            )));
        }
        let (keep, drop) = match action {
            ToolAction::Allow => ("allow", "block"),
            ToolAction::BlockStrip | ToolAction::BlockReject => ("block", "allow"),
            other => {
                return Err(StoreError::Invalid(format!(
                    "Aktion {} ist keine Entscheidung (allow, block-strip, block-reject)",
                    other.as_str()
                )));
            }
        };
        let keep_name = decision_policy_name(keep, scope);
        let drop_name = decision_policy_name(drop, scope);
        // Gegenrichtlinie: Tool entfernen.
        match self.get::<ToolPolicySpec>(&drop_name).await {
            Ok(mut p) if p.spec.tools.iter().any(|t| t == tool) => {
                p.spec.tools.retain(|t| t != tool);
                if p.spec.tools.is_empty() {
                    self.delete(
                        actor,
                        aigw_core::resource::ResourceKind::ToolPolicy,
                        &drop_name,
                    )
                    .await?;
                } else {
                    self.update(actor, p.metadata, p.spec).await?;
                }
            }
            Ok(_) | Err(StoreError::NotFound { .. }) => {}
            Err(e) => return Err(e),
        }
        match self.get::<ToolPolicySpec>(&keep_name).await {
            Ok(mut p) => {
                let has = p.spec.tools.iter().any(|t| t == tool);
                if has && p.spec.action == action && p.spec.enabled {
                    return Ok(keep_name);
                }
                if !has {
                    p.spec.tools.push(tool.to_owned());
                }
                p.spec.action = action;
                p.spec.enabled = true;
                self.update(actor, p.metadata, p.spec).await?;
            }
            Err(StoreError::NotFound { .. }) => {
                let mut meta = ObjectMeta::named(keep_name.clone());
                meta.labels
                    .insert("aigw.io/managed-by".into(), "tool-inventory".into());
                self.create(
                    actor,
                    meta,
                    ToolPolicySpec {
                        scope: scope.clone(),
                        tools: vec![tool.to_owned()],
                        servers: Vec::new(),
                        action,
                        unapproved: None,
                        locked: false,
                        enabled: true,
                        description: Some(if keep == "allow" {
                            "Freigaben aus dem Tool-Inventar".into()
                        } else {
                            "Sperren aus dem Tool-Inventar".into()
                        }),
                    },
                )
                .await?;
            }
            Err(e) => return Err(e),
        }
        Ok(keep_name)
    }
}

/// Name der Entscheidungs-Richtlinie einer Ebene (`tools-allow-team-web`).
pub fn decision_policy_name(kind: &str, scope: &Scope) -> String {
    let mut name = format!("tools-{kind}-{}", scope.level.as_str());
    if let Some(t) = &scope.target {
        name.push('-');
        name.push_str(t);
    }
    if name.len() > 128 {
        let d = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, name.as_bytes());
        let hex: String = d.as_ref()[..8].iter().map(|b| format!("{b:02x}")).collect();
        let mut cut = 110;
        while !name.is_char_boundary(cut) {
            cut -= 1;
        }
        name.truncate(cut);
        name.push('-');
        name.push_str(&hex);
    }
    name
}

fn push_events(
    out: &mut ToolFlushResult,
    kind: ToolEventKind,
    id: Id,
    item: &ToolObservation,
    signals: &[String],
) {
    out.events.push(ToolEvent {
        kind,
        id,
        key: item.key.clone(),
        origin: item.origin.clone(),
        signals: signals.to_vec(),
        blocked_by: None,
    });
    if !signals.is_empty() {
        out.events.push(ToolEvent {
            kind: ToolEventKind::Suspicious,
            id,
            key: item.key.clone(),
            origin: item.origin.clone(),
            signals: signals.to_vec(),
            blocked_by: None,
        });
    }
}

async fn insert(
    tx: &mut crate::db::Tx,
    id: Id,
    scope_key: &str,
    item: &ToolObservation,
    desc: Option<String>,
    params: Option<String>,
    suspicious: Val,
) -> Result<(), StoreError> {
    Q::new(
        "INSERT INTO tool_inventory (id, scope_key, name, organization_id, team_id, api_key_id, \
         kind, origin, client, first_seen, last_seen, requests, calls, blocked, \
         description_hash, schema_hash, description, parameters, changed, suspicious) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, \
         $19, $20)",
    )
    .bind(Val::uuid(id))
    .bind(Val::text(scope_key))
    .bind(Val::text(item.key.name.to_string()))
    .bind(Val::Uuid(item.key.organization))
    .bind(Val::Uuid(item.key.team))
    .bind(Val::Uuid(item.key.api_key))
    .bind(Val::text(item.kind.as_str()))
    .bind(Val::text(item.origin.clone()))
    .bind(Val::Text(item.client.clone()))
    .bind(Val::ms(item.first_seen_ms))
    .bind(Val::ms(item.last_seen_ms))
    .bind(Val::int(i64::try_from(item.requests).unwrap_or(i64::MAX)))
    .bind(Val::int(i64::try_from(item.calls).unwrap_or(i64::MAX)))
    .bind(Val::int(i64::try_from(item.blocked).unwrap_or(i64::MAX)))
    .bind(Val::text(item.description_hash.clone()))
    .bind(Val::text(item.schema_hash.clone()))
    .bind(Val::Text(desc))
    .bind(Val::Text(params))
    .bind(Val::bool(false))
    .bind(suspicious)
    .execute(tx)
    .await?;
    Ok(())
}

async fn history(
    tx: &mut crate::db::Tx,
    tool_id: Id,
    now: i64,
    event: &str,
    item: &ToolObservation,
    desc: Option<String>,
    params: Option<String>,
) -> Result<(), StoreError> {
    Q::new(
        "INSERT INTO tool_inventory_history (id, tool_id, seen_at, event, description_hash, \
         schema_hash, description, parameters, client) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(Val::uuid(new_id()))
    .bind(Val::uuid(tool_id))
    .bind(Val::ms(now))
    .bind(Val::text(event))
    .bind(Val::text(item.description_hash.clone()))
    .bind(Val::text(item.schema_hash.clone()))
    .bind(Val::Text(desc))
    .bind(Val::Text(params))
    .bind(Val::Text(item.client.clone()))
    .execute(tx)
    .await?;
    Ok(())
}

async fn counters(
    tx: &mut crate::db::Tx,
    id: Id,
    item: &ToolObservation,
) -> Result<(), StoreError> {
    Q::new(
        "UPDATE tool_inventory SET requests = requests + $1, calls = calls + $2, \
         blocked = blocked + $3, \
         last_seen = CASE WHEN last_seen < $4 THEN $4 ELSE last_seen END, \
         client = COALESCE($5, client) WHERE id = $6",
    )
    .bind(Val::int(i64::try_from(item.requests).unwrap_or(i64::MAX)))
    .bind(Val::int(i64::try_from(item.calls).unwrap_or(i64::MAX)))
    .bind(Val::int(i64::try_from(item.blocked).unwrap_or(i64::MAX)))
    .bind(Val::ms(item.last_seen_ms))
    .bind(Val::Text(item.client.clone()))
    .bind(Val::uuid(id))
    .execute(tx)
    .await?;
    Ok(())
}
