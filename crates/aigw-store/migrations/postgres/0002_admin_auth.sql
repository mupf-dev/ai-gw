-- SPDX-FileCopyrightText: 2026 ai-gw contributors
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- AIGW-74 (F90): Einladungslinks für vorab angelegte Nutzer. Gespeichert wird nur SHA-256 (hex)
-- des Tokens; einmalig (beim Login gelöscht), mit Ablauf. Je Nutzer höchstens eine Einladung.

CREATE TABLE admin_invitation (
    token_hash TEXT PRIMARY KEY,
    user_id    UUID NOT NULL UNIQUE REFERENCES app_user (id) ON DELETE CASCADE,
    created_by TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX admin_invitation_expires ON admin_invitation (expires_at);

-- Laufender Login mit Einladung: Hash des Einladungs-Tokens (eingelöst erst beim Rücksprung).
ALTER TABLE admin_login_state ADD COLUMN invite_hash TEXT;
