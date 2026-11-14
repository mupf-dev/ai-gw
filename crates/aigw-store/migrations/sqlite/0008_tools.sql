-- SPDX-FileCopyrightText: 2026 ai-gw contributors
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- AIGW-87 Tool-Inventar und Tool-Richtlinien (SQLite; Gegenstück in
-- migrations/postgres/0008_tools.sql):
--   * tool_policy: Ressource `ToolPolicy` (Snapshot, GitOps, Audit je Objekt).
--   * tool_inventory: von Anwendungen mitgeschickte Tools je (Organisation, Team, Key, Name),
--     gepuffert und aggregiert geschrieben; Hashes von Beschreibung und Schema, Inhalte nur bei
--     passendem payloadMode.
--   * tool_inventory_history: Verlauf der Hashes (neu/geändert).
CREATE TABLE tool_policy (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    labels      TEXT NOT NULL DEFAULT '{}',
    level       TEXT NOT NULL,
    target_id   TEXT,
    action      TEXT NOT NULL,
    tools       TEXT NOT NULL DEFAULT '[]',
    servers     TEXT NOT NULL DEFAULT '[]',
    unapproved  TEXT,
    locked      INTEGER NOT NULL DEFAULT 0,
    enabled     INTEGER NOT NULL DEFAULT 1,
    description TEXT,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
CREATE INDEX tool_policy_target ON tool_policy (level, target_id);

CREATE TABLE tool_inventory (
    id               TEXT PRIMARY KEY,
    scope_key        TEXT NOT NULL,
    name             TEXT NOT NULL,
    organization_id  TEXT,
    team_id          TEXT,
    api_key_id       TEXT,
    kind             TEXT NOT NULL,
    origin           TEXT NOT NULL,
    client           TEXT,
    first_seen       INTEGER NOT NULL,
    last_seen        INTEGER NOT NULL,
    requests         INTEGER NOT NULL DEFAULT 0,
    calls            INTEGER NOT NULL DEFAULT 0,
    blocked          INTEGER NOT NULL DEFAULT 0,
    description_hash TEXT NOT NULL,
    schema_hash      TEXT NOT NULL,
    description      TEXT,
    parameters       TEXT,
    changed          INTEGER NOT NULL DEFAULT 0,
    changed_at       INTEGER,
    suspicious       TEXT,
    UNIQUE (scope_key, name)
);
CREATE INDEX tool_inventory_name ON tool_inventory (name, id);
CREATE INDEX tool_inventory_team ON tool_inventory (team_id);
CREATE INDEX tool_inventory_org ON tool_inventory (organization_id);

CREATE TABLE tool_inventory_history (
    id               TEXT PRIMARY KEY,
    tool_id          TEXT NOT NULL REFERENCES tool_inventory (id) ON DELETE CASCADE,
    seen_at          INTEGER NOT NULL,
    event            TEXT NOT NULL,
    description_hash TEXT NOT NULL,
    schema_hash      TEXT NOT NULL,
    description      TEXT,
    parameters       TEXT,
    client           TEXT
);
CREATE INDEX tool_inventory_history_tool ON tool_inventory_history (tool_id, seen_at);
