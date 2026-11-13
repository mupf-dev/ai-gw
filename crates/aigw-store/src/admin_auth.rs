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
use time::OffsetDateTime;

use crate::db::{DbRow, Q, Val};
use crate::keys::generate_admin_api_key;
use crate::repo::audit::{AuditEvent, BOOTSTRAP_TOKEN_KIND, append_event};
use crate::repo::{Action, AdminTx, Expect, Row, map_db, now};
use crate::{Store, StoreError};

/// Art im Audit-Log für Admin-API-Keys.
pub const ADMIN_API_KEY_KIND: &str = "admin-api-key";

/// Art im Audit-Log für Einladungslinks (F90).
pub const ADMIN_INVITATION_KIND: &str = "admin-invitation";

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
    /// SHA-256 eines Einladungs-Tokens, mit dem dieser Login begonnen wurde (F90).
    pub invite_hash: Option<KeyHash>,
}

/// Offene Einladung eines Nutzers (F90; das Token selbst wird nicht gespeichert).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvitationRecord {
    /// Eingeladener Nutzer.
    pub user_id: Id,
    /// Akteur, der die Einladung erstellt hat.
    pub created_by: String,
    /// Erstellt.
    pub created_at: OffsetDateTime,
    /// Gültig bis.
    pub expires_at: OffsetDateTime,
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
        let t = now();
        Q::new(
            "INSERT INTO admin_session (id_hash, user_id, groups, created_at, last_seen_at, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(Val::text(id_hash.to_hex()))
        .bind(Val::uuid(user_id))
        .bind(Val::json(&groups)?)
        .bind(Val::time(t))
        .bind(Val::time(t))
        .bind(Val::time(expires_at))
        .execute(&self.pool)
        .await
        .map_err(|e| map_db(e, "admin_session"))?;
        Ok(())
    }

    /// Legt eine Session an und schreibt das Ereignis `event` in **derselben** Transaktion
    /// (F113, `admin.auth.audit_required`): scheitert der Audit-Eintrag, entsteht keine
    /// Session.
    pub async fn create_admin_session_audited(
        &self,
        id_hash: &KeyHash,
        user_id: Id,
        groups: &[String],
        expires_at: OffsetDateTime,
        event: &AuditEvent,
    ) -> Result<(), StoreError> {
        let t = now();
        let mut tx = self.pool.begin().await?;
        let version = Q::new("SELECT version FROM config_version WHERE id = 1")
            .fetch_i64(&mut tx)
            .await?;
        Q::new(
            "INSERT INTO admin_session (id_hash, user_id, groups, created_at, last_seen_at, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(Val::text(id_hash.to_hex()))
        .bind(Val::uuid(user_id))
        .bind(Val::json(&groups)?)
        .bind(Val::time(t))
        .bind(Val::time(t))
        .bind(Val::time(expires_at))
        .execute(&mut tx)
        .await
        .map_err(|e| map_db(e, "admin_session"))?;
        append_event(&mut tx, event, version).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Löscht eine Session und schreibt das Ereignis `event` in derselben Transaktion (F113).
    /// Liefert `false`, wenn es die Session nicht (mehr) gab – dann wird nichts geschrieben.
    pub async fn delete_admin_session_audited(
        &self,
        id_hash: &KeyHash,
        event: &AuditEvent,
    ) -> Result<bool, StoreError> {
        let mut tx = self.pool.begin().await?;
        let n = Q::new("DELETE FROM admin_session WHERE id_hash = $1")
            .bind(Val::text(id_hash.to_hex()))
            .execute(&mut tx)
            .await?;
        if n == 0 {
            return Ok(false);
        }
        let version = Q::new("SELECT version FROM config_version WHERE id = 1")
            .fetch_i64(&mut tx)
            .await?;
        append_event(&mut tx, event, version).await?;
        tx.commit().await?;
        Ok(true)
    }

    // -----------------------------------------------------------------------------------------
    // Einladungen (F90)
    // -----------------------------------------------------------------------------------------

    /// Legt eine Einladung für `user_id` an (ersetzt eine bestehende) – mit Audit-Eintrag
    /// (Art `admin-invitation`, Aktion `create`, ohne Token).
    pub async fn create_invitation(
        &self,
        actor: &str,
        user_id: Id,
        user_name: &str,
        token_hash: &KeyHash,
        expires_at: OffsetDateTime,
    ) -> Result<InvitationRecord, StoreError> {
        let t = now();
        let mut tx = AdminTx::begin(self, actor).await?;
        Q::new("DELETE FROM admin_invitation WHERE user_id = $1")
            .bind(Val::uuid(user_id))
            .execute(&mut tx.tx)
            .await?;
        Q::new(
            "INSERT INTO admin_invitation (token_hash, user_id, created_by, created_at, expires_at)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(Val::text(token_hash.to_hex()))
        .bind(Val::uuid(user_id))
        .bind(Val::text(actor))
        .bind(Val::time(t))
        .bind(Val::time(expires_at))
        .execute(&mut tx.tx)
        .await
        .map_err(|e| map_db(e, ADMIN_INVITATION_KIND))?;
        let rec = InvitationRecord {
            user_id,
            created_by: actor.to_owned(),
            created_at: crate::repo::trunc_millis(t),
            expires_at: crate::repo::trunc_millis(expires_at),
        };
        let details = serde_json::json!({
            "expiresAt": expires_at
                .format(&time::format_description::well_known::Rfc3339)
                .ok(),
        });
        tx.audit_kind(
            Action::Create,
            ADMIN_INVITATION_KIND,
            Some(user_id),
            user_name,
            None::<&()>,
            Some(&details),
        )
        .await?;
        tx.commit().await?;
        Ok(rec)
    }

    /// Offene Einladung eines Nutzers (auch abgelaufene; der Aufrufer prüft `expires_at`).
    pub async fn get_invitation(
        &self,
        user_id: Id,
    ) -> Result<Option<InvitationRecord>, StoreError> {
        Q::new("SELECT * FROM admin_invitation WHERE user_id = $1")
            .bind(Val::uuid(user_id))
            .fetch_optional(&self.pool)
            .await?
            .map(|r| {
                let r = Row(&r);
                Ok(InvitationRecord {
                    user_id: r.uuid("user_id")?,
                    created_by: r.text("created_by")?,
                    created_at: r.time("created_at")?,
                    expires_at: r.time("expires_at")?,
                })
            })
            .transpose()
    }

    /// Widerruft die Einladung eines Nutzers (mit Audit-Eintrag `delete`); `false`, wenn es
    /// keine gab.
    pub async fn delete_invitation(
        &self,
        actor: &str,
        user_id: Id,
        user_name: &str,
    ) -> Result<bool, StoreError> {
        if self.get_invitation(user_id).await?.is_none() {
            return Ok(false);
        }
        let mut tx = AdminTx::begin(self, actor).await?;
        let n = Q::new("DELETE FROM admin_invitation WHERE user_id = $1")
            .bind(Val::uuid(user_id))
            .execute(&mut tx.tx)
            .await?;
        tx.audit_kind(
            Action::Delete,
            ADMIN_INVITATION_KIND,
            Some(user_id),
            user_name,
            None::<&()>,
            None::<&()>,
        )
        .await?;
        tx.commit().await?;
        Ok(n > 0)
    }

    /// Löst eine Einladung ein (einmalig): löscht sie und liefert den Nutzer, wenn sie zum
    /// Zeitpunkt `at` noch gilt. Abgelaufene Einladungen werden dabei ebenfalls entfernt.
    pub async fn take_invitation(
        &self,
        token_hash: &KeyHash,
        at: OffsetDateTime,
    ) -> Result<Option<Id>, StoreError> {
        let row = Q::new(
            "DELETE FROM admin_invitation WHERE token_hash = $1 RETURNING user_id, expires_at",
        )
        .bind(Val::text(token_hash.to_hex()))
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let r = Row(&row);
        let expires = r.time("expires_at")?;
        (expires > at).then(|| r.uuid("user_id")).transpose()
    }

    /// Gibt es eine (noch nicht abgelaufene) Einladung zu diesem Token? (ohne einzulösen)
    pub async fn invitation_valid(
        &self,
        token_hash: &KeyHash,
        at: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let n = Q::new(
            "SELECT COUNT(*) FROM admin_invitation WHERE token_hash = $1 AND expires_at > $2",
        )
        .bind(Val::text(token_hash.to_hex()))
        .bind(Val::time(at))
        .fetch_i64(&self.pool)
        .await?;
        Ok(n > 0)
    }

    /// Liest eine Session (ohne Prüfung von Ablauf/Leerlauf).
    pub async fn get_admin_session(
        &self,
        id_hash: &KeyHash,
    ) -> Result<Option<SessionRecord>, StoreError> {
        let row = Q::new("SELECT * FROM admin_session WHERE id_hash = $1")
            .bind(Val::text(id_hash.to_hex()))
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| {
            let r = Row(&r);
            Ok(SessionRecord {
                user_id: r.uuid("user_id")?,
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
        Q::new("UPDATE admin_session SET last_seen_at = $1 WHERE id_hash = $2")
            .bind(Val::time(at))
            .bind(Val::text(id_hash.to_hex()))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Löscht eine Session (Abmelden, Ablauf).
    pub async fn delete_admin_session(&self, id_hash: &KeyHash) -> Result<(), StoreError> {
        Q::new("DELETE FROM admin_session WHERE id_hash = $1")
            .bind(Val::text(id_hash.to_hex()))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Löscht alle Sessions eines Nutzers (z. B. nach Rollenentzug).
    pub async fn delete_admin_sessions_of(&self, user_id: Id) -> Result<u64, StoreError> {
        Ok(Q::new("DELETE FROM admin_session WHERE user_id = $1")
            .bind(Val::uuid(user_id))
            .execute(&self.pool)
            .await?)
    }

    /// Entfernt abgelaufene bzw. zu lange inaktive Sessions und alte Login-Zustände.
    pub async fn purge_admin_auth(
        &self,
        at: OffsetDateTime,
        idle_before: OffsetDateTime,
        login_before: OffsetDateTime,
    ) -> Result<u64, StoreError> {
        let mut n = Q::new("DELETE FROM admin_session WHERE expires_at <= $1 OR last_seen_at < $2")
            .bind(Val::time(at))
            .bind(Val::time(idle_before))
            .execute(&self.pool)
            .await?;
        n += Q::new("DELETE FROM admin_login_state WHERE created_at < $1")
            .bind(Val::time(login_before))
            .execute(&self.pool)
            .await?;
        n += Q::new("DELETE FROM admin_invitation WHERE expires_at <= $1")
            .bind(Val::time(at))
            .execute(&self.pool)
            .await?;
        Ok(n)
    }

    /// Speichert einen laufenden Login.
    pub async fn put_login_state(
        &self,
        state_hash: &KeyHash,
        state: &LoginState,
    ) -> Result<(), StoreError> {
        Q::new(
            "INSERT INTO admin_login_state (state_hash, nonce, code_verifier, return_to, created_at,
                                            invite_hash)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(Val::text(state_hash.to_hex()))
        .bind(Val::text(state.nonce.clone()))
        .bind(Val::text(state.code_verifier.clone()))
        .bind(Val::text(state.return_to.clone()))
        .bind(Val::time(state.created_at))
        .bind(Val::Text(state.invite_hash.as_ref().map(KeyHash::to_hex)))
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
        let row = Q::new(
            "DELETE FROM admin_login_state WHERE state_hash = $1
             RETURNING nonce, code_verifier, return_to, created_at, invite_hash",
        )
        .bind(Val::text(state_hash.to_hex()))
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| {
            let r = Row(&r);
            Ok(LoginState {
                nonce: r.text("nonce")?,
                code_verifier: r.text("code_verifier")?,
                return_to: r.text("return_to")?,
                created_at: r.time("created_at")?,
                invite_hash: r
                    .opt_text("invite_hash")?
                    .map(|h| {
                        KeyHash::from_hex(&h)
                            .ok_or_else(|| StoreError::Corrupt("ungültiger Einladungs-Hash".into()))
                    })
                    .transpose()?,
            })
        })
        .transpose()
    }

    // -----------------------------------------------------------------------------------------
    // Bootstrap-Token
    // -----------------------------------------------------------------------------------------

    /// `true`, wenn das Bootstrap-Token (Modus `auto`) dauerhaft abgeschaltet wurde.
    pub async fn bootstrap_disabled(&self) -> Result<bool, StoreError> {
        let n = Q::new("SELECT COUNT(*) FROM admin_auth_state WHERE name = $1")
            .bind(Val::text(BOOTSTRAP_DISABLED))
            .fetch_i64(&self.pool)
            .await?;
        Ok(n > 0)
    }

    /// Schaltet das Bootstrap-Token dauerhaft ab (idempotent; der erste Grund bleibt).
    /// Liefert `true`, wenn es bisher aktiv war.
    pub async fn disable_bootstrap(&self, reason: &str) -> Result<bool, StoreError> {
        let n = Q::new(
            "INSERT INTO admin_auth_state (name, value, updated_at) VALUES ($1, $2, $3)
             ON CONFLICT (name) DO NOTHING",
        )
        .bind(Val::text(BOOTSTRAP_DISABLED))
        .bind(Val::text(reason))
        .bind(Val::time(now()))
        .execute(&self.pool)
        .await?;
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

fn admin_key_from_row(row: &DbRow) -> Result<AdminApiKeyRecord, StoreError> {
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
#[derive(Clone)]
pub struct CreatedAdminApiKey {
    /// Gespeicherter Key.
    pub key: AdminApiKeyRecord,
    /// Klartext `sk-aigw-admin-…`.
    pub secret: String,
}

// AIGW-58: `Debug` ohne Klartext (Schutz vor versehentlichem Loggen).
impl std::fmt::Debug for CreatedAdminApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreatedAdminApiKey")
            .field("key", &self.key)
            .field("secret", &"***")
            .finish()
    }
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
        let t = now();
        let mut tx = AdminTx::begin(self, actor).await?;
        Q::new(
            "INSERT INTO admin_api_key (id, name, prefix, key_hash, description, roles, expires_at,
                                        created_by, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(Val::uuid(id))
        .bind(Val::text(name))
        .bind(Val::text(generated.prefix.clone()))
        .bind(Val::text(generated.hash.to_hex()))
        .bind(Val::Text(spec.description.clone()))
        .bind(Val::json(&spec.roles)?)
        .bind(Val::opt_time(spec.expires_at))
        .bind(Val::text(actor))
        .bind(Val::time(t))
        .bind(Val::time(t))
        .execute(&mut tx.tx)
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
        expect: Expect,
    ) -> Result<AdminApiKeyRecord, StoreError> {
        let mut tx = AdminTx::begin(self, actor).await?;
        let before = fetch_admin_key(&mut tx, name).await?;
        tx.check_expect("admin_api_key", ADMIN_API_KEY_KIND, name, before.id, expect)
            .await?;
        Q::new(
            "UPDATE admin_api_key SET description = $1, roles = $2, expires_at = $3, updated_at = $4
             WHERE name = $5",
        )
        .bind(Val::Text(spec.description.clone()))
        .bind(Val::json(&spec.roles)?)
        .bind(Val::opt_time(spec.expires_at))
        .bind(Val::time(AdminTx::next_version(Some(before.updated_at))))
        .bind(Val::text(name))
        .execute(&mut tx.tx)
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
    pub async fn delete_admin_api_key(
        &self,
        actor: &str,
        name: &str,
        expect: Expect,
    ) -> Result<(), StoreError> {
        let mut tx = AdminTx::begin(self, actor).await?;
        let before = fetch_admin_key(&mut tx, name).await?;
        tx.check_expect("admin_api_key", ADMIN_API_KEY_KIND, name, before.id, expect)
            .await?;
        Q::new("DELETE FROM admin_api_key WHERE name = $1")
            .bind(Val::text(name))
            .execute(&mut tx.tx)
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
        Q::new("SELECT * FROM admin_api_key WHERE name = $1")
            .bind(Val::text(name))
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(admin_key_from_row)
            .transpose()
    }

    /// Alle Admin-API-Keys, nach Name sortiert.
    pub async fn list_admin_api_keys(&self) -> Result<Vec<AdminApiKeyRecord>, StoreError> {
        Q::new("SELECT * FROM admin_api_key ORDER BY name")
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
        Q::new("SELECT * FROM admin_api_key WHERE key_hash = $1")
            .bind(Val::text(hash.to_hex()))
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
        let n = Q::new(
            "UPDATE admin_api_key SET last_used_at = $1 WHERE id = $2 AND last_used_at IS NULL",
        )
        .bind(Val::time(at))
        .bind(Val::uuid(key.id))
        .execute(&self.pool)
        .await?;
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
        Q::new("UPDATE admin_api_key SET last_used_at = $1 WHERE id = $2")
            .bind(Val::time(at))
            .bind(Val::uuid(id))
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

async fn fetch_admin_key(tx: &mut AdminTx, name: &str) -> Result<AdminApiKeyRecord, StoreError> {
    let row = Q::new("SELECT * FROM admin_api_key WHERE name = $1")
        .bind(Val::text(name))
        .fetch_optional(&mut tx.tx)
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
            invite_hash: Some(h("invite")),
        };
        s.put_login_state(&h("state"), &st).await.unwrap();
        assert_eq!(s.take_login_state(&h("state")).await.unwrap(), Some(st));
        assert_eq!(s.take_login_state(&h("state")).await.unwrap(), None);
    }

    #[tokio::test]
    async fn invitations_are_single_use_and_expire() {
        let s = store().await;
        let user = s
            .create("t", ObjectMeta::named("anna"), UserSpec::default())
            .await
            .unwrap();
        let exp = now() + Duration::hours(1);
        s.create_invitation("bootstrap-admin", user.id, "anna", &h("inv1"), exp)
            .await
            .unwrap();
        assert!(s.invitation_valid(&h("inv1"), now()).await.unwrap());
        // eine neue Einladung ersetzt die alte
        s.create_invitation("bootstrap-admin", user.id, "anna", &h("inv2"), exp)
            .await
            .unwrap();
        assert!(!s.invitation_valid(&h("inv1"), now()).await.unwrap());
        assert_eq!(s.take_invitation(&h("inv1"), now()).await.unwrap(), None);
        assert_eq!(
            s.get_invitation(user.id).await.unwrap().unwrap().created_by,
            "bootstrap-admin"
        );
        // einmalig
        assert_eq!(
            s.take_invitation(&h("inv2"), now()).await.unwrap(),
            Some(user.id)
        );
        assert_eq!(s.take_invitation(&h("inv2"), now()).await.unwrap(), None);
        // abgelaufen: nicht einlösbar (und entfernt)
        s.create_invitation("x", user.id, "anna", &h("inv3"), exp)
            .await
            .unwrap();
        assert_eq!(
            s.take_invitation(&h("inv3"), exp + Duration::seconds(1))
                .await
                .unwrap(),
            None
        );
        assert!(s.get_invitation(user.id).await.unwrap().is_none());
        // widerrufen, Audit ohne Token
        s.create_invitation("x", user.id, "anna", &h("inv4"), exp)
            .await
            .unwrap();
        assert!(s.delete_invitation("x", user.id, "anna").await.unwrap());
        assert!(!s.delete_invitation("x", user.id, "anna").await.unwrap());
        let log = s.audit_log(50).await.unwrap();
        assert!(
            log.iter()
                .any(|e| e.kind == ADMIN_INVITATION_KIND && e.action == "create")
        );
        assert!(
            log.iter()
                .any(|e| e.kind == ADMIN_INVITATION_KIND && e.action == "delete")
        );
        let text = serde_json::to_string(&log).unwrap();
        assert!(
            !text.contains(&h("inv4").to_hex()),
            "kein Token-Hash im Audit"
        );
        // Nutzer löschen entfernt Einladungen
        s.create_invitation("x", user.id, "anna", &h("inv5"), exp)
            .await
            .unwrap();
        s.delete("t", aigw_core::resource::ResourceKind::User, "anna")
            .await
            .unwrap();
        assert!(!s.invitation_valid(&h("inv5"), now()).await.unwrap());
    }

    #[tokio::test]
    async fn audited_sessions_write_event_in_same_transaction() {
        let s = store().await;
        let user = s
            .create("t", ObjectMeta::named("anna"), UserSpec::default())
            .await
            .unwrap();
        let ev = |action: &str| AuditEvent {
            actor: "user:anna".into(),
            action: action.into(),
            kind: crate::repo::audit::ADMIN_SESSION_KIND.into(),
            object_id: Some(user.id),
            object_name: Some("anna".into()),
            details: None,
        };
        let exp = now() + Duration::hours(1);
        s.create_admin_session_audited(&h("sid"), user.id, &[], exp, &ev("login"))
            .await
            .unwrap();
        assert!(s.get_admin_session(&h("sid")).await.unwrap().is_some());
        assert!(
            s.delete_admin_session_audited(&h("sid"), &ev("logout"))
                .await
                .unwrap()
        );
        assert!(
            !s.delete_admin_session_audited(&h("sid"), &ev("logout"))
                .await
                .unwrap()
        );
        let actions: Vec<String> = s
            .audit_log(10)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == crate::repo::audit::ADMIN_SESSION_KIND)
            .map(|e| e.action)
            .collect();
        assert_eq!(actions, vec!["logout".to_owned(), "login".to_owned()]);
        // Scheitert der Audit-Eintrag, entsteht keine Session (fail-closed).
        sqlx::query("DROP TABLE audit_log")
            .execute(match &s.pool {
                crate::db::Pool::Sqlite(p) => p,
                crate::db::Pool::Postgres(_) => unreachable!(),
            })
            .await
            .unwrap();
        assert!(
            s.create_admin_session_audited(&h("sid2"), user.id, &[], exp, &ev("login"))
                .await
                .is_err()
        );
        assert!(s.get_admin_session(&h("sid2")).await.unwrap().is_none());
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
        let u = s
            .update_admin_api_key("me", "ci", spec2, Expect::Any)
            .await
            .unwrap();
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
        s.delete_admin_api_key("me", "ci", Expect::Any)
            .await
            .unwrap();
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
