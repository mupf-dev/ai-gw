-- SPDX-FileCopyrightText: 2026 mupf-dev
-- SPDX-License-Identifier: FSL-1.1-ALv2
--
-- AIGW-87 Tool-Inventar und Tool-Richtlinien (Postgres; Gegenstück in
-- migrations/sqlite/0008_tools.sql):
--   * tool_policy: Ressource `ToolPolicy` (Snapshot, GitOps, Audit je Objekt).
--   * tool_inventory: von Anwendungen mitgeschickte Tools je (Organisation, Team, Key, Name),
--     gepuffert und aggregiert geschrieben; Hashes von Beschreibung und Schema, Inhalte nur bei
--     passendem payloadMode.
--   * tool_inventory_history: Verlauf der Hashes (neu/geändert).
CREATE TABLE tool_policy (
    id          UUID PRIMARY KEY,
    name        TEXT COLLATE "C" NOT NULL UNIQUE,
    labels      JSONB NOT NULL DEFAULT '{}',
    level       TEXT NOT NULL,
    target_id   UUID,
    action      TEXT NOT NULL,
    tools       JSONB NOT NULL DEFAULT '[]',
    servers     JSONB NOT NULL DEFAULT '[]',
    unapproved  TEXT,
    locked      BOOLEAN NOT NULL DEFAULT FALSE,
    enabled     BOOLEAN NOT NULL DEFAULT TRUE,
    description TEXT,
    created_at  TIMESTAMPTZ NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL
);
CREATE INDEX tool_policy_target ON tool_policy (level, target_id);

CREATE TABLE tool_inventory (
    id               UUID PRIMARY KEY,
    scope_key        TEXT COLLATE "C" NOT NULL,
    name             TEXT COLLATE "C" NOT NULL,
    organization_id  UUID,
    team_id          UUID,
    api_key_id       UUID,
    kind             TEXT NOT NULL,
    origin           TEXT NOT NULL,
    client           TEXT,
    first_seen       TIMESTAMPTZ NOT NULL,
    last_seen        TIMESTAMPTZ NOT NULL,
    requests         BIGINT NOT NULL DEFAULT 0,
    calls            BIGINT NOT NULL DEFAULT 0,
    blocked          BIGINT NOT NULL DEFAULT 0,
    description_hash TEXT NOT NULL,
    schema_hash      TEXT NOT NULL,
    description      TEXT,
    parameters       TEXT,
    changed          BOOLEAN NOT NULL DEFAULT FALSE,
    changed_at       TIMESTAMPTZ,
    suspicious       JSONB,
    UNIQUE (scope_key, name)
);
CREATE INDEX tool_inventory_name ON tool_inventory (name, id);
CREATE INDEX tool_inventory_team ON tool_inventory (team_id);
CREATE INDEX tool_inventory_org ON tool_inventory (organization_id);

CREATE TABLE tool_inventory_history (
    id               UUID PRIMARY KEY,
    tool_id          UUID NOT NULL REFERENCES tool_inventory (id) ON DELETE CASCADE,
    seen_at          TIMESTAMPTZ NOT NULL,
    event            TEXT NOT NULL,
    description_hash TEXT NOT NULL,
    schema_hash      TEXT NOT NULL,
    description      TEXT,
    parameters       TEXT,
    client           TEXT
);
CREATE INDEX tool_inventory_history_tool ON tool_inventory_history (tool_id, seen_at);
