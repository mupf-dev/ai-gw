-- SPDX-FileCopyrightText: 2026 ai-gw contributors
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- AIGW-9: Grundschema. Läuft unverändert auf SQLite und Postgres:
--   * nur TEXT und BIGINT (keine UUID-/JSONB-/TIMESTAMPTZ-/BOOLEAN-Typen),
--   * IDs: UUIDv7 als Text, Zeitpunkte: Unix-Millisekunden (UTC),
--   * Geldbeträge: Nano-Einheiten der Abrechnungswährung (BIGINT),
--   * Wahrheitswerte: BIGINT 0/1, strukturierte Felder: JSON als TEXT,
--   * Binärdaten (Chiffrat): Hex als TEXT.
-- Verweise zwischen Ressourcen laufen über IDs (Fremdschlüssel); nach außen über Namen.

-- Monoton steigende Konfigurationsversion (genau eine Zeile).
CREATE TABLE config_version (
    id         BIGINT PRIMARY KEY CHECK (id = 1),
    version    BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);
INSERT INTO config_version (id, version, updated_at) VALUES (1, 0, 0);

-- Jede Admin-Änderung: wer, was, vorher/nachher (JSON der Ressource ohne Geheimnisse).
CREATE TABLE audit_log (
    id             TEXT PRIMARY KEY,
    at             BIGINT NOT NULL,
    actor          TEXT NOT NULL,
    action         TEXT NOT NULL,          -- create | update | delete | rotate
    kind           TEXT NOT NULL,
    object_id      TEXT,
    object_name    TEXT,
    config_version BIGINT NOT NULL,
    before_json    TEXT,
    after_json     TEXT
);
CREATE INDEX audit_log_at ON audit_log (at);
CREATE INDEX audit_log_object ON audit_log (kind, object_id);

-- Mandanten -----------------------------------------------------------------------------------

CREATE TABLE organization (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL UNIQUE,
    labels       TEXT NOT NULL DEFAULT '{}',
    display_name TEXT,
    limits       TEXT NOT NULL DEFAULT '[]',
    budget       TEXT,
    metadata     TEXT NOT NULL DEFAULT '{}',
    created_at   BIGINT NOT NULL,
    updated_at   BIGINT NOT NULL
);

CREATE TABLE team (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL UNIQUE,
    labels          TEXT NOT NULL DEFAULT '{}',
    organization_id TEXT NOT NULL REFERENCES organization (id),
    display_name    TEXT,
    allowed_models  TEXT NOT NULL DEFAULT '[]',
    limits          TEXT NOT NULL DEFAULT '[]',
    budget          TEXT,
    metadata        TEXT NOT NULL DEFAULT '{}',
    created_at      BIGINT NOT NULL,
    updated_at      BIGINT NOT NULL
);
CREATE INDEX team_organization ON team (organization_id);

-- "user" ist in Postgres reserviert.
CREATE TABLE app_user (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL UNIQUE,
    labels       TEXT NOT NULL DEFAULT '{}',
    oidc_issuer  TEXT,
    oidc_subject TEXT,
    email        TEXT,
    display_name TEXT,
    roles        TEXT NOT NULL DEFAULT '[]',
    created_at   BIGINT NOT NULL,
    updated_at   BIGINT NOT NULL,
    UNIQUE (oidc_issuer, oidc_subject)
);

CREATE TABLE api_key (
    id             TEXT PRIMARY KEY,
    name           TEXT NOT NULL UNIQUE,
    labels         TEXT NOT NULL DEFAULT '{}',
    prefix         TEXT NOT NULL UNIQUE,   -- 12 Zeichen, sichtbar
    key_hash       TEXT NOT NULL UNIQUE,   -- SHA-256 (hex) des vollständigen Keys
    state          TEXT NOT NULL,          -- active | revoked
    team_id        TEXT REFERENCES team (id),
    user_id        TEXT REFERENCES app_user (id),
    expires_at     BIGINT,
    allowed_models TEXT NOT NULL DEFAULT '[]',
    limits         TEXT NOT NULL DEFAULT '[]',
    budget         TEXT,
    metadata       TEXT NOT NULL DEFAULT '{}',
    last_used_at   BIGINT,
    created_at     BIGINT NOT NULL,
    updated_at     BIGINT NOT NULL
);
CREATE INDEX api_key_team ON api_key (team_id);
CREATE INDEX api_key_user ON api_key (user_id);

-- Anbieter, Modelle, Routing ------------------------------------------------------------------

-- Werte AES-256-GCM-verschlüsselt; key_id = ID des Master-Keys (Rotation).
CREATE TABLE provider_credential (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    labels     TEXT NOT NULL DEFAULT '{}',
    provider   TEXT NOT NULL,
    fields     TEXT NOT NULL DEFAULT '[]', -- nur Feldnamen
    key_id     TEXT NOT NULL,
    nonce      TEXT NOT NULL,              -- 12 Byte, hex
    ciphertext TEXT NOT NULL,              -- Chiffrat + Tag, hex
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);

CREATE TABLE model_group (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    labels     TEXT NOT NULL DEFAULT '{}',
    aliases    TEXT NOT NULL DEFAULT '[]',
    routing    TEXT NOT NULL,
    fallbacks  TEXT NOT NULL DEFAULT '[]',
    limits     TEXT NOT NULL DEFAULT '[]',
    settings   TEXT NOT NULL DEFAULT '{}',
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);

-- Name und Aliasse aller Modellgruppen in einem Namensraum (Eindeutigkeit über Gruppen hinweg).
CREATE TABLE model_alias (
    alias          TEXT PRIMARY KEY,
    model_group_id TEXT NOT NULL REFERENCES model_group (id) ON DELETE CASCADE
);
CREATE INDEX model_alias_group ON model_alias (model_group_id);

CREATE TABLE deployment (
    id             TEXT PRIMARY KEY,
    name           TEXT NOT NULL UNIQUE,
    labels         TEXT NOT NULL DEFAULT '{}',
    model_group_id TEXT NOT NULL REFERENCES model_group (id),
    provider       TEXT NOT NULL,
    endpoint       TEXT,
    provider_model TEXT NOT NULL,
    credential_id  TEXT REFERENCES provider_credential (id),
    weight         BIGINT NOT NULL DEFAULT 1,
    priority       BIGINT NOT NULL DEFAULT 0,
    rpm            BIGINT,
    tpm            BIGINT,
    enabled        BIGINT NOT NULL DEFAULT 1,
    settings       TEXT NOT NULL DEFAULT '{}',
    created_at     BIGINT NOT NULL,
    updated_at     BIGINT NOT NULL
);
CREATE INDEX deployment_model_group ON deployment (model_group_id);
CREATE INDEX deployment_credential ON deployment (credential_id);

-- Versionierte Preise; Beträge als Dezimaltext je 1 Mio. Tokens in token_prices (JSON).
CREATE TABLE price (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL UNIQUE,
    labels        TEXT NOT NULL DEFAULT '{}',
    provider      TEXT NOT NULL DEFAULT '',  -- '' = alle Anbieter
    model         TEXT NOT NULL,
    valid_from    BIGINT NOT NULL,
    token_prices  TEXT NOT NULL DEFAULT '{}',
    per_request   BIGINT,
    per_image     BIGINT,
    created_at    BIGINT NOT NULL,
    updated_at    BIGINT NOT NULL,
    UNIQUE (provider, model, valid_from)
);

-- Plugins und MCP -----------------------------------------------------------------------------

-- target_id verweist je nach level auf organization/team/api_key/model_group (polymorph,
-- daher ohne Fremdschlüssel; das Löschen des Ziels löscht Bindungen im Repository mit).
CREATE TABLE plugin_binding (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    labels     TEXT NOT NULL DEFAULT '{}',
    level      TEXT NOT NULL,
    target_id  TEXT,
    plugin     TEXT NOT NULL,
    position   BIGINT NOT NULL DEFAULT 0,
    enabled    BIGINT NOT NULL DEFAULT 1,
    config     TEXT NOT NULL DEFAULT '{}',
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);
CREATE INDEX plugin_binding_target ON plugin_binding (level, target_id);

CREATE TABLE mcp_server (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL UNIQUE,
    labels        TEXT NOT NULL DEFAULT '{}',
    url           TEXT NOT NULL,
    transport     TEXT NOT NULL,
    credential_id TEXT REFERENCES provider_credential (id),
    enabled       BIGINT NOT NULL DEFAULT 1,
    settings      TEXT NOT NULL DEFAULT '{}',
    created_at    BIGINT NOT NULL,
    updated_at    BIGINT NOT NULL
);

CREATE TABLE mcp_grant (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL UNIQUE,
    labels        TEXT NOT NULL DEFAULT '{}',
    mcp_server_id TEXT NOT NULL REFERENCES mcp_server (id) ON DELETE CASCADE,
    subject_level TEXT NOT NULL,            -- team | api-key
    subject_id    TEXT NOT NULL,
    tools         TEXT NOT NULL DEFAULT '[]',
    created_at    BIGINT NOT NULL,
    updated_at    BIGINT NOT NULL
);
CREATE INDEX mcp_grant_subject ON mcp_grant (subject_level, subject_id);

-- Laufzeitdaten -------------------------------------------------------------------------------

-- Nur für counters.backend = postgres-sync: Deltas je Instanz und Fenster.
CREATE TABLE counter_state (
    counter_key  TEXT NOT NULL,
    window_start BIGINT NOT NULL,
    instance_id  TEXT NOT NULL,
    value        BIGINT NOT NULL,
    updated_at   BIGINT NOT NULL,
    PRIMARY KEY (counter_key, window_start, instance_id)
);
CREATE INDEX counter_state_window ON counter_state (window_start);

-- Verbrauch je Budget-Zeitraum (Ebene + Ziel).
CREATE TABLE budget_usage (
    level        TEXT NOT NULL,             -- organization | team | api-key
    target_id    TEXT NOT NULL,
    period       TEXT NOT NULL,             -- day | week | month | total
    period_start BIGINT NOT NULL,
    amount       BIGINT NOT NULL,           -- Nano-Einheiten
    updated_at   BIGINT NOT NULL,
    PRIMARY KEY (level, target_id, period, period_start)
);

-- Rohlog je Anfrage (gebündelt geschrieben, befristet).
CREATE TABLE request_log (
    id               TEXT PRIMARY KEY,      -- UUIDv7 = x-aigw-request-id
    at               BIGINT NOT NULL,
    api_key_id       TEXT,
    team_id          TEXT,
    organization_id  TEXT,
    endpoint         TEXT NOT NULL,         -- z. B. chat.completions, messages
    model_requested  TEXT,
    model_group      TEXT,
    deployment_id    TEXT,
    provider         TEXT,
    provider_model   TEXT,
    status           BIGINT NOT NULL,
    error_code       TEXT,
    input_tokens     BIGINT NOT NULL DEFAULT 0,
    output_tokens    BIGINT NOT NULL DEFAULT 0,
    cached_tokens    BIGINT NOT NULL DEFAULT 0,
    reasoning_tokens BIGINT NOT NULL DEFAULT 0,
    usage_estimated  BIGINT NOT NULL DEFAULT 0,
    cost             BIGINT NOT NULL DEFAULT 0, -- Nano-Einheiten
    latency_ms       BIGINT NOT NULL DEFAULT 0,
    ttft_ms          BIGINT,
    cache            TEXT,                  -- hit-exact | hit-semantic | miss
    stream           BIGINT NOT NULL DEFAULT 0,
    client_ip        TEXT,
    metadata         TEXT NOT NULL DEFAULT '{}',
    payload          TEXT                   -- nur wenn konfiguriert, maskiert
);
CREATE INDEX request_log_at ON request_log (at);
CREATE INDEX request_log_key_at ON request_log (api_key_id, at);
CREATE INDEX request_log_team_at ON request_log (team_id, at);

-- Minuten-Rollups für Dashboards ('' statt NULL, da Teil des Primärschlüssels).
CREATE TABLE usage_rollup (
    bucket_start     BIGINT NOT NULL,       -- Minutenbeginn (Unix-ms)
    api_key_id       TEXT NOT NULL DEFAULT '',
    team_id          TEXT NOT NULL DEFAULT '',
    organization_id  TEXT NOT NULL DEFAULT '',
    model_group      TEXT NOT NULL DEFAULT '',
    deployment_id    TEXT NOT NULL DEFAULT '',
    requests         BIGINT NOT NULL DEFAULT 0,
    errors           BIGINT NOT NULL DEFAULT 0,
    cache_hits       BIGINT NOT NULL DEFAULT 0,
    input_tokens     BIGINT NOT NULL DEFAULT 0,
    output_tokens    BIGINT NOT NULL DEFAULT 0,
    cached_tokens    BIGINT NOT NULL DEFAULT 0,
    reasoning_tokens BIGINT NOT NULL DEFAULT 0,
    cost             BIGINT NOT NULL DEFAULT 0,
    latency_ms_sum   BIGINT NOT NULL DEFAULT 0,
    latency_hist     TEXT NOT NULL DEFAULT '[]', -- feste Buckets, JSON-Array
    PRIMARY KEY (bucket_start, api_key_id, team_id, organization_id, model_group, deployment_id)
);
CREATE INDEX usage_rollup_team ON usage_rollup (team_id, bucket_start);
CREATE INDEX usage_rollup_model ON usage_rollup (model_group, bucket_start);
