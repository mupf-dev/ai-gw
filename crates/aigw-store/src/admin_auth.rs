// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Anmeldung an der Admin-API (AIGW-32): Sessions, laufende OIDC-Logins, Admin-API-Keys und
//! der Zustand des Bootstrap-Tokens.
//!
//! Sessions und Login-Zustände sind keine Konfiguration: sie werden direkt geschrieben (ohne
//! `config_version`/Audit). Admin-API-Keys dagegen sind Berechtigungen und laufen wie jede
//! Admin-Änderung über eine Transaktion mit Audit-Eintrag (Art `admin-api-key`).
//!
//! Geheimnisse liegen nie im Klartext in der Datenbank: von Session-IDs, `state`-Werten und
//! Admin-Keys wird nur SHA-256 gespeichert. Nonce und PKCE-Verifier eines laufenden Logins
//! müssen lesbar bleiben; sie leben höchstens wenige Minuten und werden beim Rückruf gelöscht.

use aigw_core::resource::{Id, RoleBinding, new_id, validate_name};
use aigw_core::snapshot::KeyHash;
use sqlx::any::AnyRow;
use time::OffsetDateTime;

use crate::keys::generate_admin_api_key;
use crate::repo::audit::{AuditEvent, BOOTSTRAP_TOKEN_KIND};
use crate::repo::{Action, AdminTx, Row, map_db, millis, now, parse_id};
use crate::{Store, StoreError};

/// Art im Audit-Log für Admin-API-Keys.
pub const ADMIN_API_KEY_KIND: &str = "admin-api-key";

/// Schlüssel in `admin_auth_state`: Bootstrap-Token dauerhaft abgeschaltet.
const BOOTSTRAP_DISABLED: &str = "bootstrap_disabled";

// ---------------------------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------------------------

/// Gespeicherte Session (ohne Session-ID, die nur der Browser kennt).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    /// Angemeldeter Nutzer (`app_user.id`).
    pub user_id: Id,
    /// IdP-Gruppen beim Login.
    pub groups: Vec<String>,
    /// Anmeldung.
    pub created_at: OffsetDateTime,
    /// Letzte Aktivität (gedrosselt aktualisiert).
    pub last_seen_at: OffsetDateTime,
    /// Absoluter Ablauf.
    pub expires_at: OffsetDateTime,
}

/// Ein laufender OIDC-Login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginState {
    /// Nonce für das ID-Token.
    pub nonce: String,
    /// PKCE-Verifier.
    pub code_verifier: String,
    /// Ziel nach dem Login (relativer Pfad unter `/admin/`).
    pub return_to: String,
    /// Beginn des Logins.
    pub created_at: OffsetDateTime,
}

impl Store {
    /// Legt eine Session an.
    pub async fn create_admin_session(
        &self,
        id_hash: &KeyHash,
        user_id: Id,
        groups: &[String],
        expires_at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        let t = millis(now());
        sqlx::query(
            "INSERT INTO admin_session (id_hash, user_id, groups, created_at, last_seen_at, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(id_hash.to_hex())
        .bind(user_id.to_string())
        .bind(serde_json::to_string(groups)?)
        .bind(t)
        .bind(t)
        .bind(millis(expires_at))
        .execute(&self.pool)
        .await
        .map_err(|e| map_db(e, "admin_session"))?;
        Ok(())
    }

    /// Liest eine Session (ohne Prüfung von Ablauf/Leerlauf).
    pub async fn get_admin_session(
        &self,
        id_hash: &KeyHash,
    ) -> Result<Option<SessionRecord>, StoreError> {
        let row = sqlx::query("SELECT * FROM admin_session WHERE id_hash = $1")
            .bind(id_hash.to_hex())
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| {
            let r = Row(&r);
            Ok(SessionRecord {
                user_id: parse_id(&r.text("user_id")?)?,
                groups: r.json("groups")?,
                created_at: r.time("created_at")?,
                last_seen_at: r.time("last_seen_at")?,
                expires_at: r.time("expires_at")?,
            })
        })
        .transpose()
    }

    /// Vermerkt Aktivität einer Session.
    pub async fn touch_admin_session(
        &self,
        id_hash: &KeyHash,
        at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        sqlx::query("UPDATE admin_session SET last_seen_at = $1 WHERE id_hash = $2")
            .bind(millis(at))
            .bind(id_hash.to_hex())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Löscht eine Session (Abmelden, Ablauf).
    pub async fn delete_admin_session(&self, id_hash: &KeyHash) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM admin_session WHERE id_hash = $1")
            .bind(id_hash.to_hex())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Löscht alle Sessions eines Nutzers (z. B. nach Rollenentzug).
    pub async fn delete_admin_sessions_of(&self, user_id: Id) -> Result<u64, StoreError> {
        Ok(sqlx::query("DELETE FROM admin_session WHERE user_id = $1")
            .bind(user_id.to_string())
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    /// Entfernt abgelaufene bzw. zu lange inaktive Sessions und alte Login-Zustände.
    pub async fn purge_admin_auth(
        &self,
        at: OffsetDateTime,
        idle_before: OffsetDateTime,
        login_before: OffsetDateTime,
    ) -> Result<u64, StoreError> {
        let mut n =
            sqlx::query("DELETE FROM admin_session WHERE expires_at <= $1 OR last_seen_at < $2")
                .bind(millis(at))
                .bind(millis(idle_before))
                .execute(&self.pool)
                .await?
                .rows_affected();
        n += sqlx::query("DELETE FROM admin_login_state WHERE created_at < $1")
            .bind(millis(login_before))
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(n)
    }

    /// Speichert einen laufenden Login.
    pub async fn put_login_state(
        &self,
        state_hash: &KeyHash,
        state: &LoginState,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO admin_login_state (state_hash, nonce, code_verifier, return_to, created_at)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(state_hash.to_hex())
        .bind(state.nonce.clone())
        .bind(state.code_verifier.clone())
        .bind(state.return_to.clone())
        .bind(millis(state.created_at))
        .execute(&self.pool)
        .await
        .map_err(|e| map_db(e, "admin_login_state"))?;
        Ok(())
    }

    /// Liest **und löscht** einen laufenden Login (jeder `state` gilt genau einmal).
    pub async fn take_login_state(
        &self,
        state_hash: &KeyHash,
    ) -> Result<Option<LoginState>, StoreError> {
        let row = sqlx::query(
            "DELETE FROM admin_login_state WHERE state_hash = $1
             RETURNING nonce, code_verifier, return_to, created_at",
        )
        .bind(state_hash.to_hex())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| {
            let r = Row(&r);
            Ok(LoginState {
                nonce: r.text("nonce")?,
                code_verifier: r.text("code_verifier")?,
                return_to: r.text("return_to")?,
                created_at: r.time("created_at")?,
            })
        })
        .transpose()
    }

    // -----------------------------------------------------------------------------------------
    // Bootstrap-Token
    // -----------------------------------------------------------------------------------------

    /// `true`, wenn das Bootstrap-Token (Modus `auto`) dauerhaft abgeschaltet wurde.
    pub async fn bootstrap_disabled(&self) -> Result<bool, StoreError> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM admin_auth_state WHERE name = $1")
            .bind(BOOTSTRAP_DISABLED)
            .fetch_one(&self.pool)
            .await?;
        Ok(n > 0)
    }

    /// Schaltet das Bootstrap-Token dauerhaft ab (idempotent; der erste Grund bleibt).
    /// Liefert `true`, wenn es bisher aktiv war.
    pub async fn disable_bootstrap(&self, reason: &str) -> Result<bool, StoreError> {
        let n = sqlx::query(
            "INSERT INTO admin_auth_state (name, value, updated_at) VALUES ($1, $2, $3)
             ON CONFLICT (name) DO NOTHING",
        )
        .bind(BOOTSTRAP_DISABLED)
        .bind(reason.to_owned())
        .bind(millis(now()))
        .execute(&self.pool)
        .await?
        .rows_affected();
        if n > 0 {
            self.record_audit_event(&AuditEvent {
                actor: "system".into(),
                action: "disable".into(),
                kind: BOOTSTRAP_TOKEN_KIND.into(),
                object_id: None,
                object_name: None,
                details: Some(serde_json::json!({"reason": reason})),
            })
            .await?;
        }
        Ok(n > 0)
    }
}

// ---------------------------------------------------------------------------------------------
// Admin-API-Keys
// ---------------------------------------------------------------------------------------------

/// Einstellbare Felder eines Admin-API-Keys.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminApiKeySpec {
    /// Beschreibung (Zweck, Eigentümer).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Rollen (optional eingeschränkt auf Organisation/Team).
    pub roles: Vec<RoleBinding>,
    /// Ablauf.
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub expires_at: Option<OffsetDateTime>,
}

/// Gespeicherter Admin-API-Key (ohne Hash und Klartext).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminApiKeyRecord {
    /// ID.
    pub id: Id,
    /// Name (eindeutig).
    pub name: String,
    /// Sichtbares Präfix (12 Zeichen).
    pub prefix: String,
    /// Einstellungen.
    #[serde(flatten)]
    pub spec: AdminApiKeySpec,
    /// Angelegt von (Audit-Akteur).
    pub created_by: String,
    /// Angelegt.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Zuletzt geändert.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    /// Zuletzt benutzt (gedrosselt).
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_used_at: Option<OffsetDateTime>,
}

fn admin_key_from_row(row: &AnyRow) -> Result<AdminApiKeyRecord, StoreError> {
    let r = Row(row);
    Ok(AdminApiKeyRecord {
        id: r.id()?,
        name: r.text("name")?,
        prefix: r.text("prefix")?,
        spec: AdminApiKeySpec {
            description: r.opt_text("description")?,
            roles: r.json("roles")?,
            expires_at: r.opt_time("expires_at")?,
        },
        created_by: r.text("created_by")?,
        created_at: r.time("created_at")?,
        updated_at: r.time("updated_at")?,
        last_used_at: r.opt_time("last_used_at")?,
    })
}

fn not_found(name: &str) -> StoreError {
    StoreError::Invalid(format!("Admin-API-Key {name:?} nicht gefunden"))
}

/// Ergebnis des Anlegens: Datensatz und Klartext (nur einmal).
#[derive(Debug, Clone)]
pub struct CreatedAdminApiKey {
    /// Gespeicherter Key.
    pub key: AdminApiKeyRecord,
    /// Klartext `sk-aigw-admin-…`.
    pub secret: String,
}

impl Store {
    /// Legt einen Admin-API-Key an (Audit: `create`, Art `admin-api-key`).
    pub async fn create_admin_api_key(
        &self,
        actor: &str,
        name: &str,
        spec: AdminApiKeySpec,
    ) -> Result<CreatedAdminApiKey, StoreError> {
        validate_name(name).map_err(StoreError::Invalid)?;
        let generated = generate_admin_api_key()?;
        let id = new_id();
        let t = millis(now());
        let mut tx = AdminTx::begin(self, actor).await?;
        sqlx::query(
            "INSERT INTO admin_api_key (id, name, prefix, key_hash, description, roles, expires_at,
                                        created_by, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(id.to_string())
        .bind(name.to_owned())
        .bind(generated.prefix.clone())
        .bind(generated.hash.to_hex())
        .bind(spec.description.clone())
        .bind(serde_json::to_string(&spec.roles)?)
        .bind(spec.expires_at.map(millis))
        .bind(actor.to_owned())
        .bind(t)
        .bind(t)
        .execute(&mut *tx.tx)
        .await
        .map_err(|e| map_db(e, ADMIN_API_KEY_KIND))?;
        let rec = fetch_admin_key(&mut tx, name).await?;
        tx.audit_kind(
            Action::Create,
            ADMIN_API_KEY_KIND,
            Some(id),
            name,
            None::<&()>,
            Some(&rec),
        )
        .await?;
        tx.commit().await?;
        Ok(CreatedAdminApiKey {
            key: rec,
            secret: generated.secret,
        })
    }

    /// Ändert Beschreibung, Rollen und Ablauf (Audit: `update`).
    pub async fn update_admin_api_key(
        &self,
        actor: &str,
        name: &str,
        spec: AdminApiKeySpec,
    ) -> Result<AdminApiKeyRecord, StoreError> {
        let mut tx = AdminTx::begin(self, actor).await?;
        let before = fetch_admin_key(&mut tx, name).await?;
        sqlx::query(
            "UPDATE admin_api_key SET description = $1, roles = $2, expires_at = $3, updated_at = $4
             WHERE name = $5",
        )
        .bind(spec.description.clone())
        .bind(serde_json::to_string(&spec.roles)?)
        .bind(spec.expires_at.map(millis))
        .bind(millis(now()))
        .bind(name.to_owned())
        .execute(&mut *tx.tx)
        .await?;
        let after = fetch_admin_key(&mut tx, name).await?;
        tx.audit_kind(
            Action::Update,
            ADMIN_API_KEY_KIND,
            Some(after.id),
            name,
            Some(&before),
            Some(&after),
        )
        .await?;
        tx.commit().await?;
        Ok(after)
    }

    /// Löscht einen Admin-API-Key (wirkt sofort; Audit: `delete`).
    pub async fn delete_admin_api_key(&self, actor: &str, name: &str) -> Result<(), StoreError> {
        let mut tx = AdminTx::begin(self, actor).await?;
        let before = fetch_admin_key(&mut tx, name).await?;
        sqlx::query("DELETE FROM admin_api_key WHERE name = $1")
            .bind(name.to_owned())
            .execute(&mut *tx.tx)
            .await?;
        tx.audit_kind(
            Action::Delete,
            ADMIN_API_KEY_KIND,
            Some(before.id),
            name,
            Some(&before),
            None::<&()>,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Liest einen Admin-API-Key; `None`, wenn es ihn nicht gibt.
    pub async fn get_admin_api_key(
        &self,
        name: &str,
    ) -> Result<Option<AdminApiKeyRecord>, StoreError> {
        sqlx::query("SELECT * FROM admin_api_key WHERE name = $1")
            .bind(name.to_owned())
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(admin_key_from_row)
            .transpose()
    }

    /// Alle Admin-API-Keys, nach Name sortiert.
    pub async fn list_admin_api_keys(&self) -> Result<Vec<AdminApiKeyRecord>, StoreError> {
        sqlx::query("SELECT * FROM admin_api_key ORDER BY name")
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(admin_key_from_row)
            .collect()
    }

    /// Sucht einen Admin-API-Key über den SHA-256 seines Klartexts.
    pub async fn find_admin_api_key(
        &self,
        hash: &KeyHash,
    ) -> Result<Option<AdminApiKeyRecord>, StoreError> {
        sqlx::query("SELECT * FROM admin_api_key WHERE key_hash = $1")
            .bind(hash.to_hex())
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(admin_key_from_row)
            .transpose()
    }

    /// Vermerkt die **erste** Nutzung eines Admin-API-Keys; `true` nur für genau einen Aufruf
    /// (auch bei gleichzeitigen Anfragen mehrerer Instanzen) – dann steht ein Ereignis
    /// `first-use` im Audit-Log.
    pub async fn touch_admin_api_key_first(
        &self,
        key: &AdminApiKeyRecord,
        at: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let n = sqlx::query(
            "UPDATE admin_api_key SET last_used_at = $1 WHERE id = $2 AND last_used_at IS NULL",
        )
        .bind(millis(at))
        .bind(key.id.to_string())
        .execute(&self.pool)
        .await?
        .rows_affected();
        if n > 0 {
            self.record_audit_event(&AuditEvent {
                actor: format!("admin-key:{}", key.name),
                action: "first-use".into(),
                kind: ADMIN_API_KEY_KIND.into(),
                object_id: Some(key.id),
                object_name: Some(key.name.clone()),
                details: None,
            })
            .await?;
        }
        Ok(n > 0)
    }

    /// Vermerkt die Nutzung eines Admin-API-Keys.
    pub async fn touch_admin_api_key(&self, id: Id, at: OffsetDateTime) -> Result<(), StoreError> {
        sqlx::query("UPDATE admin_api_key SET last_used_at = $1 WHERE id = $2")
            .bind(millis(at))
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

async fn fetch_admin_key(tx: &mut AdminTx, name: &str) -> Result<AdminApiKeyRecord, StoreError> {
    let row = sqlx::query("SELECT * FROM admin_api_key WHERE name = $1")
        .bind(name.to_owned())
        .fetch_optional(&mut *tx.tx)
        .await?
        .ok_or_else(|| not_found(name))?;
    admin_key_from_row(&row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigw_core::resource::{ObjectMeta, Role, UserSpec};
    use time::Duration;

    async fn store() -> Store {
        let s = Store::connect("sqlite::memory:").await.unwrap();
        s.migrate().await.unwrap();
        s
    }

    fn h(s: &str) -> KeyHash {
        crate::keys::hash_api_key(s)
    }

    #[tokio::test]
    async fn sessions_roundtrip_and_purge() {
        let s = store().await;
        let user = s
            .create("t", ObjectMeta::named("anna"), UserSpec::default())
            .await
            .unwrap();
        let exp = now() + Duration::hours(1);
        s.create_admin_session(&h("sid"), user.id, &["g1".into()], exp)
            .await
            .unwrap();
        let rec = s.get_admin_session(&h("sid")).await.unwrap().unwrap();
        assert_eq!(rec.user_id, user.id);
        assert_eq!(rec.groups, vec!["g1".to_owned()]);
        assert!(s.get_admin_session(&h("other")).await.unwrap().is_none());
        // Nutzer löschen entfernt seine Sessions (ON DELETE CASCADE) – oder sie verweist ins
        // Leere; beides macht die Session ungültig.
        assert_eq!(s.delete_admin_sessions_of(user.id).await.unwrap(), 1);
        s.create_admin_session(&h("sid2"), user.id, &[], now())
            .await
            .unwrap();
        let n = s
            .purge_admin_auth(
                now() + Duration::seconds(1),
                now() - Duration::hours(1),
                now(),
            )
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn login_state_is_single_use() {
        let s = store().await;
        let st = LoginState {
            nonce: "n".into(),
            code_verifier: "v".into(),
            return_to: "/admin/".into(),
            created_at: crate::repo::trunc_millis(now()),
        };
        s.put_login_state(&h("state"), &st).await.unwrap();
        assert_eq!(s.take_login_state(&h("state")).await.unwrap(), Some(st));
        assert_eq!(s.take_login_state(&h("state")).await.unwrap(), None);
    }

    #[tokio::test]
    async fn bootstrap_latch() {
        let s = store().await;
        assert!(!s.bootstrap_disabled().await.unwrap());
        assert!(s.disable_bootstrap("test").await.unwrap());
        assert!(!s.disable_bootstrap("nochmal").await.unwrap());
        assert!(s.bootstrap_disabled().await.unwrap());
    }

    #[tokio::test]
    async fn admin_keys_crud_with_audit() {
        let s = store().await;
        let spec = AdminApiKeySpec {
            description: Some("CI".into()),
            roles: vec![RoleBinding {
                role: Role::Viewer,
                organization: None,
                team: None,
            }],
            expires_at: None,
        };
        let c = s
            .create_admin_api_key("me", "ci", spec.clone())
            .await
            .unwrap();
        assert!(c.secret.starts_with("sk-aigw-admin-"));
        let found = s
            .find_admin_api_key(&crate::keys::hash_api_key(&c.secret))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.name, "ci");
        assert_eq!(found.spec.roles, spec.roles);
        assert!(matches!(
            s.create_admin_api_key("me", "ci", spec.clone()).await,
            Err(StoreError::Conflict(_))
        ));
        let mut spec2 = spec.clone();
        spec2.description = None;
        let u = s.update_admin_api_key("me", "ci", spec2).await.unwrap();
        assert_eq!(u.spec.description, None);
        s.touch_admin_api_key(u.id, now()).await.unwrap();
        assert!(
            s.get_admin_api_key("ci")
                .await
                .unwrap()
                .unwrap()
                .last_used_at
                .is_some()
        );
        s.delete_admin_api_key("me", "ci").await.unwrap();
        assert!(s.get_admin_api_key("ci").await.unwrap().is_none());
        assert!(s.list_admin_api_keys().await.unwrap().is_empty());
        let log = s.audit_log(10).await.unwrap();
        let kinds: Vec<_> = log
            .iter()
            .filter(|e| e.kind == ADMIN_API_KEY_KIND)
            .map(|e| e.action.clone())
            .collect();
        assert_eq!(kinds.len(), 3);
        // Audit enthält nie den Hash.
        assert!(
            log.iter()
                .all(|e| { e.after.as_ref().is_none_or(|v| v.get("keyHash").is_none()) })
        );
    }
}
