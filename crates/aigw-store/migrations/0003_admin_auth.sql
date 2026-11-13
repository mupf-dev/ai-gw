-- SPDX-FileCopyrightText: 2026 ai-gw contributors
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- AIGW-32: Anmeldung an der Admin-API – Sessions (OIDC-Login), laufende Logins (state/nonce/
-- PKCE), Admin-API-Keys und Zustand des Bootstrap-Tokens. Portabel für SQLite und Postgres.
-- Rollenzuordnungen stehen weiter in app_user.roles (direkt) bzw. kommen aus IdP-Gruppen
-- (Konfiguration admin.auth.oidc.group_roles, Gruppen je Session gespeichert).

-- Sessions: gespeichert wird nur SHA-256 (hex) der zufälligen Session-ID aus dem Cookie.
CREATE TABLE admin_session (
    id_hash      TEXT PRIMARY KEY,
    user_id      TEXT NOT NULL REFERENCES app_user (id) ON DELETE CASCADE,
    groups       TEXT NOT NULL DEFAULT '[]',   -- IdP-Gruppen beim Login (JSON-Liste)
    created_at   BIGINT NOT NULL,
    last_seen_at BIGINT NOT NULL,
    expires_at   BIGINT NOT NULL
);
CREATE INDEX admin_session_user ON admin_session (user_id);
CREATE INDEX admin_session_expires ON admin_session (expires_at);

-- Laufende Logins (höchstens wenige Minuten): Schlüssel = SHA-256 (hex) des state-Werts.
CREATE TABLE admin_login_state (
    state_hash    TEXT PRIMARY KEY,
    nonce         TEXT NOT NULL,
    code_verifier TEXT NOT NULL,
    return_to     TEXT NOT NULL,
    created_at    BIGINT NOT NULL
);
CREATE INDEX admin_login_state_created ON admin_login_state (created_at);

-- Admin-API-Keys (sk-aigw-admin-<Präfix>-<Geheimnis>), getrennt von den Keys der Datenebene.
CREATE TABLE admin_api_key (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL UNIQUE,
    prefix       TEXT NOT NULL UNIQUE,
    key_hash     TEXT NOT NULL UNIQUE,         -- SHA-256 (hex) des vollständigen Keys
    description  TEXT,
    roles        TEXT NOT NULL DEFAULT '[]',   -- JSON-Liste von RoleBinding
    expires_at   BIGINT,
    created_by   TEXT NOT NULL,
    created_at   BIGINT NOT NULL,
    updated_at   BIGINT NOT NULL,
    last_used_at BIGINT
);

-- Schlüssel/Wert-Zustand der Anmeldung (z. B. bootstrap_disabled = Zeitpunkt + Grund).
CREATE TABLE admin_auth_state (
    name       TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at BIGINT NOT NULL
);
