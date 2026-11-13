// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Manipulationsschutz und Aufbewahrung des Audit-Logs (F116).
//!
//! **Hash-Kette:** Jeder Eintrag bekommt eine fortlaufende Nummer `seq`, den Hash seines
//! Vorgängers (`prev_hash`) und seinen eigenen Hash (`hash` = SHA-256 über `prev_hash` und eine
//! kanonische JSON-Form aller Felder). Den Kopf der Kette (letzte Nummer, letzter Hash) hält die
//! Tabelle `audit_chain` (eine Zeile); jedes Schreiben sperrt diese Zeile bis zum Commit –
//! damit ist die Reihenfolge auch bei mehreren Instanzen (Postgres) eindeutig. Geändertes,
//! gelöschtes oder eingeschobenes wird bei [`Store::verify_audit_chain`] gefunden: falscher
//! Hash, falscher Vorgänger, Lücke in `seq` oder abweichender Kopf (abgeschnittenes Ende).
//!
//! Grenzen: Wer Schreibrechte auf die Datenbank hat, kann die ganze Kette samt Kopf neu
//! berechnen. Dagegen hilft nur ein Abgleich mit einem außerhalb gesicherten Kopf
//! (`head` aus dem Prüfbericht, z. B. regelmäßig in ein WORM-Archiv/SIEM schreiben).
//!
//! **Aufbewahrung:** [`Store::prune_audit_log`] löscht einen Anfang der Kette (alle Einträge
//! vor dem Stichtag) und merkt sich Nummer und Hash des letzten gelöschten Eintrags als
//! Anker (`anchor_seq`, `anchor_hash`); die Prüfung beginnt dort. Die Löschung selbst steht
//! als Eintrag (`audit-log`/`prune`) in der Kette.
//!
//! Einträge aus der Zeit vor der Hash-Kette (Migration 0002) haben keine Nummer; sie werden
//! als „unverkettet“ gezählt, aber nicht geprüft.

use std::fmt::Write as _;

use aigw_core::resource::Id;
use serde::Serialize;
use serde_json::Value;
use time::OffsetDateTime;

use super::{Val, millis, now, trunc_millis};
use crate::db::{DbRow, Q, Tx};
use crate::{Store, StoreError};

/// Art im Audit-Log für Vorgänge am Audit-Log selbst (Aufbewahrung).
pub const AUDIT_LOG_KIND: &str = "audit-log";

/// Hash vor dem ersten Eintrag (64 × `0`).
pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Höchstzahl gemeldeter Abweichungen je Prüfung.
const MAX_ISSUES: usize = 100;

/// Ein neuer Eintrag (alle Felder außer Kettenfeldern).
#[derive(Debug, Clone)]
pub(crate) struct NewAuditRow {
    pub(crate) id: Id,
    pub(crate) at: OffsetDateTime,
    pub(crate) actor: String,
    pub(crate) action: String,
    pub(crate) kind: String,
    pub(crate) object_id: Option<Id>,
    pub(crate) object_name: Option<String>,
    pub(crate) config_version: i64,
    pub(crate) before: Option<Value>,
    pub(crate) after: Option<Value>,
    pub(crate) organization: Option<String>,
    pub(crate) team: Option<String>,
    pub(crate) organization_before: Option<String>,
    pub(crate) team_before: Option<String>,
}

impl NewAuditRow {
    /// Eintrag „jetzt“ mit neuer ID, ohne Bereich und Vorher/Nachher.
    pub(crate) fn new(actor: &str, action: &str, kind: &str, config_version: i64) -> Self {
        Self {
            id: aigw_core::resource::new_id(),
            at: trunc_millis(now()),
            actor: actor.to_owned(),
            action: action.to_owned(),
            kind: kind.to_owned(),
            object_id: None,
            object_name: None,
            config_version,
            before: None,
            after: None,
            organization: None,
            team: None,
            organization_before: None,
            team_before: None,
        }
    }
}

/// Schreibt JSON kanonisch: Objektschlüssel sortiert, keine Leerzeichen (unabhängig von der
/// Schlüsselreihenfolge, die z. B. Postgres-JSONB beim Speichern ändert).
fn canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(k.clone()).to_string());
                out.push(':');
                canonical(&m[k], out);
            }
            out.push('}');
        }
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(x, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// Hash eines Eintrags (hex) über Vorgänger-Hash, Nummer und alle Felder.
fn entry_hash(prev: &str, seq: i64, r: &NewAuditRow) -> String {
    let doc = serde_json::json!({
        "seq": seq,
        "id": r.id.to_string(),
        "at": millis(r.at),
        "actor": r.actor,
        "action": r.action,
        "kind": r.kind,
        "objectId": r.object_id.map(|i| i.to_string()),
        "objectName": r.object_name,
        "configVersion": r.config_version,
        "before": r.before,
        "after": r.after,
        "organization": r.organization,
        "team": r.team,
        "organizationBefore": r.organization_before,
        "teamBefore": r.team_before,
    });
    let mut text = String::with_capacity(256);
    text.push_str(prev);
    text.push('\n');
    canonical(&doc, &mut text);
    let d = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, text.as_bytes());
    let mut hex = String::with_capacity(64);
    for b in d.as_ref() {
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

/// Hängt einen Eintrag an die Kette an (in der Transaktion `tx`; sperrt den Kettenkopf bis
/// zum Commit). **Alle** Schreibzugriffe auf `audit_log` müssen hierüber laufen.
pub(crate) async fn append(tx: &mut Tx, r: &NewAuditRow) -> Result<(), StoreError> {
    // Sperrt die Kopfzeile (Postgres: Zeilensperre bis Commit; SQLite: Schreibsperre).
    let head = Q::new("UPDATE audit_chain SET seq = seq + 1 WHERE id = 1 RETURNING seq, head")
        .fetch_one(&mut *tx)
        .await?;
    let seq = head.i64("seq")?;
    let prev = head.text("head")?;
    let hash = entry_hash(&prev, seq, r);
    Q::new(
        "INSERT INTO audit_log (id, at, actor, action, kind, object_id, object_name, config_version,
                                before_json, after_json, organization, team,
                                organization_before, team_before, seq, prev_hash, hash)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)",
    )
    .bind(Val::uuid(r.id))
    .bind(Val::time(r.at))
    .bind(Val::text(r.actor.clone()))
    .bind(Val::text(r.action.clone()))
    .bind(Val::text(r.kind.clone()))
    .bind(Val::Uuid(r.object_id))
    .bind(Val::Text(r.object_name.clone()))
    .bind(Val::int(r.config_version))
    .bind(Val::Json(r.before.clone()))
    .bind(Val::Json(r.after.clone()))
    .bind(Val::Text(r.organization.clone()))
    .bind(Val::Text(r.team.clone()))
    .bind(Val::Text(r.organization_before.clone()))
    .bind(Val::Text(r.team_before.clone()))
    .bind(Val::int(seq))
    .bind(Val::text(prev))
    .bind(Val::text(hash.clone()))
    .execute(&mut *tx)
    .await?;
    Q::new("UPDATE audit_chain SET head = $1 WHERE id = 1")
        .bind(Val::text(hash))
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// Abweichung bei der Prüfung.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AuditChainIssue {
    /// Nummer in der Kette (bei fehlenden Einträgen: die erwartete).
    pub seq: i64,
    /// ID des Eintrags (fehlt bei fehlenden Einträgen).
    #[schema(value_type = Option<String>, format = Uuid)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Id>,
    /// Beschreibung.
    pub problem: String,
}

/// Ergebnis von [`Store::verify_audit_chain`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AuditChainReport {
    /// Kette unversehrt?
    pub ok: bool,
    /// Geprüfte Einträge.
    pub checked: u64,
    /// Einträge ohne Kettenfelder (aus der Zeit vor der Hash-Kette) – nicht prüfbar.
    pub unchained: u64,
    /// Anker (Nummer des letzten durch Aufbewahrung gelöschten Eintrags, sonst 0).
    pub anchor_seq: i64,
    /// Letzte Nummer laut Kettenkopf.
    pub head_seq: i64,
    /// Hash des Kettenkopfs – zum Abgleich mit einer extern gesicherten Kopie.
    pub head_hash: String,
    /// Abweichungen (höchstens 100).
    pub issues: Vec<AuditChainIssue>,
}

fn row_of(row: &DbRow) -> Result<(NewAuditRow, i64, String, String), StoreError> {
    let r = super::Row(row);
    Ok((
        NewAuditRow {
            id: r.id()?,
            at: r.time("at")?,
            actor: r.text("actor")?,
            action: r.text("action")?,
            kind: r.text("kind")?,
            object_id: r.opt_uuid("object_id")?,
            object_name: r.opt_text("object_name")?,
            config_version: r.int("config_version")?,
            before: row.opt_json_value("before_json")?,
            after: row.opt_json_value("after_json")?,
            organization: r.opt_text("organization")?,
            team: r.opt_text("team")?,
            organization_before: r.opt_text("organization_before")?,
            team_before: r.opt_text("team_before")?,
        },
        r.int("seq")?,
        r.text("prev_hash")?,
        r.text("hash")?,
    ))
}

impl Store {
    /// Prüft die Hash-Kette des Audit-Logs vollständig (seitenweise, 1000 Einträge je Abfrage).
    pub async fn verify_audit_chain(&self) -> Result<AuditChainReport, StoreError> {
        let head = Q::new("SELECT seq, head, anchor_seq, anchor_hash FROM audit_chain WHERE id = 1")
            .fetch_one(&self.pool)
            .await?;
        let (head_seq, head_hash) = (head.i64("seq")?, head.text("head")?);
        let anchor_seq = head.i64("anchor_seq")?;
        let mut prev = head.text("anchor_hash")?;
        let mut expected = anchor_seq + 1;
        let mut issues = Vec::new();
        let mut checked = 0u64;
        fn push(issues: &mut Vec<AuditChainIssue>, seq: i64, id: Option<Id>, problem: String) {
            if issues.len() < MAX_ISSUES {
                issues.push(AuditChainIssue { seq, id, problem });
            }
        }
        loop {
            let rows = Q::new(
                "SELECT id, at, actor, action, kind, object_id, object_name, config_version,
                        organization, team, organization_before, team_before, before_json,
                        after_json, seq, prev_hash, hash
                 FROM audit_log WHERE seq >= $1 ORDER BY seq LIMIT 1000",
            )
            .bind(Val::int(expected))
            .fetch_all(&self.pool)
            .await?;
            if rows.is_empty() {
                break;
            }
            for row in &rows {
                let (r, seq, prev_hash, hash) = row_of(row)?;
                if seq > expected {
                    push(
                        &mut issues,
                        expected,
                        None,
                        format!(
                            "Einträge {expected}–{} fehlen (gelöscht?)",
                            seq - 1
                        ),
                    );
                }
                if prev_hash != prev {
                    push(
                        &mut issues,
                        seq,
                        Some(r.id),
                        "Vorgänger-Hash passt nicht (Eintrag eingeschoben oder Vorgänger verändert)"
                            .into(),
                    );
                }
                if entry_hash(&prev_hash, seq, &r) != hash {
                    push(
                        &mut issues,
                        seq,
                        Some(r.id),
                        "Hash passt nicht zum Inhalt (Eintrag verändert)".into(),
                    );
                }
                prev = hash;
                expected = seq + 1;
                checked += 1;
            }
        }
        if expected - 1 != head_seq {
            push(
                &mut issues,
                expected,
                None,
                format!(
                    "Kettenende {} statt {head_seq} laut Kopf (Einträge am Ende gelöscht?)",
                    expected - 1
                ),
            );
        } else if prev != head_hash {
            push(
                &mut issues,
                head_seq,
                None,
                "Hash des letzten Eintrags passt nicht zum Kettenkopf".into(),
            );
        }
        let unchained = Q::new("SELECT COUNT(*) FROM audit_log WHERE seq IS NULL")
            .fetch_i64(&self.pool)
            .await?;
        Ok(AuditChainReport {
            ok: issues.is_empty(),
            checked,
            unchained: u64::try_from(unchained).unwrap_or(0),
            anchor_seq,
            head_seq,
            head_hash,
            issues,
        })
    }

    /// Löscht Audit-Einträge vor `before` (Aufbewahrung, F116): den Anfang der Kette bis zum
    /// letzten Eintrag vor dem Stichtag, setzt den Anker und protokolliert die Löschung als
    /// Eintrag (`audit-log`/`prune`, Akteur `system`). Liefert die Anzahl gelöschter Einträge.
    pub async fn prune_audit_log(&self, before: OffsetDateTime) -> Result<u64, StoreError> {
        let mut tx = self.pool.begin().await?;
        // Kopf zuerst sperren: kein gleichzeitiges Anhängen während des Löschens.
        Q::new("UPDATE audit_chain SET seq = seq WHERE id = 1")
            .execute(&mut tx)
            .await?;
        let last = Q::new(
            "SELECT seq, hash FROM audit_log WHERE seq IS NOT NULL AND at < $1
             ORDER BY seq DESC LIMIT 1",
        )
        .bind(Val::time(before))
        .fetch_optional(&mut tx)
        .await?;
        let mut deleted = Q::new("DELETE FROM audit_log WHERE seq IS NULL AND at < $1")
            .bind(Val::time(before))
            .execute(&mut tx)
            .await?;
        let mut anchor = None;
        if let Some(last) = last {
            let (seq, hash) = (last.i64("seq")?, last.text("hash")?);
            deleted += Q::new("DELETE FROM audit_log WHERE seq <= $1")
                .bind(Val::int(seq))
                .execute(&mut tx)
                .await?;
            Q::new("UPDATE audit_chain SET anchor_seq = $1, anchor_hash = $2 WHERE id = 1")
                .bind(Val::int(seq))
                .bind(Val::text(hash))
                .execute(&mut tx)
                .await?;
            anchor = Some(seq);
        }
        if deleted > 0 {
            let version = Q::new("SELECT version FROM config_version WHERE id = 1")
                .fetch_i64(&mut tx)
                .await?;
            let mut r = NewAuditRow::new("system", "prune", AUDIT_LOG_KIND, version);
            r.after = Some(serde_json::json!({
                "deleted": deleted,
                "before": before.format(&time::format_description::well_known::Rfc3339).ok(),
                "anchorSeq": anchor,
            }));
            append(&mut tx, &r).await?;
        }
        tx.commit().await?;
        Ok(deleted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_sorts_keys() {
        let a: Value = serde_json::from_str(r#"{"b":1,"a":{"y":[1,{"d":2,"c":3}],"x":"ü"}}"#)
            .unwrap_or_default();
        let mut s = String::new();
        canonical(&a, &mut s);
        assert_eq!(s, r#"{"a":{"x":"ü","y":[1,{"c":3,"d":2}]},"b":1}"#);
    }

    #[test]
    fn hash_depends_on_everything() {
        let r = NewAuditRow::new("user:anna", "create", "team", 3);
        let h = entry_hash(GENESIS_HASH, 1, &r);
        assert_eq!(h.len(), 64);
        assert_eq!(h, entry_hash(GENESIS_HASH, 1, &r));
        assert_ne!(h, entry_hash(GENESIS_HASH, 2, &r));
        assert_ne!(h, entry_hash(&h, 1, &r));
        let mut r2 = r.clone();
        r2.after = Some(serde_json::json!({"x": 1}));
        assert_ne!(h, entry_hash(GENESIS_HASH, 1, &r2));
    }
}
